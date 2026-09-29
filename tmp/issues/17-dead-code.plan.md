## Issue 17: dead code sweep (phase 5)

### Scope and what I checked on `riir` @ 0db5978

Phase 5 is shared with 14+15 (they own `speaker/**`, `useSystemAudio.ts`, `SettingsPanel.tsx`, `lib/functions/stt.function.ts` and the audio TS) and 16 (`MessageHistory.tsx`, `completion/Input.tsx`, `speech/ResultsSection.tsx`). This plan stays out of those files. The one exception is a single line in `speaker/linux.rs`, which the issue lists by name. See step 7.

Current state of each item in the issue:
- `GetLicense.tsx` and `Promote.tsx` have no importers. `GetLicense` calls `get_checkout_url`, and no Rust command by that name exists. Delete both.
- `activate.rs` holds one comment line and has no `mod` declaration. Delete it.
- `center_window_completely` is unused. Delete it.
- `WindowVisibility.is_hidden` is **not** dead code. `shortcuts.rs:221-233` reads it under `#[cfg(target_os = "windows")]`, so it is only dead on Linux. Fix: gate the type and its `.manage` with a cfg. Do not delete it.
- `cpal` was already removed by issue 13. Nothing to do.
- `useHistory.selectedConversationId` was already removed. Nothing to do.
- All four npm deps still have importers: `moment` (3 files), `@bany/curl-to-json` (3), `react-error-boundary` (3), `recharts` (`Usage.tsx`, `ui/chart.tsx`). The issue says to keep them in that case.
- The two clippy `type_complexity` warnings are still there: `capture.rs:304` and `speaker/linux.rs:48`.
- `Overlay.tsx` problems confirmed:
  - The Cancel button has both `onClick` and `onMouseDown` calling `handleCancel`, so one click closes twice.
  - `selectionRef` is attached to two divs and never read.
  - `devicePixelRatio || 1` is a dead fallback.
  - There are also three redundant Escape listeners (window, document and body, all in capture phase). The window one fires first and calls `stopImmediatePropagation`, so the other two never run.

A fresh sweep found more dead code:
- **Tauri commands the renderer never calls:**
  - `window::move_window`: the shortcut move loop is in `shortcuts.rs`.
  - `start_conversation`, `append_message`, `rename_conversation`: the TS wrappers `startConversation`, `appendMessage` and `renameConversation` in `src/lib/database/index.ts` have no callers. `append_turn` is the only write path, and it uses the query functions internally.
  - `speaker::get_vad_config` is also unused, but it is in 14+15's files. Leave it and report it to them.
- **Cargo:** `cargo machete` reports `dotenv` (in `[dependencies]`; it stays in `[build-dependencies]`) and `once_cell`. `ringbuf` is used only in `speaker/macos.rs`, so move it to the macOS target deps.
- **build.rs:** `PAYMENT_ENDPOINT` was only used by the old activate module. It also appears in `.github/workflows/publish.yml`.
- **TS files with no importers:**
  - `src/pages/system-prompts/Create.tsx` (`CreateSystemPrompt`)
  - `src/pages/settings/components/DeleteChats.tsx` (not in the settings barrel)
- **TS exports with no importers:**
  - `CONVERSATION_SAVE_DEBOUNCE_MS`
  - `getPromptTemplateNames`
  - `isWindows`
  - These types, which are never referenced even in their own file: `CustomProvider`, `ModelSelectionProps`, `SelectedSpeechProvider`, `SettingsState`, `SpeechProviderFormData` (all `types/settings.ts`), `UseCompletionHook`, `UseHistoryType`
  - Several more are exported but only used in their own file. Those lose `export`.
- **Rust `pub` items used only in their own module:** `window.rs` `position_window_top_center` and `show_dashboard_window`; `api.rs` response structs; `llm/provider.rs` `ParsedCurl`, `extract_variables`, `build_messages`; `llm/pluely.rs` `ApiResponseConfig`, `ApiConfigError`, `UserAudioConfig`, `map_api_error_message`; `llm/stream.rs` `StreamOutcome`.

### Steps

**1. Overlay bug. The failing test comes first.**

