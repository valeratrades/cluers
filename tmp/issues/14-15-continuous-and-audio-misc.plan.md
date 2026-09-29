
# Plan: issue 14+15, continuous mode and audio misc (Linux)

Worktree setup: `git reset --hard riir`. Inside the worktree, use `nix develop path:. -c ...`.

## Root cause (issue 14)
In continuous mode, the backend task runs only for one recording. The recording is started by `start_system_audio_capture` and stopped by an `app.listen("manual-stop-continuous")` flag. Every bug in issue 14 follows from that lifecycle:
- There is no task between recordings, so quick actions fail with "not running".
- A new recording has to `stop` first, and that aborts the answer still in flight.
- Calibration calls `startBackend` to restart the session, but in continuous mode that call starts a recording.
- Stop aborts the task, so `unlisten` is skipped and the listener leaks.
- The recorder preallocates `sr*secs` and creates one `sleep` future per sample.

**Fix:** a continuous capture becomes a session-long task, the same as VAD mode. Recording start, send and discard become control messages to that live task.

**Decision on stop semantics:**
- "Stop & Send" (Enter) flushes the recording.
- Discard (Esc) drops it.
- Stop capture (header button or global shortcut) ends the session and drops everything: the recording and any in-flight STT/LLM. This matches VAD mode and ARCHITECTURE ("stop aborts and awaits").

## Tests first (red)

### R1. `src-tauri/src/speaker/commands.rs` `mod tests`: data-driven `recordings` spec
- Same level as the existing `lockstep` test. Use the app pattern from `capture.rs` tests: `tauri::test::mock_builder().build(mock_context(noop_assets()))`.
- Samples are fed through an `mpsc::UnboundedReceiver<f32>` wrapped with `stream::poll_fn(|cx| rx.poll_recv(cx))`. Actions go through a `mpsc::UnboundedReceiver<Record>`. Config goes through a `watch` channel.
- After each script step, poll the adapter with `now_or_never()` until it is Pending, and collect the items.
- Collect the `continuous-recording-stopped`, `recording-progress` and `speech-discarded` payloads with `app.listen`.
- Script type: `enum Step { Loud(ms), Quiet(ms), Act(Record), Max(secs) }`.
- Expected output per row: segments as `(start_ms, end_ms, n_samples)`, plus the list of stopped reasons.

| Row | Script | Expected |
|---|---|---|
| between start and send | `Loud(1000), Start, Loud(2000), Send` | `[(1000, 3000, 88200)]`, `[sent]` |
| discard | `Start, Loud(1000), Discard` | no segments, `[discarded]` |
| limit auto-sends | `Max(1), Start, Loud(1500)` | `[(0, 1000, 44100)]`, `[limit]`; a later `Send` is a no-op |
| send while idle | `Send` | no segments, no stop events |
| silent recording | `Start, Quiet(1000), Send` | `[discarded]` and a `speech-discarded` event |
| shorter than `min_speech_ms` | `Start, Loud(50), Send` | `[discarded]` |
| start while recording | `Start, Loud(1000), Start, Loud(1000), Send` | one segment `(0, 2000)` |
| progress | `Start, Loud(2500)` | progress events `[1, 2]` |
| live max change | `Start, Loud(500), Max(1), Loud(600)` | limit fires at 1000 ms |

On current code this is red because the function does not exist yet.

### R2. `commands.rs` `lifecycle` table
- Routing moves into `AudioState::control`.
- Add ops `Prompt` and `Record`:
  - `Record` on a capture whose open-closure dropped the record receiver (VAD mode) returns `Err`.
  - `Prompt` on a live capture returns `Ok`.
  - Both return `Err` when idle.

### R3. `src-tauri/tests/vad.rs`
- `config_rejected`: add rows `min_speech_ms >= max_segment_ms`, `pre_speech_ms > 10_000` and `max_recording_duration_secs = 0`. Remove the `hop 0` row, and change `silence < hop` to use `vad::HOP_MS`.
- New `reconfigure_is_live` table over the `pauses` fixture at 44.1 kHz:
  1. Calling `reconfigure(&same)` before every chunk gives events identical to no reconfigure. This proves that the active segment, pre-roll and floor survive.
  2. Switching `silence_ms` from 1000 to 3000 at t=X gives `expected(truth, 3000)` for the segments after X. Reuse the `expected` helper.

