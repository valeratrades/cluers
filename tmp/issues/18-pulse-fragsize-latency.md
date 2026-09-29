# 18 ~2s capture latency on PipeWire-pulse

Found during issue 03: `pa_simple` opened with default buffer attributes (`None`) on PipeWire-pulse delivers its first fragment only after ~1.96s, so every VAD decision lags speech by ~2s and turn detection feels sluggish. Present before 03 as well.

Fix: set `BufferAttr.fragsize` explicitly (e.g. ~20–50ms of audio at the stream's rate) in `src-tauri/src/speaker/linux.rs` `Simple::new`. Measure first-sample latency before/after with the live-Pulse tests in linux.rs (extend one to assert first sample < 200ms).
