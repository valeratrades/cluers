# 21 Push-to-talk records from the browser with a Pulse device name

`src/pages/chats/components/AudioRecorder.tsx:86-93` passes the saved input device id to `getUserMedia({audio: {deviceId: {exact}}})`. That id is a Pulse source name from `get_input_devices`, but WebKit expects its own opaque device ids. So a non-default saved mic most likely makes getUserMedia fail; since v0.1.14 the error is at least visible.

Consumers: the overlay mic button (push-to-talk since v0.1.13, `src/pages/app/components/completion/Audio.tsx`) and the chats view.

Fix: record push-to-talk in Rust, the same way as the rest of audio capture. `SpeakerInput::microphone(device_id)` already exists (`src-tauri/src/speaker/{mod,linux}.rs`, 44.1 kHz mono f32, fail-fast on a missing device, live-Pulse tests). Needed pieces:
- A start/stop command pair (or a `Record`-style control, see how continuous mode does Start/Send/Discard in `speaker/commands.rs`) that records the mic into a buffer.
- On stop, run STT through `llm::stt::transcribe` and return the text.
- Delete the browser MediaRecorder path, and with it the last renderer-side audio capture.

Mind: VAD capture already opens the mic while it runs. Decide whether push-to-talk during a VAD session is refused or shares the stream.
