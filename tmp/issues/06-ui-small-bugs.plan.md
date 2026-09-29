Plan for issue 06: small UI bugs (Phase 1, frontend only, no Rust changes)

Baseline: `nix develop -c node_modules/.bin/tsc --noEmit` passes today with exit 0. There is no JS test framework in package.json (no vitest, jest or playwright).

## 1. CustomCursor never hides on window blur
**File:** `src/components/CustomCursor.tsx`

**Bug:** `handleWindowBlur` sets `style.display = "0"`. That value is invalid and the browser ignores it. The cursor is otherwise hidden and shown through `opacity` (see `handleMouseLeave` and `handleMouseMove`).

**Fix (remove rather than add):** once the bug is fixed, `handleWindowBlur` would be identical to `handleMouseLeave`. So:
- Delete `handleWindowBlur`.
- Rename `handleMouseLeave` to `hide`.
- Register `hide` for both `document` `mouseleave` and `window` `blur`, and remove both in the effect cleanup.
- Result: blur sets `opacity = "0"`. The next `mousemove` sets it back to 1, which is already how it works.

## 2. Rendering sorts parent state in place, which reorders the history sent to the LLM
**Files:**
- `src/pages/app/components/completion/Input.tsx:174-175`
- `src/pages/app/components/completion/MessageHistory.tsx:86-87` (same prop, same array, same bug; the issue does not list it)

**Why it matters (more than the issue says):** `Array.prototype.sort` mutates `state.conversationHistory` from `useCompletion`, leaving it newest-first. `useCompletion.ts:128` and `:474` then build `messageHistory` for the LLM straight from that array, and new turns are appended at the end (`:353`). After you open the conversation panel (keepEngaged) or the MessageHistory popover, the next request sends the LLM a scrambled message order. The DB is not affected, because Rust assigns timestamps in `append_message`.

**Fix:** at both sites, use `[...conversationHistory].sort((a, b) => b.timestamp - a.timestamp)`.
- Drop the `?.`, because `ChatMessage.timestamp` is non-optional.
- `toSorted` cannot be used: `tsconfig` has `lib: ES2020`, and changing lib is out of scope.
- `ResultsSection.tsx:141` is already safe because it calls `.slice(2)` first. No change there.
- Issue 16 later replaces the bubble markup in these files. That is a different phase, so there is no conflict.

## 3. `any` types on the chat components
**Files:**
- `src/pages/chats/components/ChatFiles.tsx:20`: `attachedFiles: AttachedFile[]`
- `src/pages/chats/components/ChatScreenshot.tsx:7-8`: `screenshotConfiguration: ScreenshotConfig; attachedFiles: AttachedFile[]`
- Import both from `@/types`, the existing convention (e.g. `completion/Input.tsx`).

**Root cause, same bug class:** `src/types/completion.hook.ts` (`UseCompletionReturn`) also declares:
- `:42 attachedFiles: any[]` → `AttachedFile[]`
- `:92 screenshotConfiguration: any` → `ScreenshotConfig`
- `:94 setScreenshotConfiguration: Dispatch<SetStateAction<any>>` → `Dispatch<SetStateAction<ScreenshotConfig>>`

Fix those three lines here (a type-only change). The sources are already typed: `useChatCompletion` state is `AttachedFile[]`, and `context.type.ts:34` is `ScreenshotConfig`. Fix any fallout that `tsc` reports at the call sites. Do not cast to silence it.

Issue 09 (phase 2) rewrites the hooks and should inherit these types.

## 4. Overlay swallows errors
**File:** `src/components/Overlay.tsx`

**Facts from reading `src-tauri/src/capture.rs`** (owned by issue 05 in this phase, so do not touch it):
- When `capture_selected_area` fails, the overlay windows are NOT destroyed. The user stays on a dimmed full-screen overlay with no feedback.
- The monitor's image has already been removed from state, so selecting again cannot succeed.
- ESC calls `close_overlay_window`, which emits `capture-closed`. That resets `isScreenshotLoading` in the main window, so the recovery path already works; it is just not explained to the user.

The overlay is the window the user is looking at, so show errors there.

**Changes:**
- Add `const [error, setError] = useState<string | null>(null);`.
- `handleCancel`:
  - Call `await invoke("close_overlay_window")` without the `{ reason }` payload. The Rust command takes no such argument, so it is dead data.
  - Replace the empty catch with `catch (e) { setError(\`Failed to close overlay: ${e}\`); }`.
- `handleSelectionComplete`:
  - Replace `catch { // Error ignored; console.error(...) }` with `catch (e) { setError(\`Capture failed: ${e}\`); }`.
  - Delete the contradictory comment and the `console.error`.
- Instruction banner (lines ~172-177):
  - When `error` is set, render `<span className="text-red-400 font-semibold">{error}</span>` and "Press ESC to close" in place of the "Click and drag" text.
  - Keep the existing box. Do not add a new component or a toast library.

Out of scope, not touched:
- The Cancel button calls `handleCancel` twice (onMouseDown and onClick).
- The duplicate `ref={selectionRef}`.
- The `devicePixelRatio || 1` fallback.

Mention these in the report for issue 17 or later.

## 5. Providers.tsx `.catch(() => setApiKeyStored(false))`: leave to issue 10
Issue 10 lists the same bullet (`10-providers-unify.md:11`) and rewrites the file in phase 2. A correct fix needs new error state and rendering, and the sibling `submitApiKey` and `clearApiKey` rejections are unhandled too. That is not trivially local, and anything added now would be thrown away. No change.

## 6. PluelyPrompts index key: not a bug, no change
`PluelyPrompts.tsx:242` maps `prompts` directly. The list is not filtered, sorted or reordered; it is replaced as a whole by `setPrompts(response.prompts)`. A `title-index` key is stable for that list. Switching to `key={prompt.title}` would add a duplicate-key risk, because titles come from the server and are not guaranteed unique. Record this as a false assumption in the issue.

## Tests
Rule: add a failing test first where feasible. Here it is not feasible without new infrastructure:
- There is no JS test runner.
- Every affected behaviour is DOM/React or Tauri-IPC glue.

Adding vitest + jsdom + Tauri IPC mocks for 4 small bugs goes against "remove > add". So there are no new tests, and verification is manual plus typecheck. If the owner wants a regression guard for #2 (the LLM history order), the right place is issue 09's unified hook, with a test driving `submit` twice through the hook interface. Flag it to 09; do not build it here.

## Verification
1. `nix develop -c node_modules/.bin/tsc --noEmit` exits 0.
2. `grep -rn "\.sort(" src --include=*.tsx`: every remaining sort is on a copy (`[...x]`, `.slice()`) or a local array.
3. `grep -rn ": any\b\|any\[\]" src/pages/chats src/types/completion.hook.ts` shows no hits for the touched fields.
4. Manual, in `nix develop -c npm run tauri dev`:
   - (a) Move the mouse into the app window, then alt-tab away. The custom cursor fades out.
   - (b) Turn on conversation mode and send 2 messages, then a 3rd. In devtools, `state.conversationHistory` stays in chronological order (or check the outgoing request body order).
   - (c) To force a capture failure, temporarily point `monitorIndex` at a nonexistent index, or select after the first failed attempt. The overlay shows a red "Capture failed: No captured image found..." message. ESC closes it, and the main window's screenshot spinner resets.

## Commit
One commit on the issue worktree: `fix(ui): cursor blur hide, non-mutating history sort, typed chat props, overlay errors surfaced`.