### R4. `src/hooks/useSystemAudio.test.tsx` (new file)
Follow the pattern of `useGlobalShortcuts.test.tsx`:
- `mockIPC` records `(cmd, args)`, and `shouldMockEvents` lets the test emit backend events.
- `vi.mock("@/contexts")` provides `useApp` values. `vi.mock` `resolveProviderInput` and `buildEnhancedSystemPrompt`.
- A harness component exposes the hook result through a ref.

| Case | Expected | Red today? |
|---|---|---|
| continuous `startCapture` | invokes `start_system_audio_capture` | yes |
| `startContinuousRecording` | invokes `system_audio_control {kind:"record", action:"start"}` and never `stop_system_audio_capture` | yes |
| Enter keydown on an `<input>` while recording | no `record/send` | yes |
| Enter on `document.body` | `record/send` | no |
| emit `continuous-recording-stopped` with `"limit"` | `isProcessing` true and the notice is set | yes |
| `update_vad_config` rejects | `error` is set | yes |

## Rust changes

### 1. `src-tauri/src/speaker/vad.rs`
- Remove `hop_ms` from `VadConfig` and add `pub const HOP_MS: u32 = 20;`. No UI edits it, and it is the only field that cannot change live. Unknown keys in old localStorage JSON are ignored by serde, so no shim is needed.
- `validate()` additions:
  - `min_speech_ms < max_segment_ms`
  - `pre_speech_ms <= 10_000`
  - `1 <= max_recording_duration_secs <= 3600`
- Add `pub fn reconfigure(&mut self, config: &VadConfig) -> Result<(), String>`:
  - `let fresh = Self::new(config, self.sr)?;`
  - `*self = Self { pending, samples_seen, hops_seen, pre_roll, floor, run, active, ..fresh }` (the stream-state fields are moved out of `self` with `mem::take` / `take`).
  - `hop_len` cannot change, because the hop is a constant and `sr` is fixed.

### 2. `src-tauri/src/speaker/turn.rs`
- Use `HOP_MS` instead of `vad.hop_ms`.
- Add `pub fn reconfigure(&mut self, vad: &VadConfig)`, which recomputes `mic_hold_ms`. Without it, a live `silence_ms` change misjudges echo runs.

### 3. `src-tauri/src/lib.rs`
- `AudioState.vad_config: Arc<Mutex<VadConfig>>` becomes `vad: tokio::sync::watch::Sender<VadConfig>`. tokio 1.47 implements `Default` for `Sender<T: Default>`.
- Remove the `speaker::manual_stop_continuous` and `speaker::get_vad_config` registrations. Register `speaker::default_vad_config`.
- Drop the unused `Arc`/`Mutex` imports if nothing else uses them.

### 4. `src-tauri/src/speaker/commands.rs`
**New and changed types:**
- `#[derive(Deserialize, Clone, Copy)] #[serde(rename_all = "camelCase")] pub enum Record { Start, Send, Discard }`
- Add `Control::Record { action: Record }`.
- `Capture` becomes `{ task, control, record: mpsc::UnboundedSender<Record> }`.
- `AudioState::start`: `open: FnOnce(UnboundedReceiver<Control>, UnboundedReceiver<Record>)`. The VAD path drops the record receiver.
- New `AudioState::control(&self, Control) -> Result<(), String>`:
  - `Record` goes to `record`; a failed send returns `"Continuous recording is not available (auto-detect mode or capture ended)"`.
  - Everything else goes to `control`.
  - `system_audio_control` becomes a one-line wrapper around it.

**`start_system_audio_capture`:**
- `vad_config: VadConfig` is now required. Call `state.vad.send_replace(cfg)` after `validate()`, then `let vad = state.vad.subscribe()`.
- VAD path: inside the lockstep `.map`, `if vad.has_changed() { let c = vad.borrow_and_update().clone(); sys_seg.reconfigure(&c).expect(..); mic_seg.reconfigure(&c).expect(..) }`. The `expect` holds because `update_vad_config` validated the config.
- Continuous path: `drive(&app, sr, vad, recordings(&app, &mut stream, rec_rx, sr, vad.clone()), ...)`, then `report_stream_error`.
- Delete `run_continuous_capture`, `manual_stop_continuous`, `audio-encoding-error`, the per-sample `sleep` and the preallocation, together with their imports (`AtomicBool`, `Listener`, `Instant` if unused).

