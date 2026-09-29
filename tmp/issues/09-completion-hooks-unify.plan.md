## Plan for issue 09: unify useCompletion / useChatCompletion and fix their shared bugs

Scope: `src/hooks/useCompletion.ts`, `src/hooks/useChatCompletion.ts`, one new shared hook file, `src/lib/llm/index.ts`, `src/lib/database/index.ts`, the Rust db layer (`schema.rs`, `queries.rs`, `commands.rs`), one line in `lib.rs`, `useHistory.ts`, `types/completion.hook.ts`, and the minimal vitest wiring. `useSystemAudio.ts` (issue 12), `useGlobalShortcuts.ts` and `app.context.tsx` (issue 11) and the provider storage (issue 10) are not touched.

### What the code actually does (verified on `riir` @ a857592)
1. **Listener leak (#218).** The `captured-selection` effect stores `unlisten` only after `await listen(...)`. If cleanup runs before `listen` resolves, the listener is never removed. React StrictMode is on (`main.tsx`), so this happens on **every mount** in dev. In prod it happens whenever the deps change within one IPC roundtrip. `isProcessingScreenshotRef` plus the 100 ms timeout collapses the duplicates into one call. But the call that wins is the **oldest leaked listener**, and its closure is stale: `conversationHistory` is empty and the `persistTurn` it holds has `currentConversationId=null`. So each selection screenshot in auto mode creates a new conversation with no history.
2. **Stale closure in chat.** `useChatCompletion.submit` rebuilds `setMessages(...)` from the `messages` snapshot it took at call start (`:198-202, 266-283, 325-329`).
3. **Not in the issue:** in chat auto mode, `handleScreenshotSubmit` runs `setTimeout(() => submit(prompt))` with the `submit` from the current closure. Its `state.attachedFiles` does not contain the screenshot that was just added, so **the screenshot is never sent**.
4. **Stuck spinner.** After issue 05, Rust emits `capture-closed` or `captured-selection` on every overlay path. The only stuck path left is `start_screen_capture` rejecting: the `finally` resets loading only when `config.enabled` is set.
5. **Orphan writes.** `persistTurn` makes two IPC calls (or three, counting `startConversation`). Chat persists the user message *before* streaming, so a failed or cancelled stream also leaves an orphan user message.
6. **Not in the issue:** `startNewConversation` and `applyConversation` do not cancel the in-flight stream. A running turn is then persisted into the old conversation, and `setState` makes that old conversation current again.
7. `generateConversationTitle` is just `trim()`, and `start_conversation` already trims.
8. `cancelChat(...).catch(() => {})` appears 7 times across both hooks. Issue 02 says: do not catch.

### Step 1: atomic `append_turn` (Rust, test first)
`src-tauri/src/db/schema.rs`:
```rust
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewTurn { pub user: String, pub attached_files: Vec<AttachedFile>, pub assistant: String }

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppendedTurn { pub conversation_id: String, pub user: AppendedMessage, pub assistant: AppendedMessage }
```
`src-tauri/src/db/queries.rs`:
```rust
pub fn append_turn(conn: &mut Connection, conversation_id: Option<&str>, turn: NewTurn) -> Result<AppendedTurn, DbError>
```
- If either `user` or `assistant` is empty after trim, return `InvalidInput("turn message is empty")`. This replaces the TS `console.error` guard.
- Open `let tx = conn.transaction()?;` (same pattern as `delete_all_conversations`).
- When the id is `None`, use `start_conversation(&tx, &turn.user)?.id`. The title is the trimmed user text, the same as `generateConversationTitle`.
- Then call `append_message(&tx, …, Role::User, attached_files: (!files.is_empty()).then_some(files))`, then `append_message(&tx, …, Role::Assistant, None)`, then `tx.commit()?`.

`commands.rs`: add `append_turn(db, conversation_id: Option<String>, turn: NewTurn) -> Result<AppendedTurn, DbError>` as a `with_conn` shim. Register it in `src-tauri/src/lib.rs` after `append_message`. This is a one-line merge point with issue 11.

**Test (write it first).** Add `append_turn_cases` in the `queries.rs` tests module. It is data-driven over `(name, target, sabotage: bool, turn, expect_ok: bool, expect_dump: &str)`.
- `dump(conn)` renders every conversation as `title[role:content(+files), …]`, using `list_conversation_summaries` plus `load_conversation`.
- `sabotage` installs `CREATE TEMP TRIGGER s BEFORE INSERT ON messages WHEN NEW.role='assistant' BEGIN SELECT RAISE(ABORT,'s'); END;`.
- Cases:
  - new: ok
  - existing: ok
  - unknown id: Err, db unchanged
  - new + sabotage: Err, dump `""`
  - existing `x` + sabotage: Err, dump `x[]`
  - empty assistant: Err
- Sequence: first implement `append_turn` **without** the transaction and confirm the two sabotage cases fail (they leave `hi[user:hi]` and `x[user:hi]`). Then add the transaction.

`src/lib/database/index.ts`: add the `NewTurn` and `AppendedTurn` interfaces and `appendTurn(conversationId: string | null, turn: NewTurn): Promise<AppendedTurn>`. Keep `startConversation` and `appendMessage`, because `useSystemAudio` still uses them. Removing them belongs to 12/17.

`ARCHITECTURE.md` "Command surface": add one sentence. `append_turn` persists a user+assistant pair in one transaction and starts the conversation when the id is null. It is the only write path for chat turns from the overlay and the chat view.

### Step 2: shared provider builder (`src/lib/llm/index.ts`)
```ts
export async function resolveProviderInput(
  selected: { provider: string; variables: Record<string, string> },
  providers: TYPE_PROVIDER[]
): Promise<ProviderInput>
```
- If `await shouldUsePluelyAPI()` is true, return the pluely literal.
- If `!selected.provider`, `throw new Error("Please select an AI provider in settings")`.
- Look up the provider. If it is missing, `throw new Error("Invalid provider selected")`.
- Otherwise return `id: selected.provider` (it is the matched id, so the old `|| ""` goes), plus `curl`, `responseContentPath ?? ""`, `streaming ?? false` and `userVariables` (non-empty values, upper-cased keys).

This is the one new public function; the issue requires it and 12 will consume it.

### Step 3: vitest harness, then the failing leak test
- Add devDeps `vitest` and `jsdom` only. No @testing-library: the test mounts a probe component with `react-dom/client` + `act`.
- Add `"test": "vitest run"` to `package.json`.
- In `vite.config.ts`, add `/// <reference types="vitest/config" />` and `test: { environment: "jsdom" }`.
- Create `src/hooks/useCompletionCore.ts`. It is **not** re-exported from `hooks/index.ts`.
- First move the current screenshot code into it **as it is today**, bug included:
  `export function useScreenshotCapture(config: ScreenshotConfig, onCapture: (shot: AttachedFile, autoPrompt: string | null) => void | Promise<void>, onError: (message: string) => void): { captureScreenshot: () => Promise<void>; isScreenshotLoading: boolean }`
- Create `src/hooks/useCompletionCore.test.ts`:
  - Use `mockIPC(handler, { shouldMockEvents: true })` from `@tauri-apps/api/mocks`, with `emit` from `@tauri-apps/api/event` and `clearMocks` in `afterEach`.
  - Render the hook inside `<StrictMode>`. StrictMode's synchronous mount→cleanup→mount is exactly the "cleanup before listen resolves" race.
  - Table of cases `{ name, callbacks: rerender sequence, startFails, events: ("selection"|"closed")[], expectCalls: string[], expectLoading, expectErrors }`:
    1. "latest callback, once": rerender with A, B, C, then capture, then selection. Expect `["C"]`. The current code yields `["A"]`, so the test fails.
    2. "start fails clears spinner": `start_screen_capture` throws. Expect loading false and 1 error. Today loading is stuck true, so the test fails.
    3. "closed then late selection ignored": capture, closed, selection. Expect `[]` and loading false.
    4. "not initiated ignored": selection only. Expect `[]`.
- Then fix it:
  - Keep `latest = useRef({config, onCapture, onError})`, refreshed in `useLayoutEffect` without deps.
  - Add `awaitingSelection = useRef(false)`, with the tail comment `// captured-selection is broadcast to every window`.
  - One `useEffect(…, [])` registers both `captured-selection` and `capture-closed` through `const p = listen(...)`, and cleanup runs `void p.then(f => f())`.
  - The selection handler: `if (!awaitingSelection.current) return; awaitingSelection.current = false; setLoading(false); await deliver(payload)`.
  - `captureScreenshot` is stable (`[]` deps). Its catch sets loading false, clears `awaitingSelection` and calls `onError(\`Failed to capture screenshot: ${e}\`)`.
  - `deliver` builds the `AttachedFile` once. In auto mode an empty `autoPrompt` calls `onError("Auto screenshot prompt is empty")`; this replaces the silent fallback to manual.
  - Delete `isProcessingScreenshotRef` and its 100 ms timeout. They only existed to paper over the leak.
  - Keep the macOS permission block verbatim, so nothing breaks off Linux.

### Step 4: `useStream` in the same file
```ts
export function useStream(): {
  run: (req: Omit<StreamChatRequest, "requestId">, onDelta: (d: string) => void) => Promise<string | null>;
  cancel: () => void;
}
```
- `run`:
  - Swap `currentRef` to a new id, then `await cancelChat(previous)` if there is one.
  - Iterate `streamChat`. Return `null` as soon as the request is no longer current, including inside the catch: a cancelled request rejects with `Cancelled`.
  - Rethrow only for the current request.
  - On completion clear the ref and return the full text.
- `cancel`: swap the ref to null, then `if (id) void cancelChat(id)`.
- Add `useEffect(() => cancel, [cancel])` for unmount.
- No `.catch(() => {})`, per issue 02.

### Step 5: rewrite `useCompletion.ts`
- Use the `ChatMessage` and `ChatConversation` types from `@/types`; delete the local copies.
- Add a private `runTurn(message, files): Promise<boolean>`:
  - `setState` sets input=message, isLoading, error=null and response="".
  - `await stream.run({ provider: await resolveProviderInput(selectedAIProvider, allAiProviders), message, systemPrompt: buildEnhancedSystemPrompt(systemPrompt || undefined), history, attachedFiles: files }, d => setState(p => ({...p, response: p.response + d})))`.
  - A `null` result returns false.
  - Otherwise `appendTurn(state.currentConversationId, {user: message, attachedFiles: files, assistant: full})`, then one functional `setState`: set `currentConversationId`, append the 2 messages, clear input, set `isLoading: false`. Loading stays true until the persist finishes, which closes the double-new-conversation race for Enter submits.
  - Focus the input and return true.
  - The catch sets `error: String(e)` and `isLoading: false`.
- `submit(speechText?)`: `if (await runTurn(text, attachedFiles)) clearAttachedFiles()`.
- Screenshot `onCapture(shot, prompt)`:
  - If there is a prompt, call `runTurn(prompt, [shot])`. The composer buffer is not touched.
  - Otherwise: if `attachedFiles.length >= MAX_FILES`, set the error; else call `addAttachedScreenshot(shot.base64)`.
  - This deletes the 170-line re-implementation (444-611) and `persistTurn` (306-368).
- `cancel` is `stream.cancel()` plus setting isLoading false. `startNewConversation` and `applyConversation` call `stream.cancel()` first (finding 6).
- Delete the `console.log` debug lines in `handleConversationSelected`.
- Fix `scrollAreaRef.current || scrollAreaRef.current` to `scrollAreaRef.current?.querySelector(...)`, with deps `[isPopoverOpen]`.
- Drop `handleScreenshotSubmit` from the return value and from `UseCompletionReturn` in `src/types/completion.hook.ts`. It has no consumer.

### Step 6: rewrite `useChatCompletion.ts`
- Change the signature to `(conversationId: string, messages: ChatConversation | null, setMessages: Dispatch<SetStateAction<ChatConversation | null>>)`. `View.tsx` already passes a `useState` setter, so no change is needed there.
- Attachments: use the per-window context buffer (`attachedFiles`, `addAttachedScreenshot`, `removeAttachedFile`, `clearAttachedFiles`, `handleAttachedFileSelect`, `handleAttachedPaste`, `isFilesPopoverOpen`). The dashboard is its own window with its own `AppProvider`. This deletes the local `AttachedFile`, `addFile`, `removeFile`, `clearFiles`, `fileToBase64`, `handleFileSelect` and `handlePaste`, about 110 lines.
- `runTurn(message, files)`:
  - `key = crypto.randomUUID()` gives the ids `uId`/`aId`.
  - Add a local `patch(f)` = `setMessages(prev => ({...prev!, messages: f(prev!.messages)}))`. It is always functional (fixes finding 2). A null `prev` is a bug and throws.
  - Append the optimistic user message, clear the input, set isLoading.
  - Deltas upsert `aId` by id.
  - On success, `appendTurn(conversationId, …)` and then `patch` maps `uId`/`aId` to the persisted ids and timestamps, plus sets `updatedAt`.
  - On failure, remove `uId`/`aId`, restore the input, set the error. Superseded turns (unmount) do nothing.
- `submit(speechText?)`: snapshot `attachedFiles`, call `clearAttachedFiles()`, then `runTurn`.
- Screenshots go through the same `onCapture` as the overlay. Auto mode sends `[shot]` directly (fixes finding 3).
- Drop the unused `hasActiveLicense` dep. Return only what `src/pages/chats/components/*` reads (check with `grep -on "completion\.\w*" src/pages/chats`); `setState` and the STT fields go.

### Step 7: `useHistory.ts`
Remove `selectedConversationId` from `UseHistoryReturn`, the state and the return value. It has no reader and no setter.

### Verification
- `nix develop -c cargo test --manifest-path src-tauri/Cargo.toml db::queries` (also run it after the naive step to see the 2 sabotage cases fail)
- `nix develop -c cargo clippy --manifest-path src-tauri/Cargo.toml -- -D warnings`
- `nix develop -c npm install && nix develop -c npm test`. Cases 1-2 fail before the Step 3 fix and all pass after.
- `nix develop -c node_modules/.bin/tsc --noEmit`
- `grep -rn "catch(() => {})\|isPluelyHosted: true\|persistTurn\|isProcessingScreenshotRef" src/hooks/useCompletion.ts src/hooks/useChatCompletion.ts src/hooks/useCompletionCore.ts` returns nothing.
- Manual (`nix develop -c npm run tauri dev`, where StrictMode is active):
  - Overlay, selection mode + auto: 3 captures in a row give one conversation with 3 turns in the dashboard.
  - Chat view, auto mode: the model describes the screenshot.
  - Set a bad curl, then submit: the error shows and no new conversation appears.
  - "New conversation" mid-stream: nothing is persisted.

### Handoffs
- 12: `useSystemAudio` should adopt `resolveProviderInput` and `useStream`, and switch to `append_turn`.
- 17: then delete `start_conversation`, `append_message`, `generateConversationTitle` and their TS wrappers.
## Review amendments (orchestrator)
- vitest and jsdom are approved. Issue 11 adds the same pair this phase. Put the config in `vite.config.ts` under `test: { environment: "jsdom" }` and add the script `"test": "vitest run"`. The orchestrator resolves the package.json/lock conflict at merge.
