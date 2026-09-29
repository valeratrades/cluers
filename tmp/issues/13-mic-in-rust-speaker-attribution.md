# 13 Mic capture in Rust; know when the interviewer stops and I start

Depends on 08, 12.

Current: two unrelated pipelines. System audio → Rust energy VAD → useSystemAudio. Mic → browser Silero VAD (`AutoSpeechVad.tsx`, threshold 0.6) → STT → `useCompletion.submit` in a *separate* conversation. System path never looks at the mic: my "mm-hm" while answering (≥0.16s passes `min_speech_chunks`) triggers an LLM call; on speakers the mic path also hears the interviewer → double answers. `cpal` is in Cargo.toml but unused.

Goal:
- Mic captured in Rust (cpal), fed through the same `vad.rs` segmenter, tagged `User`; system audio tagged `Interviewer`.
- `turn.rs` uses both: interviewer turn ends on (interviewer silence ≥ threshold) OR (user speech start); user speech never triggers an answer; brief user backchannels during interviewer speech don't split the interviewer turn.
- Echo: when on speakers, interviewer audio bleeds into mic. Decide mitigation (energy/correlation vs. system reference, or treat mic-speech concurrent with louder system-speech as bleed) — fixture it.
- Browser Silero path removed (single VAD implementation); STT provider checks duplicated in `useSystemAudio.ts:360-373` / `AutoSpeechVad.tsx:42-67` collapse.
- Fixtures: two-channel scenarios (interviewer / user / overlap / backchannel / bleed), asserting turn events.