**New private `recordings<'a, R: Runtime>(app: &'a AppHandle<R>, samples, actions, sr, vad: watch::Receiver<VadConfig>) -> impl Stream<Item = (Vec<(Speaker, VadEvent)>, u64)> + 'a`:**
- Built with `stream::poll_fn` like `lockstep`. Each iteration polls `actions.poll_recv` first, then `samples.ready_chunks(4096)`. It ends when either source ends.
- State: `buf: Option<Vec<f32>>`, `start: u64`, `seen: u64` (sample clock).
- `Start`: when idle, set `buf = Some(Vec::new())`, `start = seen`, and emit `continuous-recording-start`. When already recording it is a no-op (tail comment: Enter and a click can race).
- Chunks: always advance `seen`. When recording, append to the buffer and emit `recording-progress` each time a whole recorded second is crossed. When `len >= sr * max_recording_duration_secs` (read live from `vad.borrow()`), finish with `Stopped::Limit`.
- `Send` finishes with `Stopped::Sent`. `Discard` emits `Stopped::Discarded`. When idle, both are no-ops (tail comment: they race the limit auto-send).
- Finish:
  - shorter than `min_speech_ms`: `speech-discarded "too short"` and `Discarded`
  - peak below the gate: the existing silent message and `Discarded`
  - otherwise emit `Stopped::Sent` or `Stopped::Limit`, then yield `(vec![(Interviewer, Segment { samples: apply_noise_gate(..), start_ms, end_ms })], end_ms)`
- The `min_speech_ms` rule also keeps segment `start_ms` values unique, which `Turns` asserts.
- `#[derive(Serialize, Clone, Copy)] #[serde(rename_all = "camelCase")] enum Stopped { Sent, Limit, Discarded }` is the payload of `continuous-recording-stopped`.

**`drive`:**
- Takes `vad: watch::Receiver<VadConfig>` instead of `&VadConfig`, and computes `let continuous = !vad.borrow().enabled;` once.
- `Turns::new(.., &vad.borrow())`.
- New select arm: `Ok(()) = vad.changed() => { cfg = vad.borrow_and_update().clone(); turns.reconfigure(&cfg); vec![] }`. Metrics use the latest `cfg`.
- After an interviewer `Input::Segment`, add `if continuous { inputs.push(Input::Flush) } // each recording is one turn`.

**Config commands:**
- `update_vad_config`: `validate()?` then `state.vad.send_replace(config)`.
- `get_vad_config` is replaced by `default_vad_config() -> VadConfig`, which returns `VadConfig::default()`. It is the single source of defaults.

**`calibrate_vad_thresholds`:**
- `stream.next().await` can block forever, so the deadline never fires. Wrap the sampling loop in `tokio::time::timeout(duration_secs + 2)` and return `Err` on timeout.
- Replace the per-sample `VecDeque` with `ready_chunks`.

**Swallowed-error audit:** add a tail comment to `linux.rs:81` `unwrap_or_else` (Pulse descriptions are optional). The `.expect`s are invariant asserts and stay (fail-fast).

### 5. `src-tauri/src/speaker/linux.rs` tests
- Replace `first_sample_arrives_within_200ms` with `delivers_audio_in_small_fragments`:
  - Wait for the first sample, with a 5 s hang guard.
  - Read 44 100 samples. Count a "fragment" each time `stream.next().now_or_never()` is `None` before the next `await`.
  - Assert `fragments >= 10`: about 40 with a 20 ms fragsize, at most 2 with the ~2 s server default.
  - It counts fragments instead of timing them, so it holds under load and still catches a regression to ~2 s.
- `stream_ends_with_error_when_capture_is_killed`:
  1. Reproduce first:
     ```
     for i in $(seq 200); do nix develop -c cargo test --manifest-path src-tauri/Cargo.toml --lib speaker::linux -- --test-threads=8 || break; done
     ```
     Run it under parallel load, for example a concurrent `cargo build`.
  2. If the failure is a module load/unload assert or a kill racing other tests' `NullSink`s, add `static LIVE_PULSE: Mutex<()>`. Each live test takes `let serial = LIVE_PULSE.lock().unwrap();` and ends with an explicit `drop(serial)` (no underscore names).
  3. If the 5 s "stream ends" timeout itself fires (PipeWire not failing the killed stream), document on the test why it is inherently racy and keep it.

## TS changes