New file `src/components/Overlay.test.tsx`. Use the same harness style as `src/hooks/useGlobalShortcuts.test.tsx`: `createRoot`, `act`, `mockIPC`, `IS_REACT_ACT_ENVIRONMENT`.
- `mockIPC((cmd) => { if (cmd === "close_overlay_window") closes++; })`.
- Render `<Overlay monitorIndex={0} />` into a container appended to `document.body`.
- The test is data-driven: `test.each` over `[name, gesture]`, and every case expects `closes === 1`. Cases:
  - `"cancel button click"`: dispatch the real browser sequence on the Cancel button: `mousedown`, then `mouseup`, then `click`, all `bubbles: true`. This fails today with 2.
  - `"escape key"`: `document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }))`. This passes today and guards step 2.
  - `"tiny drag cancels"`: `mousedown` then `mouseup` on the backdrop 3px apart.
- Wrap each dispatch in `await act(async () => ...)`.

Run `nix develop -c npm test` and confirm the first case fails with 2 calls.

**2. Fix `src/components/Overlay.tsx`.**
- Remove `onClick={handleCancel}` from the Cancel button. Keep the `onMouseDown` handler: it must stop propagation, otherwise the backdrop starts a selection.
- Delete `selectionRef`, both `ref={selectionRef}` and the `useRef` import.
- Change `window.devicePixelRatio || 1` to `window.devicePixelRatio`.
- Keep only `window.addEventListener("keydown", handleEscapeKey, true)` and its matching remove. Drop the document and body listeners and the `|| e.keyCode === 27` check.

The test should now pass.

**3. Delete files.**
- `src/components/GetLicense.tsx`
- `src/components/Promote.tsx`
- `src-tauri/src/activate.rs`
- `src/pages/system-prompts/Create.tsx`
- `src/pages/settings/components/DeleteChats.tsx`

**4. Dead DB commands (Rust and TS together, no shim).**
- `src-tauri/src/db/commands.rs`: delete `start_conversation`, `append_message` and `rename_conversation`, then trim the `use super::schema::{...}` list.
- `src-tauri/src/lib.rs`: remove the three entries from `generate_handler!`.
- `src-tauri/src/db/queries.rs`:
  - `start_conversation`: make it private (`fn`) and change the return type to `Result<String, DbError>` (the id). `append_turn` only uses `.id`.
  - `append_message`: make it private.
  - Delete `rename_conversation` and the `rename_unknown_errors` test.
  - Update existing tests from `cid.id` to `cid`.
- `src-tauri/src/db/schema.rs`: delete `ConversationId`. It is no longer IPC.
- Move `NewMessage` out of `schema.rs` into `queries.rs` as a private struct with no serde derives. `schema.rs` is for IPC types only, and `NewMessage` no longer crosses IPC.
- `src/lib/database/index.ts`: delete `startConversation`, `appendMessage` and `renameConversation`, then the types that become unused (`ConversationIdResponse`, `NewMessage`). Drop `export` from `AppendedMessageResponse`, `AppendedTurn` and `NewTurn` if `tsc` shows they are only used locally.
- `ARCHITECTURE.md` "Command surface": replace the sentence "the frontend `start_conversation`s once and `append_message`s per turn" with: chat writes go only through `append_turn`, which creates the conversation when the id is null. There is no rename.

**5. Window and shortcuts.**
- `src-tauri/src/window.rs`:
  - Delete `center_window_completely` and its `#[allow(dead_code)]`.
  - Delete the `move_window` command and remove `window::move_window` from `lib.rs`.
  - Make `position_window_top_center` and `show_dashboard_window` private.
  - In `setup_main_window`, delete the `.or_else("pluely")` / first-window fallback chain. Use `app.get_webview_window("main").ok_or("main window missing")?`, which is fail-fast because the window is declared in the config.
  - Delete the commented-out Windows block.
- `src-tauri/src/shortcuts.rs`: put `#[cfg(target_os = "windows")]` on `pub struct WindowVisibility` and drop the `#[allow(dead_code)]`.
- `src-tauri/src/lib.rs`: pull `.manage(shortcuts::WindowVisibility{..})` out of the chain into `#[cfg(target_os = "windows")] let builder = builder.manage(...);`. Delete the "Learn more about Tauri commands" boilerplate comment.
- Existing macOS code in `lib.rs` already assigns to a non-`mut` `builder`, so macOS does not compile today. This plan does not make that worse. Report it; do not fix it.

**6. Cargo and build.**
- `src-tauri/Cargo.toml`: remove `dotenv` and `once_cell` from `[dependencies]`. Move `ringbuf` to `[target.'cfg(target_os = "macos")'.dependencies]`.
- `src-tauri/build.rs`: delete the `PAYMENT_ENDPOINT` block, and add a tail comment `// .env is optional` on `dotenv::dotenv().ok();` to justify the swallowed error.
- `.github/workflows/publish.yml`: delete the two `PAYMENT_ENDPOINT` lines (54 and 68).

