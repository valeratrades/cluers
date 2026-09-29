
# Plan: Issue 05, screenshot capture hygiene (`src-tauri/src/capture.rs`)

Scope: only `src-tauri/src/capture.rs`, one line in `src-tauri/src/lib.rs` if needed, and a `[dev-dependencies]` entry in `src-tauri/Cargo.toml`. No TS changes: `Overlay.tsx` belongs to 06 and the hooks belong to 09. The only IPC change is that `capture-closed` is now also emitted on error paths. That matches what the TS already expects.

## What is actually wrong (verified)

1. `close_overlay_window` (`:192-194`) calls `main_window.emit("capture-closed", ()).unwrap()`, which panics if emit fails. In Tauri 2, `Emitter::emit` already broadcasts to every target, so the `get_webview_window("main")` lookup does nothing useful. It also causes silent loss: when there is no main window, nothing is emitted.
2. `.ok()` / `let _ =` swallow errors at `:66` (stale cleanup), `:91`, `:147`, `:148`, `:151`, `:153-154`, `:165`, `:182` and `:251`. If the overlay fails to show, the command still returns `Ok(())`. The TS never gets `capture-closed` or an error, so `isScreenshotLoading` stays on.
3. `thread::sleep(100ms)` runs inside an async command at `:145` (once per monitor) and again at `:159`. The xcap `Monitor::all()` / `capture_image()` calls at `:44` and `:73` also block the worker. On X11 they take tens to hundreds of ms per monitor. The issue does not list them, but they are the same bug class.
4. **Not in the issue:** at `:209` the code uses `ok_or({ state.overlay_active.store(false, ..); .. })`. This is evaluated eagerly, so it clears `overlay_active` on every call, including successful ones.
5. **Not in the issue:** error paths in `capture_selected_area` (zero-size selection, missing monitor index, PNG encode failure) return `Err` but leave the overlays on screen. The chosen monitor's image has already been removed, so a retry on that monitor fails with "No captured image found" and the user is stuck until ESC. `Overlay.tsx` only logs the error, which 06 fixes.
6. **Not in the issue:** there is a fallback on corrupted state at `:55-61` and `:110-118`. When the monitor counts disagree it logs with `eprintln!` and then uses the raw physical xcap dims as logical coordinates. `capture_to_base64` (`:266-307`) has the same pattern: `.ok().flatten()` chains and a made-up `(0,0,0,0,0,0)` rect.
7. `overlay_active` is only read at `:64`. If stale overlays are destroyed unconditionally at the start of each capture, the flag has no remaining use.

## Design

Take things away first: drop `overlay_active` and `MonitorInfo`, drop the `Arc` wrapping inside the managed state, drop `request_user_attention`, and merge the three copies of the destroy-overlays loop into one private fn.

### State
```rust
#[derive(Default)]
pub struct CaptureState {
    images: Mutex<Vec<RgbaImage>>, // index = monitor index sent to overlay-{idx}
}
```
- Tauri already wraps managed state in an `Arc`, so the inner `Arc` goes.
- The field is private. `lib.rs` keeps using `CaptureState::default()`, so `lib.rs` does not change.
- `SelectionCoords`: drop the `Serialize` derive (it is only ever deserialized) and make the fields private.

### Private helper (the only new fn, not pub)
```rust
fn destroy_overlays<R: Runtime>(app: &AppHandle<R>) -> Result<(), String>
```
It loops over `app.webview_windows()`, calls `window.destroy()` on every window whose label starts with `capture-overlay-`, and applies `.map_err(...)?`. It replaces the loops at `:89-93`, `:180-184` and `:248-253`.

### `close_overlay_window<R: Runtime>(app: AppHandle<R>) -> Result<(), String>` (still sync)
The body becomes:
1. `destroy_overlays(&app)?`
2. `state.images.lock().unwrap().clear()` (a poisoned lock panics, which is the fail-fast convention in ARCHITECTURE)
3. `app.emit("capture-closed", ()).map_err(..)?`

This removes the `main` lookup and the `unwrap`.