### `src/hooks/useSystemAudio.ts`
- Delete `DEFAULT_VAD_CONFIG` and `hop_ms`.
- `vadConfig` becomes `VadConfig | null`. On mount, use localStorage `vad_config_v2` if present, otherwise `invoke("default_vad_config")`. A parse failure calls `setError` and does not fall back.
- Add `resetVadConfig()`, which fetches the defaults and keeps `enabled`.
- `startCapture` starts the backend in both modes (the stop-first there is at session start, where nothing is in flight).
- `startContinuousRecording`, `manualStopAndSend` and `ignoreContinuousRecording` send `system_audio_control {kind:"record", action}` and never stop. Remove the optimistic `setIsProcessing` and `manual_stop_continuous`.
- `continuous-recording-stopped` listener:
  - payload `"sent"` or `"limit"`: `setIsProcessing(true)`
  - `"limit"`: notice "max duration reached, sent"
  - always reset recording and progress
- Remove the `audio-encoding-error` listener.
- `backendLiveRef` becomes `capturing`.
- `startNewConversation`, the `updateVadConfiguration` mode switch and `calibrateVad` restart for both modes, and reset `isRecordingInContinuousMode` after the stop.
- Keyboard handler: return early when `e.target` is an input, a textarea or `isContentEditable`, for Enter, Space and Esc alike.
- Catches:
  - lines 215, 226, 241, 323, 531, 561 and 817 now call `setError` (the 817 catch currently hides validation errors)
  - `js_log` catch gets a tail comment: logging must not recurse
  - the unmount stop catch forwards to `dbg`

### Speech components
- `SettingsPanel.tsx`: delete `handleResetDefaults`. Add an `onResetVadConfig` prop.
- `speech/index.tsx`:
  - pass the `onResetVadConfig` prop
  - handle a `vadConfig` that is still null (render nothing until it loads)
  - the amber box label changes from "Recording discarded:" to "Recording:"
- `PermissionFlow.tsx` and `audio-visualizer.tsx`: add a tail justification to each remaining catch (the next poll retries; the visualizer is cosmetic).

### Device selection
- `src/pages/audio/components/AudioSelection.tsx`: delete the block that silently rewrites a saved but missing device to the default. `""` stays "system default". A saved id that is not listed shows "(not connected)". Starting a capture then fails fast in Rust with "PulseAudio source X not found".
- The device-load catch shows its error instead of only logging it.
- `src/pages/chats/components/AudioRecorder.tsx`: the transcription and start catches show an inline error instead of silently calling `onCancel`.

## Docs
`ARCHITECTURE.md` speaker section:
- The continuous capture lives for the whole session. Recording is driven by `Control::Record`, and each recording is one turn (Flush).
- The VAD config is a `watch` in `AudioState`. `update_vad_config` reaches the live segmenters and `Turns`.
- The hop is a constant.

## Verification
- `nix develop -c cargo test --manifest-path src-tauri/Cargo.toml` (needs Pulse)
- `nix develop -c cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets` with no new warnings
- `nix develop -c npx tsc --noEmit` and `nix develop -c npm test`
- The flake loop above.
- Manual GUI checks in continuous mode:
  - a quick action between recordings works
  - Enter in the context textarea does not send
  - start a recording while an answer streams: the answer is kept
  - the 1-minute limit sends and shows the notice
  - calibrate does not start a recording
  - moving a slider during VAD capture changes the meter thresholds live

## Commit order
1. Red tests R1–R4.
2. `vad`/`turn` changes (R3 green).
3. `commands.rs`/`lib.rs` (R1, R2 green).
4. TS rewire (R4 green).
5. Swallowed-error and device sweep.
6. `linux.rs` tests.
7. ARCHITECTURE.md.

## Sibling overlap
Issue 17 also edits `linux.rs:48` (clippy) and sweeps the command list in `lib.rs`. Leave `linux.rs:48` to 17. The command-list edits here will cause a trivial merge conflict with 17.

## Review amendments (orchestrator)
- You own removing `get_vad_config`. Issue 17 runs in parallel and edits `lib.rs` (dead command registrations) and one line in `linux.rs` (a clippy type alias). Keep your lib.rs edits limited to the audio registrations and state; the orchestrator merges.
- The AudioRecorder `getUserMedia` device-id mapping is tracked as issue 21. Only make its error visible, as your plan says.
