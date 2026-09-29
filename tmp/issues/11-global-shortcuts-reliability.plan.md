
# Plan: Issue 11, global shortcuts and focus reliability

Worktree setup (see tmp/issues/README.md): `git reset --hard riir`, then run tools with `nix develop path:. -c ...`.
Phase-2 siblings: 09 owns `useCompletion.ts`/`useChatCompletion.ts`, 10 owns providers, 08/18 own `speaker/`. This plan does not edit any of those files. It keeps the `useGlobalShortcuts()` return shape (`registerInputRef`, `registerAudioCallback`, `registerScreenshotCallback`, `registerSystemAudioCallback`), so 09 is unaffected.

## What is actually broken (traced)

1. **Duplicate renderer listeners (the real bug).** `useGlobalShortcuts` is mounted by `useCompletion` (useCompletion.ts:66) and by `useSystemAudio` (useSystemAudio.ts:118). React StrictMode (main.tsx) runs each effect twice. So the effect at useGlobalShortcuts.ts:112-256 runs 4 times. Every run checks `globalEventListeners` synchronously before its first `await listen()`, so all 4 runs see empty slots and register. There is no cleanup, and 3 of the 4 listener sets leak.
   - `start-audio-recording` toggles recording 4 times, which is a net no-op.
   - `toggle-system-audio` fires 4 times.
   - The 300 ms screenshot debounce (lines 16-17, 181-190) only hides this bug. global-hotkey already filters autorepeat (`state.pressed` in x11/mod.rs), so the debounce has no other purpose.
   - `useShortcuts.ts` is a third mount site with zero callers.
2. **Stale closure in the system-audio shortcut.** In useSystemAudio.ts:953-961 the callback reads `capturing`, but the deps are `[startCapture, stopCapture]`, and neither changes when `capturing` changes. The closure always sees `false`, so ctrl+shift+m can start a capture but never stop it.
3. **Handlers swallow errors or panic.**
   - `handle_focus_input` ignores every error with `let _ =`.
   - `handle_toggle_window` calls `.unwrap()` on emit (shortcuts.rs:215). It runs on the global-hotkey X11 thread (plugin `set_event_handler`), so a panic kills every shortcut for the rest of the session.
   - `handle_audio_shortcut` does `Err(_e) => return`.
   - A missing main window is a silent no-op everywhere. The main window is reachable-closed via alt+F4 while the dashboard keeps the app alive, so this is a real state, not a broken invariant.
4. **Dead code on the shortcut path.**
   - Custom actions cannot be created: `addCustomShortcutAction` has no caller and no custom callback is ever registered. The Rust `custom_action =>` arm emits to nobody.
   - `shortcut-registration-error` goes Rust emit, then JS listener, then DOM CustomEvent, then `console.warn` in useApp.ts:73-113. The result is invisible.
   - `check_shortcuts_registered` and `get_registered_shortcuts` have no TS caller.
   - The `toggle_dashboard` command (window.rs:92) has no TS caller, and `handle_toggle_dashboard` duplicates it.
   - `setup_global_shortcuts` only locks a mutex and prints.
   - The JS package `@tauri-apps/plugin-global-shortcut` and its capability permissions are unused.
5. **Every shortcut press re-parses strings.** `RegisteredShortcuts` stores key strings. On each press, lib.rs:180-197 re-parses every string, and `unregister_all_shortcuts` skips unparsable entries without any report. Mutex poisoning is "recovered" in 6 places, which contradicts ARCHITECTURE's rule that poisoning panics.
6. **Corrupt storage falls back to defaults.** `getShortcutsConfig` catches corrupt JSON and silently returns defaults.

## Design

### Rust: `src-tauri/src/shortcuts.rs`
Each registration owns its action, using the plugin's `on_shortcut`. This deletes the global handler, the lookup map and the re-parsing.

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Direction { Up, Down, Left, Right }            // + const ALL, impl Display ("up"...)

#[derive(Clone, Copy, Debug)]
enum Action { ToggleDashboard, ToggleWindow, FocusInput, AudioRecording, Screenshot, SystemAudio, Move(Direction) }

impl FromStr for Action { type Err = String; ... } // ids from src/config/shortcuts.ts except "move_window"; unknown -> Err("unknown shortcut action '{id}'")

#[derive(Default)]
pub struct MoveWindowState { tasks: Mutex<HashMap<Direction, tauri::async_runtime::JoinHandle<()>>> }

