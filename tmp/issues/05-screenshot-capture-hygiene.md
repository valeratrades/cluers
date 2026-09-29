# 05 capture.rs (screenshot overlay) hygiene

`src-tauri/src/capture.rs`:
- `:193` `main_window.emit("capture-closed", ()).unwrap()` — panic if window torn down.
- `:66,147,148,151,152,165,182` chain of `.ok()` on overlay show/focus/destroy — failed overlay show is invisible; screenshot silently never appears, and the TS `isScreenshotLoading` flag sticks (see 09).
- `:145,159` `thread::sleep(100ms)` inside `async fn start_screen_capture` blocks a tokio worker.

Acceptance: errors propagate to the command result or an explicit error event; no blocking sleeps in async; any remaining ignored error has a one-line justification.
