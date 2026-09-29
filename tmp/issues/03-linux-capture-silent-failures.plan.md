
# Plan: 03 Linux system-audio capture fails silently

I read ARCHITECTURE.md, the issue, the README, sibling issues 04, 08, 13 and 15, and all of `speaker/`, plus the listeners in `src/hooks/useSystemAudio.ts`. Line numbers below refer to branch `riir` at commit 6e0010f.

## Findings that change the issue

1. **Sample rate.** The rate is hardcoded, but it is not wrong. `pa_simple` records in the spec we request, and the server resamples to it. So the stream really delivers 44 100 Hz, and `sample_rate()` reports that correctly. The only false value is the `DEFAULT_SAMPLE_RATE` returned on the init-failure path, and that path is deleted below. See the rate trade-off.
2. **Device removal.** Removing the device usually does not produce a read error. The stream is opened without `DONT_MOVE`, so PulseAudio (rescue-streams) and PipeWire-pulse move it to another source. The read errors that persist are server death or restart, or the stream being killed. The test therefore uses `kill_source_output`, which triggers this case the same way every time.
3. **Access check.** `check_system_audio_access` needs no code change. It returns `true` only because `new` cannot fail. Once `new` opens the Pulse stream, the check works as written. It keeps mapping `Err` to `Ok(false)` plus `error!` because the IPC contract is a bool; justify that on one line.
4. **File overlap.** Issue 04 in the same phase rewrites the spawned task in `start_system_audio_capture`, and this issue edits 5 lines inside it. The two touch the same file, so this issue must merge **after** 04 and rebase that one hunk onto 04's lifecycle state.
5. **Frontend listener.** The frontend has no listener for any `capture-*` event, so a frontend change is required to meet "reaches the frontend and resets capture state". `src/hooks/useSystemAudio.ts` has no owner in phase 1; issues 13, 14 and 15 edit it in later phases. The addition is one listener.

## Design

### `SpeakerInput::new` does the Pulse init (`src-tauri/src/speaker/linux.rs`)
`libpulse_simple_binding::Simple` is `Send`: the crate has `unsafe impl Send`. So open it in `new` and move it into the producer thread in `stream()`. The effects:
- An init failure is an `Err` from `new`.
- The `init_tx`/`init_rx` channel, `DEFAULT_SAMPLE_RATE`, the `init_success` teardown block (`:262-287`), the `eprintln!`s and `get_default_monitor_source` are all deleted.
- `stream()` stays infallible, so `mod.rs` and the macOS/Windows backends keep their signatures.

```rust
pub struct SpeakerInput { simple: Simple, sample_rate: u32 }

impl SpeakerInput {
    pub fn new(device_id: Option<String>) -> Result<Self> {
        let source = match device_id {                  // mapping unchanged; the silent-default question belongs to issue 15
            Some(id) if !id.is_empty() && id != "default" => format!("{id}.monitor"),
            _ => "@DEFAULT_MONITOR@".to_owned(),
        };
        let spec = Spec { format: Format::F32le, channels: 1, rate: 44_100 };
        let simple = Simple::new(None, "pluely", Direction::Record, Some(&source),
                                 "System Audio Capture", &spec, None, None)
            .map_err(|e| anyhow!("Failed to open PulseAudio source {source}: {e}"))?;
        Ok(Self { simple, sample_rate: spec.rate })
    }
    pub fn stream(self) -> SpeakerStream { /* spawn producer that owns self.simple */ }
}
```
Drop the `spec.is_valid()` check. The spec is a constant, and `Simple::new` rejects an invalid one anyway.

### Producer and consumer state
Merge the two mutexes (`sample_queue` and `WakerState` with `has_data`) into one:
```rust
struct Shared { queue: VecDeque<f32>, waker: Option<Waker>, stop: bool, error: Option<PAErr> }
pub struct SpeakerStream { shared: Arc<Mutex<Shared>>, producer: Option<JoinHandle<()>>, sample_rate: u32 }
```
- **Producer loop:** `while !shared.lock().stop { read }`.
  - On `Ok`: extend the queue and keep the existing overflow trim and `warn!`, then take the waker and wake it.
  - On `Err(e)`: log with `error!`, set `shared.error = Some(e)`, wake the consumer and `return`. This removes the 100 ms retry loop at `:416-419`.