fn bindings(config: &ShortcutsConfig) -> Result<Vec<(Shortcut, Action)>, String>
```

**`bindings`**
- Validate the action id of every binding, including disabled ones.
- Parse the key only if `enabled && !key.is_empty()`.
- `move_window` expands to 4 entries: `format!("{}+{dir}", key.trim())` becomes `Action::Move(dir)`.
- The config is all-or-nothing. Any error returns before anything is touched.

**`update_shortcuts`** (keeps its signature)
- `bindings(&config)`. On `Err`, call `tracing::error!` and return the error.
- `stop_all_move_windows`.
- `app.global_shortcut().unregister_all()?`. This uses the plugin's own list and replaces `unregister_all_shortcuts`.
- For each binding, call `gs.on_shortcut(shortcut, move |app, _, ev| on_event(app, action, ev.state))`.
- Collect failures (for example an X11 BadAccess because another client holds the key). Keep today's behaviour: register what works, then log with `tracing::error!` and return `Err("Some shortcuts could not be registered: ...")`.
- The `shortcut-registration-error` emit is deleted.

**`on_event`** (private)
- `(Move(d), Pressed)` calls `start_move_window`.
- `(Move(d), Released)` calls `stop_move_window`.
- `(a, Pressed)` calls `run(app, a)`.
- `Released` does nothing for other actions.
- On `Err(e)`: `tracing::error!("shortcut {action:?} failed: {e:#}")`, with this tail comment: `// runs on the hotkey thread: nothing to return to, a panic kills all shortcuts`.

**`run(app, action) -> anyhow::Result<()>`** (private; anyhow is already a dependency)
- Main window lookup: `app.get_webview_window("main").ok_or(tauri::Error::WindowNotFound)?`.
- `ToggleDashboard` calls `crate::window::toggle_dashboard(app).map_err(anyhow::Error::msg)`.
- `FocusInput`: if not visible, `show()?`. Then `set_focus()?` and `emit("focus-text-input", ())?`.
- `AudioRecording` and `SystemAudio`: if hidden, `show()?` and `set_focus()?`, then `emit(...)?`.
- `Screenshot`: `emit("trigger-screenshot", ())?`.
- `ToggleWindow`: the existing body with `eprintln!` and `.unwrap()` replaced by `?`.
  - In the `cfg(windows)` block, change only what the new signature requires (`return;` becomes `return Ok(());`, `eprintln` + continue becomes `?`).
  - Leave the `cfg(macos)` lines byte-identical. They cannot be compiled here.
- `Move(_)` is `unreachable!("routed in on_event")`. Alternatively, match Move in `on_event` only and give `run` a non-Move parameter. Pick whichever keeps the match smaller.

**Move-window loop**
- `start_move_window(app, dir)`: if `tasks` contains `dir`, return. Otherwise insert the result of `tauri::async_runtime::spawn(loop { if let Err(e) = move_once(&app, dir) { tracing::error!(..); break } sleep(16ms).await })`.
- `stop_move_window` calls `remove(dir)` then `.abort()`.
- `stop_all_move_windows` drains the map and aborts each task.
- `move_once` is the old `handle_move_window` returning `tauri::Result<()>`. The match on `Direction` is exhaustive, so the invalid-direction arm goes away.
- The `Arc<AtomicBool>`, the `MoveWindowTask` alias and the manual `Default` impls are deleted.

**Also delete from shortcuts.rs:** `RegisteredShortcuts`, `setup_global_shortcuts`, `handle_shortcut_action`, `handle_toggle_dashboard`, `get_registered_shortcuts`, `check_shortcuts_registered`, `unregister_all_shortcuts`, all `poisoned.into_inner()` (use `.lock().unwrap()`), and the `serde_json::json` import if it becomes unused.

**Leave these alone:** `WindowVisibility` (issue 17 owns it), `validate_shortcut_key`, `set_app_icon_visibility`, `set_always_on_top`, `exit_app`.

### Rust: `src-tauri/src/lib.rs`
- Remove `.manage(shortcuts::RegisteredShortcuts::default())`.
- Replace the whole `.with_handler(...)` closure (lines 173-223) with `tauri_plugin_global_shortcut::Builder::new().build()`.
- Delete the `setup_global_shortcuts` call.
- Remove `shortcuts::check_shortcuts_registered`, `shortcuts::get_registered_shortcuts` and `window::toggle_dashboard` from `generate_handler!`.
- Add one Linux diagnostic next to the plugin init, gated by `#[cfg(target_os = "linux")]`: if `WAYLAND_DISPLAY` is set, emit `tracing::warn!("global shortcuts use X11 key grabs; under Wayland they fire only while an XWayland window has focus")`. This is the dominant "presses dropped" cause on sway/Hyprland (see Broken assumptions). A proper fix is out of scope.

### Rust: `src-tauri/src/window.rs`
- `toggle_dashboard` stops being a command: drop `#[tauri::command]` and make it `pub fn toggle_dashboard<R: Runtime>(app: &AppHandle<R>) -> Result<(), String>`. Body unchanged.

