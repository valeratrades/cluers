# 14 Continuous (VAD-off) mode still sends prematurely (upstream #223)

- Enter in *any* text input stops and sends in continuous mode (`useSystemAudio.ts:1212`); only Space excludes inputs.
- Calibrating during a manual session restarts recording by itself (`:1141-1152`).
- Max-duration auto-send (180s default, `commands.rs:400-406`) fires silently.
- Stop discards the in-progress utterance / whole continuous recording and leaks the `manual-stop-continuous` listener (`unlisten` skipped at `:421`). Decide: Esc = discard, stop = flush?
- Continuous capture preallocates `sr*secs` (up to ~690MB at 3600s) and creates a `sleep` future per sample (`commands.rs:352,415`).

Line numbers are pre-phase-3; re-locate after 12/13 rewrote useSystemAudio.

After phase 3 (issue 12):
- Regression: in continuous mode between recordings, quick actions fail with "System audio capture is not running", because they now go through `system_audio_control` and no backend task is live. Before, they worked at any time.
- Starting a new continuous recording runs stop first, which aborts an answer still in flight. The start button has no busy guard.
