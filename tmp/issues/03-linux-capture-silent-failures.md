# 03 Linux system-audio capture fails silently

`src-tauri/src/speaker/linux.rs`:
- `SpeakerInput::new` cannot fail; a failed Pulse init returns a stream already shut down, reporting a hardcoded 44100 (`:262-287`). VAD mode then ends with no event → UI stuck on "listening".
- Persistent read error (device removed) loops forever sleeping 100ms (`:416-419`) → dead capture, no signal.
- Sample rate hardcoded to 44.1k regardless of monitor source.
- `check_system_audio_access` (`speaker/commands.rs:624`) always true on Linux.
- Input vs output device enumeration ~95 duplicated lines (`:24-121` vs `:123-219`).
- `"gnome-control-center sound"` passed as a single binary name (`speaker/commands.rs:659`) → never launches.

Acceptance: init failure is an `Err` from `new`; producer-thread exit (error or device loss) terminates the stream with an error that reaches the frontend as an event (`capture-error` or similar) and resets capture state; real sample rate reported; enumeration deduplicated. Only touch `linux.rs`, `speaker/mod.rs` as needed, and the listed lines in `speaker/commands.rs` (start/stop are owned by issue 04 in the same phase — coordinate by keeping edits minimal there).