### Capabilities
- `src-tauri/capabilities/{cross-platform,default}.json`: delete the 3 `global-shortcut:allow-*` lines. The renderer never uses the JS plugin, and removing them stops the renderer from grabbing keys directly.
- `package.json`: remove `@tauri-apps/plugin-global-shortcut`.

### Frontend: `src/hooks/useGlobalShortcuts.ts` (rewrite, about 40 lines)
- Module state: `inputEl`, `onAudio`, `onScreenshot`, `onSystemAudio` (nullable), plus 4 module-level setter functions.
- `export const useGlobalShortcuts = () => registry;` where `registry` is a module const holding the 4 register fns. Their identities are stable, so useCompletion's dep arrays keep working. No refs, no `useCallback`, and no `checkShortcutsRegistered`/`getShortcuts`/`updateShortcuts`/custom-callback functions.
- `export const useGlobalShortcutListeners = () => useEffect(() => { const pending = [listen("focus-text-input", () => setTimeout(() => inputEl!.focus(), 100)), listen("start-audio-recording", () => onAudio!()), listen("trigger-screenshot", () => void onScreenshot!()), listen("toggle-system-audio", () => onSystemAudio!())]; return () => pending.forEach(p => p.then(un => un())); }, []);`
  - The `!` is deliberate fail-fast. In the main window, child and earlier-hook effects register the callbacks before this effect runs, because `Completion` is always mounted and only hidden with CSS. A null slot is therefore a bug and should throw.
  - Doc comment: "Mount once per webview, from the main-window root hook."
- Deleted: the `globalEventListeners` singleton, the screenshot debounce, the custom-shortcut listener and the registration-error listener.

### Frontend: `src/hooks/useApp.ts`
- Call `useGlobalShortcutListeners()` once.
- In the init effect, replace `console.error` with `invoke("js_log", { msg: \`shortcut init failed: ${error}\` })`. The webview console is invisible, and corrupt storage now throws (see below).
- Delete the `shortcutRegistrationError` effect (lines 73-113).

### Frontend: other files
- **Delete** `src/hooks/useShortcuts.ts` and its line in `src/hooks/index.ts`.
- **`src/hooks/useSystemAudio.ts:961`**: add `capturing` to the deps (one token). Issue 12 is phase 3 and rewrites this file later; no phase-2 sibling touches it.
- **`src/lib/storage/shortcuts.storage.ts`**:
  - `getShortcutsConfig`: drop the try/catch. A missing key returns defaults (first run, which is legitimate). Corrupt JSON throws.
  - `setShortcutsConfig`: drop the try/catch.
  - Do NOT switch to `safeLocalStorage`; it swallows every error.
  - Delete `getAllShortcutActions` (its `_hasLicense` parameter breaks the naming rule), `addCustomShortcutAction`, `removeCustomShortcutAction`, and `customActions` in `getDefaultShortcutsConfig`.
- **`src/types/shortcuts.ts`**: remove `customActions?`.
- **`src/pages/shortcuts/components/shortcuts/ShortcutManager.tsx`**: replace `actions` state and `getAllShortcutActions(true)` with `DEFAULT_SHORTCUT_ACTIONS` from `@/config`.
- **`src/hooks/useWindow.ts`**: delete `useWindowFocus` and `UseWindowFocusOptions` (lines 85-130). Nothing uses them, and none of the focus hypotheses below needs a renderer-side focus listener. Update tmp/issues/17-dead-code.md to strike that bullet ("done in 11").
- **`src/contexts/app.context.tsx`**: no change (see Broken assumptions).

### `ARCHITECTURE.md`
Add a short `## Global shortcuts` section:
- The renderer's localStorage is the config source. The main-window root hook `useApp` sends it through `update_shortcuts` on mount.
- Rust validates the whole config, then each plugin registration owns its `Action`.
- Handlers run on the global-hotkey thread and log errors instead of panicking.
- Renderer listeners are mounted exactly once, by `useGlobalShortcutListeners` in `useApp`.
- The move-window loop is a sanctioned spawn. Its handle is owned by `MoveWindowState` and aborted on release or re-registration.
- Linux backend is X11-only (Wayland caveat).

## Tests

### 1. Frontend, red first (`src/hooks/useGlobalShortcuts.test.tsx`)
Setup:
- `npm i -D vitest jsdom` and add `"test": "vitest run"` to package.json.
- Put `// @vitest-environment jsdom` at the top of the test file. vitest reuses vite.config.ts, so the `@/` alias works without config changes.
- No `@testing-library`: use `createRoot` + `act` from React 19, and set `globalThis.IS_REACT_ACT_ENVIRONMENT = true`.
- Tauri boundary: `mockIPC(() => {}, { shouldMockEvents: true })` and `emit` from `@tauri-apps/api/event` (@tauri-apps/api 2.8.0 supports this). Call `clearMocks()` after each test.

