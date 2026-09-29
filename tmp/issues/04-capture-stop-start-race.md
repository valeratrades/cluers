# 04 System-audio stop/start race leaks an unstoppable capture task

`src-tauri/src/speaker/commands.rs`:
- `stop_system_audio_capture` aborts the task, sleeps 300ms, then sets `is_capturing=false` (`:590-599`). A start landing inside that window (fast hotkey toggle, `startCapture` doing stop→start, unmount stop) → stale stop clears the flag while the new capture runs.
- Next start overwrites `stream_task` without aborting the previous handle (`:166`) → orphaned task, every utterance emitted and sent to the LLM twice until restart.
- `calibrate_vad_thresholds` not guarded against concurrent start (TOCTOU `:706`); NaN swallowed in calibration sort (`:760`).
- `get_audio_sample_rate` opens a stream on the default device, not the selected one (`:812`).

Acceptance: capture lifecycle is a single state owned under one lock (no sleep-based sequencing); start while running is either an error or an explicit restart, never a leak; stop is idempotent. Integration-style Rust test driving start/stop/start rapidly against a fake stream (the capture fns are already generic over `Stream<f32>`). Do not restructure `run_vad_capture` internals — issue 08 extracts them next phase.