- **`poll_next`:** if a sample is queued, return `Ready(Some)`. Otherwise, if `error.is_some()`, return `Ready(None)`. Otherwise store the waker and return `Pending`.
- **New method:** `pub fn take_error(&mut self) -> Option<anyhow::Error>`, which returns `self.shared.lock().unwrap().error.take().map(Into::into)`.
- **`Drop`:** set `stop`, then `producer.take().map(|h| h.join().expect("pulse capture thread panicked"))`. This replaces `let _ =`, so a producer panic is no longer hidden.

### Deduplicate device enumeration (`linux.rs:24-219`)
Add one private helper that owns the connect, wait-until-Ready and iterate logic:
```rust
struct Pulse { context: Context, mainloop: Mainloop }   // field order matters: context must drop before mainloop
impl Pulse {
    fn connect() -> Result<Self>;                                     // current :28-50 logic, once
    fn wait<G: ?Sized>(&mut self, op: Operation<G>) -> Result<()>;    // iterate while State::Running; Cancelled → Err; iterate failure → Err (today it silently `break`s)
}
impl Drop for Pulse { fn drop(&mut self) { self.context.disconnect() } }
```
Then write a private `fn list_devices(outputs: bool) -> Result<Vec<AudioDevice>>`, with `get_input_devices` and `get_output_devices` as one-line wrappers around it. It does the following:
- Makes one `get_server_info` call and takes `default_sink_name` or `default_source_name`.
- Makes one list call, either `get_sink_info_list` or `get_source_info_list`. The two operation types differ, so call `wait` inside each match arm. Each callback pushes `(name, description)` into a shared `Rc<RefCell<Vec<_>>>`.
- Makes `ListResult::Error` an `Err`. Today it silently returns a partial list.
- Filters monitor sources with `info.monitor_of_sink.is_none()` instead of `name.contains(".monitor")`. A real source can contain that substring.
- Falls back to `name` when `description` is missing, as today. That is a display label, not corrupted state.
- Builds `AudioDevice` in one place.

The introspection loops go from about 200 lines to about 70.

### `src-tauri/src/speaker/mod.rs`
Add one wrapper so `commands.rs` stays platform-neutral:
```rust
/// Why the stream ended on its own, if the backend knows.
pub fn take_error(&mut self) -> Option<anyhow::Error> {
    #[cfg(target_os = "linux")] return self.inner.take_error();
    #[cfg(not(target_os = "linux"))] None   // macOS/Windows producers don't report failures yet
}
```

### `src-tauri/src/speaker/commands.rs` (minimal hunks)
1. **Spawned task in `start_system_audio_capture` (`:148-164`).** Bind `mut stream` and pass `&mut stream` to `run_vad_capture` and `run_continuous_capture`. `&mut S: Stream + Unpin` holds, so neither signature changes and issue 04's fake-stream test is unaffected. After the run function returns:
   - Do the existing reset first: clear `stream_task` and set `is_capturing = false`.
   - Then run `if let Some(e) = stream.take_error() { error!(…); emit("capture-error", format!("{e:#}")) }`, logging if the emit itself fails.
   - The order matters: reset first, then emit, so a restart triggered by the frontend is not rejected. After rebasing onto 04, apply the same rule to 04's lifecycle state.
2. **`request_system_audio_access`, Linux block (`:657-672`).**
   - Launch `app.shell().command("gnome-control-center").args(["sound"])`, not the single string `"gnome-control-center sound"`.
   - Replace `warn!` + `Ok(())` with `Err("No audio settings app found (tried pavucontrol, gnome-control-center)")`. Both TypeScript callers already `catch`.
3. **`check_system_audio_access`.** No code change; add the one-line justification (finding 3).

Do not touch `stop_system_audio_capture`, `calibrate_vad_thresholds`, `get_audio_sample_rate` or the `run_*` internals (issues 04 and 08).

