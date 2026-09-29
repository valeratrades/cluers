# 15 Audio misc (Linux)

- `update_vad_config` does not reach a running capture (config cloned at start); validation covers 2 fields.
- VAD defaults duplicated in TS (`useSystemAudio.ts:79-89`, `SettingsPanel.tsx:124-134`) if 08 left them; `SettingsPanel.tsx:405-406` assumes 44100 for displayed durations.
- Selected device not found → silent fallback to default device; `.expect` panics in device paths.
- Any remaining `.ok()` / `let _ =` / `unwrap_or` / empty catch in `speaker/` and audio TS without a justification.

Run together with 14 (same files, one agent).