### `start_screen_capture<R: Runtime>(app: AppHandle<R>) -> Result<(), String>`
1. Call `destroy_overlays(&app)?` unconditionally. This replaces the `overlay_active` check and the `let _ = close_overlay_window` at `:63-68`, plus the loop at `:89-93`. It does not emit `capture-closed` here, because that would reset the TS flags in the middle of the flow.
2. Get the layout with `app.available_monitors()?`.
3. Run the xcap work in `tokio::task::spawn_blocking(|| { Monitor::all() ...; for each: (capture_image()?, is_primary) }).await.expect("capture spawn_blocking join")`. This follows the `Db::with_conn` convention.
   - Only `Vec<(RgbaImage, bool)>` comes back. xcap `Monitor` (it holds xcb buffers) never lives across an `.await`, so no `Send` question arises.
   - The closure returns `Result<_, String>`. An empty list is an `Err`.
4. If `layout.len() != captures.len()`, return `Err(format!("monitor count mismatch: xcap {} vs tauri {}", ..))`. This deletes the `eprintln!` and the xcap-dims fallback branch.
5. Store the images with `*state.images.lock().unwrap() = images`, dropping the guard in the same statement.
6. Move the window work into a private `async fn open_overlays<R: Runtime>(app: &AppHandle<R>, layout: &[tauri::Monitor], primary: Option<usize>) -> Result<(), String>` and call it as follows:
   ```rust
   if let Err(e) = open_overlays(&app, &layout, primary).await {
       close_overlay_window(app)?; // teardown error wins only if teardown itself fails
       return Err(e);
   }
   ```
   `close_overlay_window` destroys any partial overlays, clears the images and emits `capture-closed`. The TS listener then resets `isScreenshotLoading`, and the rejected `invoke` shows the error.

   Inside `open_overlays`, zip `layout.iter().enumerate()` and:
   - build the window as it is built today, using `?`
   - `tokio::time::sleep(Duration::from_millis(100)).await` (replaces `thread::sleep`)
   - `overlay.show()?`
   - `overlay.set_always_on_top(true)?` (kept: some X11 WMs ignore keep-above before map)
   - for the primary monitor: `set_focus()?`
   - remove `request_user_attention`: it sets an urgency hint on a `skip_taskbar` window that is already focused, so it does nothing on Linux

   After the loop: `tokio::time::sleep(100ms).await`, then `app.get_webview_window(&primary_label).ok_or(..)?.set_focus()?`. If the just-built overlay is missing at that point, the state is corrupted, so it is an `Err`, not a skip.
7. `thread` and `std::time` imports go away. Use `tokio::time`.

### `capture_selected_area<R: Runtime>(app: AppHandle<R>, coords: SelectionCoords, monitor_index: usize) -> Result<String, String>`
```rust
let image = std::mem::take(&mut *state.images.lock().unwrap()).into_iter().nth(monitor_index);
let encoded = match image {
    None => Err(format!("No captured image for monitor {monitor_index}")),
    Some(img) => tokio::task::spawn_blocking(move || crop_to_png_base64(img, coords)).await.expect("crop spawn_blocking join"),
};
match encoded {
    Err(e) => { close_overlay_window(app)?; Err(e) }
    Ok(b64) => {
        destroy_overlays(&app)?;
        app.emit("captured-selection", &b64).map_err(..)?;
        Ok(b64)
    }
}
```
- `crop_to_png_base64` is a private sync fn. It holds today's `:214-242` body: the zero-size check, clamping, crop and PNG + base64 encode.
- The success path must **not** emit `capture-closed`. The TS `capture-closed` handler clears `screenshotInitiatedByThisContext`, and the `captured-selection` listener would then drop the screenshot. Emission order today is load-bearing, and it stays the same.
- This removes the eager `ok_or` bug and the `overlay_active` stores.