### `src/hooks/useSystemAudio.ts`
In the existing listener effect (`:257-334`), add `captureErrorUnlisten = await listen<string>("capture-error", …)` and unlisten it in the cleanup. The handler:
- calls `setError(\`System audio capture stopped: ${payload}\`)`;
- resets the same state `stopCapture` resets after its invoke: `setCapturing(false)`, `setIsProcessing(false)`, `setIsAIProcessing(false)`, `setIsContinuousMode(false)`, `setIsRecordingInContinuousMode(false)`, `setRecordingProgress(0)`, `setVadMetrics(null)`;
- calls `setIsPopoverOpen(true)`, the same way start errors are shown.

It does not call `stop_system_audio_capture`; the backend has already reset its state.

## Tests (write first, and confirm they fail on current code)
Add `#[cfg(test)] mod tests` at the bottom of `linux.rs`. The tests go through the module's public interface, `crate::speaker::SpeakerInput` and `SpeakerStream`, against the live Pulse or PipeWire-pulse server. The private `Pulse` helper is used only for the fixture.

**Fixture:** `NullSink` guard.
- Created with `introspect().load_module("module-null-sink", "sink_name=cluers_test_<pid>_<n>")`. `PA_INVALID_INDEX` is a panic.
- `Drop` calls `unload_module`.
- Each test gets a unique monitor source, so the tests never touch the user's real devices or a running pluely instance.

Test cases:
1. **`new_errors_for_missing_device`:** `SpeakerInput::new_with_device(Some("cluers-no-such-sink".into())).is_err()`. Fails today because `new` is always `Ok`.
2. **`stream_ends_with_error_when_capture_is_killed`** (`#[tokio::test]`):
   - Create a `NullSink`, then `new_with_device(Some(sink))?.stream()`.
   - Await one sample within a 2 s timeout, which proves the stream is live.
   - Find the source outputs whose `source` equals the sink's monitor index (via `get_source_info_by_name("<sink>.monitor")` and `get_source_output_info_list`). Call `kill_source_output` on each.
   - `tokio::time::timeout(2s, async { while stream.next().await.is_some() {} })` must finish, and `stream.take_error().is_some()` must hold.
   - Fails today: reads loop forever, so the timeout fires.
3. **`null_sink_listed_as_output_only`:** with a `NullSink`, `get_output_devices()` contains the sink id and `get_input_devices()` does not contain `<sink>.monitor`. This guards the deduplicated enumeration.

These tests need a running Pulse-compatible server. Without one they fail loudly, which is correct: there is no CI `cargo test` job (only `publish.yml` and `sync_fork.yml`), and the dev machine runs PipeWire-pulse.

## Steps
1. Add the tests module and fixture. Run `nix develop -c cargo test --manifest-path src-tauri/Cargo.toml speaker::linux` and confirm tests 1 and 2 fail and test 3 passes.
2. Add `Pulse` and rewrite the enumeration. Test 3 must still pass.
3. Move `Simple` into `new`, merge the state into `Shared`, make read errors terminal, and add `take_error`. Tests 1 and 2 must pass.
4. Add the `take_error` wrapper to `mod.rs`.
5. Make the `commands.rs` hunks, rebased onto 04 once 04 lands.
6. Add the TypeScript listener.
7. Verify:
   - `nix develop -c cargo test --manifest-path src-tauri/Cargo.toml`
   - `nix develop -c cargo check --manifest-path src-tauri/Cargo.toml` with no warnings
   - The frontend type check the repo uses (e.g. `nix develop -c npx tsc --noEmit`)
8. Manual check:
   - Start VAD capture, then run `pactl kill-source-output <idx>` or `systemctl --user restart pipewire-pulse`. The UI must leave "listening" and show the error.
   - Select a nonexistent output device, then start. There must be an immediate error, not a hang.

## Trade-offs
See the tradeoffs field.

## Review amendments (orchestrator)
- Sample rate: option A (fixed 44.1k, truthful because Pulse resamples). Issue 08 moves VAD to ms-based timing next phase.
- `take_error()` accepted for this phase. Issue 08 should consider `Stream<Item = Result<f32, _>>` when it owns the run_* internals.
- Live-Pulse integration tests accepted (the dev machine runs PipeWire). They must fail loudly, not skip, when no server is available.
- 04 edits the same `start_system_audio_capture` hunk. The orchestrator merges 04 first and resolves the conflict. Keep your edits in that function minimal.
