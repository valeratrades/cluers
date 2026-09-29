# 08 Extract pure VAD segmenter + WAV fixture integration tests

Foundation for all turn-detection work (12, 13, 14/15).

Current: `src-tauri/src/speaker/commands.rs:175-339` `run_vad_capture` is an energy gate (soft-knee noise gate <0.003, speech = rms>0.012 || peak>0.035) fused with `AppHandle::emit`. All durations count 1024-sample hops, so "1s silence" = 45 hops = 0.96s@48k, 1.04s@44.1k, 2.9s@16k (Bluetooth HFP). Hardcoded 30s force-cut (`:189`), 0.15s tail trim, target RMS 0.1, gain ≤10. `hop_size=0` spins forever (`:201`). A single click resets the silence counter; background hum above threshold never cuts.

Goal:
- `src-tauri/src/speaker/vad.rs` (or its own workspace crate if cleaner for `tests/` access): pure, no Tauri. Samples + sample rate in, events out (`SpeechStart`, `Segment{samples,start_ms,end_ms}`, `Discarded`, `Metrics`). All config durations in ms; validated (no zero hop). 30s cap in config.
- `run_vad_capture` becomes a thin loop forwarding events to `emit`.
- `VadConfig` defaults exist in Rust + two TS copies (`useSystemAudio.ts:79-89`, `SettingsPanel.tsx:124-134`) — make Rust the single source (TS reads `get_vad_config`), or leave the TS side to issue 15 if it would collide; state which.
- Fixture tests in `src-tauri/tests/`: deterministic WAVs (generate via a TTS available in the nix flake, e.g. piper/espeak-ng — add to flake.nix devShell; commit generated fixtures, keep generator script) covering: interviewer speech with 0.5s / 1.5s / 3s pauses, then a second speaker; at 16k, 44.1k, 48k; a variant with a constant hum floor; a variant with isolated clicks in silence. Assert segment boundaries within ±50ms of ground truth and identical behaviour across sample rates.
- Read https://matklad.github.io/2021/05/31/how-to-test.html before writing tests (data-driven, test the interface not internals).

Improving the detector itself (hysteresis, adaptive noise floor, so hum/clicks behave) is in scope if fixtures show failures — the fixtures are the spec.