### `capture_to_base64` (same file, same fallback class)
- Geometry becomes `let pos = window.outer_position().map_err(..)?; let size = window.outer_size().map_err(..)?;`.
- Delete `monitor_fallback`, the `_ =>` branch and the `(0,0,0,0,0,0)` rect, about 30 lines.
- Keep the nearest-centre choice when there is no overlap. An off-screen window is a legitimate geometric case, not corrupted state.
- Replace the `find_map` with `monitors.into_iter().nth(target_idx).expect("index from enumerate")`.
- Leave the command itself non-generic. It is not under test.

### Generic commands
Making the three commands generic over `R: Runtime` is what allows the mock runtime to drive them. `tauri::generate_handler!` in `lib.rs:93-96` accepts generic commands unchanged, because `R` is inferred from the builder. If inference fails, write `capture::close_overlay_window::<tauri::Wry>` there.

## Test (write first; it fails on today's code)

1. Add to `src-tauri/Cargo.toml`:
   ```toml
   [dev-dependencies]
   tauri = { version = "2", features = ["test"] }
   ```
2. Add `#[cfg(test)] mod tests` at the bottom of `capture.rs`. This follows the existing in-module pattern of `db/queries.rs` and `llm/provider.rs`, and is needed to seed the private `images`.
   - Build the app with `tauri::test::mock_builder().manage(CaptureState::default()).build(tauri::test::mock_context(tauri::test::noop_assets()))`.
   - Record `capture-closed` and `captured-selection` payloads with `app.listen_any(...)` into an `Arc<Mutex<Vec<(String, String)>>>`.
   - Drive everything only through the command fns, which are the IPC interface.
3. Data-driven `#[tokio::test]` table. Each row is `(seeded images: Vec<(w,h)>, monitor_index, coords) -> (Ok(dims) | Err, expected events, images empty afterwards)`:

| case | today | after |
|---|---|---|
| index out of range | Err, no event, other images remain | Err, `capture-closed` once, images empty |
| zero-width selection | Err, no event, other monitors' images remain | Err, `capture-closed` once, images empty |
| valid 10x10 on a 20x20 image | Ok, `captured-selection` | Ok, decoded PNG is 10x10, only `captured-selection`, images empty |
| selection overflowing bounds | Ok, clamped | same; the clamp is pinned by the test |

4. A separate case: `close_overlay_window` on an app with no `main` window returns Ok and emits `capture-closed` once. Today it emits nothing, so this fails.

Some fixes cannot be tested headlessly: `show()` / `set_focus()` failures, the sleeps and xcap capture. MockRuntime cannot inject window-system failures, and xcap needs a display. These are verified by review and by a manual run.

## Verification
```
nix develop -c bash -c 'cd src-tauri && cargo test capture::'
nix develop -c bash -c 'cd src-tauri && cargo test'
nix develop -c bash -c 'cd src-tauri && cargo build'
nix develop -c bash -c 'cd src-tauri && cargo clippy --all-targets'   # only if 07 has landed; see note below
rg -n '\.ok\(\)|let _ =|thread::sleep|unwrap\(\)' src-tauri/src/capture.rs   # expect only lock().unwrap() and the join expects
```
Manual run (`nix develop -c pnpm tauri dev`, or the repo's usual dev command):
- Selection mode on X11 with one monitor, and with two monitors if available.
- ESC must clear the loading spinner.
- A drag must attach the screenshot.
- Kill the compositor or WM focus mid-flow if practical.

## Sequencing / conflicts
- **Phase 1 conflict risk with 07:** 07 may fix the clippy failure by bumping `xcap`. In newer xcap, `Monitor::width()`/`x()`/`is_primary()`/`capture_image()` return `XCapResult`, which would break this file. Merge 07 first and rebase 05 on it. If 07 bumps xcap, the `?` conversions happen inside the spawn_blocking closure. Both issues may also edit `Cargo.toml`: 05 only adds a `[dev-dependencies]` section, so the conflict is trivial.
- 09 (phase 2) should be told two things: `start_screen_capture` failures now also emit `capture-closed`, and that `capture-closed` must never be emitted on the success path.
- 06 (same phase) owns `Overlay.tsx`'s swallowed errors. The Rust side now tears down on error, so 06 only needs to show the error message.
