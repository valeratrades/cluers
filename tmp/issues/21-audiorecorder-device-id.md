# 21 Push-to-talk mic ignores the selected device (needs a GUI check)

`src/pages/chats/components/AudioRecorder.tsx` passes the saved input device id (a Pulse source name from `get_input_devices`) to `getUserMedia({audio: {deviceId: {exact}}})`. WebKit device ids are opaque hashes, so a non-default saved mic most likely makes getUserMedia fail (or it silently falls back). Since issue 13, the overlay's push-to-talk uses this recorder.

Options: record push-to-talk in Rust through the same Pulse mic path (`SpeakerInput::microphone`), which removes the browser capture entirely and matches the rest of the audio stack. Or map Pulse names to WebKit ids by label. The first option is preferred (one capture stack).