**7. Clippy `type_complexity`.**
- `capture.rs:304`: delete the explicit `Vec<(...)>` annotation on `cases`. It can be written `let cases = [ ... ];` and the types are inferred from `app(&images)`, `capture_selected_area(.., idx)` and `assert_eq!(got, expected)`. If inference fails, pin one literal (for example `Ok::<(u32, u32), ()>((10, 10))`) rather than adding a type alias.
- `speaker/linux.rs:48`: replace the annotated `Rc<RefCell<Vec<(Option<String>, Option<String>)>>> = Rc::default()` with `let entries = Rc::new(RefCell::new(Vec::new()));`. The element type is inferred from the `push`. This one line is in 14+15's file. If 14+15 lands first and has reshaped `list_devices`, make the same change at the new location. It is a 1-line conflict at most.

**8. `pub` / `export` reduction (compiler-driven, not by hand).**
- **Rust**, only in `window.rs`, `api.rs`, `capture.rs`, `shortcuts.rs`, `llm/*.rs` and `db/*.rs` (not `speaker/`):
  - For each item in the list above, drop `pub` (or narrow `pub(crate)` to private) and run `cargo clippy`.
  - Revert only where the compiler complains, for example a `private_interfaces` error because the type appears in a `#[tauri::command]` signature.
  - Do not touch `lib.rs` `pub use speaker::{turn, vad}` or `pub fn run`; the integration tests and `main.rs` need them.
- **TS**, excluding 14+15 and 16 files, including `stt.function.ts`, which 15 may import from:
  - Delete `CONVERSATION_SAVE_DEBOUNCE_MS` and its doc block in `src/lib/chat-constants.ts`.
  - Delete `getPromptTemplateNames` in `src/lib/platform-instructions.ts`.
  - Delete `isWindows` in `src/lib/platform.ts`.
  - Delete the never-referenced types (`CustomProvider`, `ModelSelectionProps`, `SelectedSpeechProvider`, `SettingsState`, `SpeechProviderFormData`, `UseCompletionHook`, `UseHistoryType`).
  - Drop `export` from symbols used only in their own file: `ANALYTICS_EVENTS`, `captureEvent`, `DEFAULT_RESPONSE_SETTINGS`, `setResponseSettings`, `ResponseSettings`, `getDefaultShortcutsConfig`, `setShortcutsConfig`, `CurlValidationResult`, `HistoryMessage`/`ProviderInput` (lib/llm), `LanguageOption`, `ResponseLengthOption`, `PromptTemplate`, `UseHistoryReturn`, and the remaining `types/settings.ts` types.
  - Then run `tsc`. `noUnusedLocals` is on, so any symbol that is now unused shows up as an error. Delete those and repeat until clean.
- Re-run the export sweep command (below) and repeat until only barrel-consumed or cross-file symbols remain.

### Tests

The only behaviour change is the Overlay fix, covered by the step 1 test. Everything else is removal, so the existing suites are the check:
- Rust: the `db::queries` tests, including `append_turn_cases`, still cover the now-private `append_message`/`start_conversation` path.
- TS: `tsc` (with `noUnusedLocals`) plus vitest.

### Verification (from repo root)
- `nix develop -c npm test`: the Overlay test passes. It must have failed before step 2.
- `nix develop -c npm run build`: runs `tsc` with `noUnusedLocals` plus the vite build.
- `nix develop -c bash -c 'cd src-tauri && cargo clippy --all-targets -- -D warnings'`: must be clean. This is the gate the issue asks for.
- `nix develop -c bash -c 'cd src-tauri && cargo test'`. The `speaker::linux` live-Pulse timing tests are known to flake (15 owns them).
- `nix develop -c bash -c 'cd src-tauri && cargo machete'`: must print no unused deps.
- `grep -rn 'allow(dead_code)' src-tauri/src` must be empty.
- `grep -rn 'get_checkout_url\|PAYMENT_ENDPOINT\|move_window"' src src-tauri/src .github`: only `shortcuts.ts`/`ShortcutManager` action ids may remain.
- Export sweep: for each `export (const|function|type|interface|class)` name, list names with only one file referencing them. Anything left must be justified.
- Manual check: open the screenshot overlay, click Cancel once; it closes once. Press Escape; it closes.
## Review amendments (orchestrator)
- Leave `get_vad_config` alone; 14+15 removes it. Also leave `speaker/commands.rs` alone entirely this phase.
- After your pub-reduction pass, `cargo clippy --all-targets -- -D warnings` must be clean (that includes the two type_complexity sites).