Data-driven table of `[event, register]` pairs:
- `start-audio-recording` with `registerAudioCallback`
- `trigger-screenshot` with `registerScreenshotCallback`
- `toggle-system-audio` with `registerSystemAudioCallback`
- `focus-text-input` with `registerInputRef(input)`, observed by spying on `input.focus` with fake timers

Each case:
1. Render `<StrictMode><MainWindow/></StrictMode>`. `MainWindow` mirrors production wiring: two `useGlobalShortcuts()` consumers, one of which registers a counting callback in an effect.
2. Flush with `await act(async () => {})`.
3. `await emit(event)`; expect exactly 1 call.
4. Unmount, `await emit(event)` again; expect still 1 call.

Red: write the harness against today's code, with MainWindow = `useGlobalShortcuts(); useGlobalShortcuts();`. Observe audio/system-audio/focus giving 4 calls and unmount leaking. The screenshot case passes only because of the debounce; note that. Green: add `useGlobalShortcutListeners()` to MainWindow (the single wiring change) after the refactor. Record the red run in the PR.

### 2. Rust (`#[cfg(test)] mod tests` in shortcuts.rs)
Use the `mock_builder` pattern from capture.rs:273, with `.manage(MoveWindowState::default())` only and no global-shortcut plugin (it needs an X server).

Data-driven cases calling `update_shortcuts(app.handle().clone(), config)`:
- unknown enabled id `custom_x`
- unknown disabled id
- unparsable key for `screenshot`
- unparsable `move_window` modifier

Each case expects `Err` whose message names the offending id. Red today: the unknown id goes past parsing into `app.global_shortcut()` and panics. Valid configs cannot be tested hermetically; say so in the PR.

### 3. Not auto-tested (justified)
- The `capturing` dep fix: a harness would need the entire AppProvider plus IPC.
- Handler error logging: MockRuntime cannot make `show()` fail.

Both are covered by the manual checklist below.

## Manual verification and focus investigation
Run: `RUST_LOG=info nix develop path:. -c npm run tauri dev`. Do this on a Wayland compositor (sway/Hyprland) and on X11 (or `GDK_BACKEND=x11`).

Checklist:
1. Each default shortcut fires exactly once. Check the terminal log and the behaviour.
2. ctrl+shift+m starts and then stops capture.
3. Bind a key another app holds: `update_shortcuts` returns an Err, which shows in ShortcutManager and in the log.
4. Put `{` into localStorage `shortcuts`: a `js_log` error appears. Recover with Reset in ShortcutManager.
5. alt+F4 the main window, then press ctrl+shift+i: a WindowNotFound error is logged and there is no panic.

Time-boxed "stuck typing" investigation. Record the results in the PR:
- **H1:** When a popover is open, `useWindowResize` expands the transparent main window to 600 px. Clicks on another app's text field *under* that transparent area land on Pluely and give it keyboard focus. Test: click fields under vs. outside Pluely's 600x600 rect.
- **H2:** A leaked `capture-overlay-*` window (transparent, fullscreen, always-on-top). Check `swaymsg -t get_tree` / `hyprctl clients` after a screenshot.
- **H3:** Duplicate `focus-text-input` handlers. These are already gone after this fix; `element.focus()` does not activate the toplevel.

If the cause is H1 or H2, it lives in window.rs, useWindow.ts or capture.rs, outside this issue's shortcut scope. File `tmp/issues/NN-*.md` instead of fixing it here.

## Commands
- `nix develop path:. -c cargo test --manifest-path src-tauri/Cargo.toml shortcuts`
- `nix develop path:. -c cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings`
- `nix develop path:. -c npm test`
- `nix develop path:. -c npm run build` (runs tsc over the new test file too)
- `grep -rn "RegisteredShortcuts\|custom-shortcut\|shortcut-registration-error\|useWindowFocus\|useShortcuts\b\|check_shortcuts_registered\|get_registered_shortcuts" src src-tauri/src` must be empty.

## Out of scope; file as new issues
- A Wayland global-shortcuts backend: the XDG GlobalShortcuts portal via `ashpd`, supported by Hyprland and KDE. global-hotkey 0.7 is X11-only.
- Moving shortcut config into the SQLite `settings` table so Rust registers at startup. See trade-offs.

## Review amendments (orchestrator)
- vitest and jsdom are approved. Issue 09 adds the same pair this phase. Put the config in `vite.config.ts` under `test: { environment: "jsdom" }` and add the script `"test": "vitest run"`. The orchestrator resolves conflicts at merge.
- Rejecting a config with unknown action ids is approved, but `shortcut-registration-error` must become visible in the UI (not a console.warn).
- The Wayland/X11-only finding is tracked as issue 19 (phase 3, portal backend). Keep only the diagnostic warn here.
