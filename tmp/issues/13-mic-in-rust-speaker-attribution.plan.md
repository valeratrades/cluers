`# Plan for issue 13: mic capture in Rust and speaker attribution

Worktree setup, as in `tmp/issues/README.md`: run `git reset --hard riir` first, then run every tool through `nix develop path:. -c …`. Phase 4 contains only this issue, so no other issue touches these files.

## 0. Decisions

### Mic capture uses PulseAudio, not cpal
- The mic is captured with **libpulse-simple**, reusing the existing code in `speaker/linux.rs`. cpal is not used.
- Why not cpal: on Linux, cpal is ALSA-only. `get_input_devices` already returns **Pulse source names**, and ALSA cannot open a device by a Pulse source name.
- Pulse already delivers mono f32 at 44.1 kHz for the monitor. The mic gets the same stream type, the same producer thread, the same error reporting and the same NullSink test harness.
- `cpal` is removed from `Cargo.toml`. Issue 17 lists it "unless 13 uses it"; this plan does not use it.

### Echo mitigation: attribute by time against the system-audio reference, no AEC
Both channels go through the same `Segmenter` in lockstep, so they share one sample clock. The turn machine then decides whether a mic run is the user taking the turn:

- `T = last interviewer speech end + BLEED_TAIL_MS`.
- The decision is only made once the interviewer segment is idle.
- A mic run **takes the turn** if the mic has speech after `T`. That is true when any of these holds:
  - the run's onset is after `T`;
  - the run's end is after `T`;
  - the run is still open at `T + mic_hold`, where `mic_hold` is how long the segmenter holds a run open after speech stops. If the run had gone quiet before `T`, the segmenter would have closed it by then.
- Echo starts after the interviewer speech that caused it and stops within the playback latency. So a mic run that starts inside the interviewer's speech and ends before `T` is echo or a backchannel, and it is ignored.
- This one rule covers:
  - echo;
  - user backchannels during interviewer speech;
  - overlap: the user keeps talking after the interviewer stops, and the turn closes about 1.4s after the interviewer's end instead of 2s;
  - the speakers case where echo and a quick reply merge into one mic run.
- **Rejected:**
  - webrtc AEC or cross-correlation: a new dependency, frame and resample plumbing, and it needs tuning.
  - PipeWire `module-echo-cancel`: it is set up on the user's side, so the app cannot rely on it.
- **Known limit:** the user interrupting the interviewer mid-question is not treated as taking the turn. Detecting that would need a real echo canceller.

### Interviewer backchannels while the user is speaking are dropped
This is the "mm-hm while answering" case from the issue. An interviewer segment is dropped, and its transcript ignored, when both hold:
- it is shorter than `BACKCHANNEL_MS`;
- it starts while the user holds the floor, meaning a mic run that took the turn and had not ended by then.

It is still transcribed (the driver transcribes every system segment), but it is never asked.

### Other scope decisions
- **User speech is never transcribed or asked.** Mic segments only feed the turn machine. This saves STT cost. If the LLM should later see the user's answer, transcribe mic segments then.
- **The mic is required in VAD mode.** Continuous mode records system audio only and does not open the mic.
  - On macOS and Windows, `SpeakerInput::microphone` returns an explicit `Err`. The code still compiles there, but starting VAD capture fails with a clear message.
- **The browser Silero path is deleted.** The overlay mic button and the `audio_recording` shortcut become push-to-talk by reusing the existing `AudioRecorder` from the chats page, so there is still no second VAD.

## 1. Red: extend the turn-machine interface and add two-channel fixtures (`src-tauri/src/speaker/turn.rs`, `src-tauri/tests/turn.rs`)

### 1a. Mechanical interface change (the machine's behaviour stays the same)

In `turn.rs`, add:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speaker { Interviewer, User }

pub enum Input {
    SpeechStart { speaker: Speaker, at_ms: u64 },
    Segment { speaker: Speaker, start_ms: u64, end_ms: u64 }, // start_ms is the id for Interviewer segments
    Discarded { speaker: Speaker },
    Transcript { .. }, Tick { .. }, Flush, Prompt(String), Reply(..), // unchanged
}
```

- In the red commit, every `Speaker::User` input is a no-op. That reproduces today's "the system path never looks at the mic".
- Update `commands.rs::drive` to tag every VAD event with `Speaker::Interviewer`. This is a mechanical change.
- Update the existing test helpers `seg`, `gap` and `run` to pass `Speaker::Interviewer`.

### 1b. New data-driven scene test in `tests/turn.rs`

The scenes need no new fixture files. Each clip is cut from `pauses_{rate}.wav` using `pauses.truth`:
- `"u1"` … `"u4"` are the full utterances.
- `"mm"` is the first 500 ms of u4.

```rust
struct Scene {
    name: &'static str,
    interviewer: &'static [(&'static str, u64)],  // (clip, at_ms)
    user: &'static [(&'static str, u64)],
    bleed: Option<(u64, f32)>,                    // (delay_ms, gain): mic += gain * system delayed
    want: &'static [&'static str],
}
```

`run_scene(scene, rate)`:
1. **Build the channels.**
   - Build a zeroed system channel and a zeroed mic channel. Length = last clip end + 3000 ms.
   - Add the clips into their channel.
   - Then add the echo: `mic[i + d] += gain * sys[i]`.
2. **Segment both channels in lockstep, exactly as production does.**
   - Create two segmenters: `Segmenter::new(&VadConfig::default(), rate)` for each channel.
   - Push 4096-sample chunks: the same slice range to each segmenter.
   - Feed the interviewer events first, then the mic events, then `Tick`.
   - Push `Flush` at the end.
3. **Fake STT** returns the labels of the *interviewer* clips that overlap the segment. Mic segments are never transcribed.
4. **Timing in the trace.** Ask lines get a suffix `+{(now - last_interviewer_segment_end_ms)/1000}s`, which makes "closed on user start" (+1s) distinguishable from "closed on the gap" (+2s).
   - To support this, `Sim` gains `last_interviewer_end: u64` and `ask_lag: Vec<u64>`.
   - The existing `run` ignores both. Its wants stay unchanged.
5. **Vacuity guard.** If a scene has any mic content, assert that the mic segmenter emitted at least one `SpeechStart`. This keeps the echo scenes from passing vacuously.
6. **Replies** are `["A"]`. Each scene asks at most once.
7. **Every rate must match the wanted trace.** Failures are collected per `(scene, rate)`, following the `fixture_turns` pattern.

Let `Q = [("u1",1000),("u2",4124)]` (one VAD segment ending about 6980, with a 1.5 s pause before u3@8480).

| scene | interviewer | user | bleed | want | red? |
|---|---|---|---|---|---|
| interviewer only | Q+u3@8480 | – | – | `ask "u1 u2 u3" h=0 +2s`, `answered` | green |
| user answers | Q | u4@7580 | – | `ask "u1 u2" h=0 +1s`, `answered` | **red** (+2s) |
| user only | – | u4@1000 | – | *(empty)* | green |
| user backchannel | Q+u3@8480 | mm@2000 | – | same as interviewer only | green |
| overlap | Q | u4@6000 | – | `ask "u1 u2" h=0 +1s`, `answered` | **red** |
| echo 50ms | Q+u3@8480 | – | (50, 0.3) | same as interviewer only | green |
| echo 300ms (Bluetooth) | Q+u3@8480 | – | (300, 0.3) | same as interviewer only | green |
| echo + reply | Q | u4@7580 | (150, 0.3) | `ask "u1 u2" h=0 +1s`, `answered` | **red** |
| interviewer mm-hm while user answers | Q+mm@9000 | u4@7580 | – | `ask "u1 u2" h=0 +1s`, `answered` | **red** (extra `ask "mm"`) |

### 1c. Scripted boundary test `attribution_boundaries` (table-driven, uses `check`)

This test pins the two constants:

- **Echo tail.**
  - Setup: interviewer segment `[100,1000]` "a"; then a user run of 300 ms; then interviewer "b" at 2500 (inside `TURN_GAP_MS`); then Flush.
  - User run `[T-400, T-100]` → echo → one ask `"a b"`.
  - User run `[T+100, T+400]` → turn taken → asks `"a"` and `"b"` separately.
- **Backchannel length.**
  - Setup: the user holds the floor (took the turn), then the interviewer speaks a segment.
  - A segment of length `BACKCHANNEL_MS-100` → no ask.
  - A segment of length `BACKCHANNEL_MS+100` → asked.

Commit this state and record the failing output in the commit message.

## 2. Green: attribution rules in `turn.rs`

```rust
const BLEED_TAIL_MS: u64 = 400;    // playback latency (Bluetooth ~300ms) can put echo after short interviewer speech
const BACKCHANNEL_MS: u64 = 1000;  // "mm-hm", "right"

struct UserRun { start_ms: u64, end_ms: Option<u64>, took_turn: Option<bool> } // latest mic run only
```

- **New fields on `Turns`:**
  - `user: Option<UserRun>`
  - `backchannels: BTreeSet<u64>` (dropped interviewer segment ids)
  - `now_ms: u64` (set by `Tick`)
  - `mic_hold_ms: u64`
- **Constructor:** `Turns::new(history, carry, vad: &VadConfig)`.
  - It sets `mic_hold_ms = silence_ms.div_ceil(hop_ms) * hop_ms + hop_ms`, which mirrors the segmenter's hold plus one hop of rounding margin.
  - Tail comment: `// a mic run with no speech after t has ended by t + this`.
  - Update call sites: `drive` passes `vad_cfg`; the tests pass `&VadConfig::default()`.
- **`push` arms:**
  - `SpeechStart{User, at}` → `user = Some(UserRun{start_ms: at, end_ms: None, took_turn: None})`. This overwrites the previous run. Mic runs are sequential, and the newest run decides.
  - `Segment{User, end_ms, ..}` → `user.as_mut().expect("mic segment without speech start").end_ms = Some(end_ms)`.
  - `Discarded{User}` → `user = None`. A run under `min_speech_ms` is noise.
  - `Segment{Interviewer, s, e}`:
    - Set `speaking = false` and `last_end_ms = e`.
    - If `e - s < BACKCHANNEL_MS && self.user_holds_floor_at(s)`, insert `s` into `backchannels`.
    - Otherwise insert into `open` (existing path).
  - `Transcript{start_ms}`: first `if self.backchannels.remove(&start_ms) { return/skip }`, then the existing lookup, which still panics for unknown segment ids.
  - `Tick{now}` → also set `now_ms`.
- **`resolve_user()`** runs after every input, before `pump`:
  ```
  let Some(run) = user.as_mut().filter(|r| r.took_turn.is_none()) else return;
  if speaking { return }
  let t = last_end_ms + BLEED_TAIL_MS;
  let took = if run.start_ms > t { true } else { match run.end_ms {
      Some(x) => x > t, None if now_ms >= t + mic_hold_ms => true, None => return } };
  run.took_turn = Some(took); if took { self.close() }
  ```
- **`user_holds_floor_at(s)`** is `user.is_some_and(|r| r.took_turn == Some(true) && r.start_ms <= s && r.end_ms.is_none_or(|e| e >= s))`.
- **Comments:**
  - Replace the tail comment on `TURN_GAP_MS` with: it is the close rule when the user stays silent.
  - Add at `resolve_user`: `// ponytail: echo judged by timing against the interviewer's VAD; the user barging in mid-question falls back to TURN_GAP_MS. Needs real AEC to detect.`
- **Expected result:** all scenes and boundaries are green, and the existing `fixture_turns`, `scripted` and `turn_gap_boundary` stay green.

## 3. Mic capture and lockstep driver

### 3a. `src-tauri/src/speaker/linux.rs`

Split `SpeakerInput::new` into:
- `new(device_id)`: builds the monitor source name, as today.
- `pub fn microphone(device_id: Option<String>) -> Result<Self>`: uses the source name `id` when it is non-empty and not `"default"`, otherwise `"@DEFAULT_SOURCE@"`.
- Both call a private `fn open(source: String, stream_name: &str) -> Result<Self>`, which is the existing body: the existence check, the Spec at 44100 mono f32, the 20 ms fragsize, and `Simple::new`. The stream names are `"System Audio Capture"` and `"Microphone Capture"`.

Tests: extend the existing live-Pulse tests with one table test that checks both constructors reject a missing device. Also add one test that `microphone(Some(sink.monitor()))` delivers a sample within 5 s; a monitor is a valid Pulse source.

### 3b. `src-tauri/src/speaker/mod.rs`

Add this facade. It compiles on every platform, and the message uses the argument, so no underscore name is needed:

```rust
pub fn microphone(device_id: Option<String>) -> Result<Self> {
    #[cfg(target_os = "linux")]
    return Ok(Self { inner: PlatformSpeakerInput::microphone(device_id)? });
    #[cfg(not(target_os = "linux"))]
    Err(anyhow::anyhow!("Microphone capture ({device_id:?}) is only implemented on Linux"))
}
```

### 3c. `src-tauri/src/speaker/commands.rs`

- **`start_system_audio_capture`** gains the argument `mic_device_id: Option<String>` (wire name `micDeviceId`).
- **In the sync `open` closure, when `vad_config.enabled`:**
  - Open the mic **before** `SpeakerInput::new_with_device`. Any skew between the two openings then only delays the mic, and an echo onset must never appear to come before the interviewer's.
  - Map the mic error to `format!("Failed to access microphone: {e}")`.
  - `assert_eq!(mic.sample_rate(), sr, "both Pulse streams request 44.1kHz")`.
  - Build both `Segmenter`s with `?`.
- **VAD audio stream:**
  ```rust
  (&mut stream).ready_chunks(4096).zip((&mut mic).ready_chunks(4096)).map(|(s, m)| {
      sys_buf.extend(s); mic_buf.extend(m);
      let n = sys_buf.len().min(mic_buf.len()); seen += n as u64;
      let mut ev: Vec<_> = vad.push(&sys_buf[..n]).into_iter().map(|e| (Speaker::Interviewer, e)).collect();
      ev.extend(mic_vad.push(&mic_buf[..n]).into_iter().map(|e| (Speaker::User, e))); // interviewer first: echo never precedes its source
      sys_buf.drain(..n); mic_buf.drain(..n);
      (ev, seen * 1000 / sr as u64)
  })
  ```
  - `zip` ends when either stream ends. That is fail-fast: a mic that dies ends the capture.
  - `// ponytail: one sample clock for both devices; under plain PulseAudio (no PipeWire rate matching) clock drift is ~0.2s/h worst case. Resync on timestamps if long sessions show it.`
  - After `drive`, call `report_stream_error` for `stream.error()` and for `mic.error()`.
- **`drive`:**
  - The audio item becomes `(Vec<(Speaker, VadEvent)>, u64)`, and the machine is built with `Turns::new(history, carry, vad_cfg)`.
  - `Segment` → STT only for `Interviewer`, then `Input::Segment{speaker,..}`.
  - `Discarded` → emit `speech-discarded` only for `Interviewer`, then `Input::Discarded{speaker}`.
  - `Metrics` → emit `vad-metrics` only for `Interviewer`. The UI meters and calibration stay about system audio.
- **Continuous path:** `stream::iter([(vec![(Speaker::Interviewer, segment)], end_ms)])`. It opens no mic.
- **Mic VAD config:** the same `VadConfig` as system audio. Calibration still measures system audio only; the segmenter's adaptive floor absorbs the difference in mic noise.

### 3d. `src-tauri/Cargo.toml`

Delete `cpal = "0.15.3"`. Let `cargo build` rewrite `Cargo.lock`.

## 4. Frontend

- **`src/hooks/useSystemAudio.ts` `startBackend`:** pass `micDeviceId: selectedAudioDevices.input.id !== "default" ? selectedAudioDevices.input.id : null`, and add `selectedAudioDevices.input.id` to the deps. The device id is a Pulse source name from `get_input_devices`, which is what Rust expects.
- **Delete** `src/pages/app/components/completion/AutoSpeechVad.tsx`.
- **Delete** `floatArrayToWav` from `src/lib/utils.ts`. Its only importer is gone.
- **`npm uninstall @ricky0123/vad-react`.** This updates `package.json` and the lockfile.
- **`src/pages/app/components/completion/Audio.tsx`:**
  - The trigger is always the plain mic `Button`, which toggles `isRecording`.
  - `PopoverContent`:
    - If no provider is configured, show the existing "configure provider" text.
    - Otherwise, while recording, render `<AudioRecorder onTranscriptionComplete={(t) => { setIsRecording(false); submit(t); }} onCancel={() => setIsRecording(false)} />`, imported from `@/pages/chats/components`.
- **Rename `enableVAD`/`setEnableVAD` to `isRecording`/`setIsRecording`** in `src/hooks/useCompletion.ts` (the state, `toggleRecording`, the return value) and `src/types/completion.hook.ts`. The old name would describe behaviour that no longer exists.
- The STT provider checks the issue calls duplicated collapse by deletion. `useSystemAudio` already goes through `resolveProviderInput` (issue 12).

## 5. `ARCHITECTURE.md`, speaker section

Add these bullets:
- The mic is captured through Pulse (`@DEFAULT_SOURCE@` or a named source), next to the monitor. VAD mode requires it; there is no cpal.
- Both channels go through `Segmenter` in lockstep on one sample clock, and interviewer events are fed first.
- `turn.rs` attributes speakers:
  - the interviewer's turn closes on `TURN_GAP_MS` silence, or when the user has mic speech after the echo tail;
  - user speech is never asked;
  - interviewer segments shorter than `BACKCHANNEL_MS` are dropped while the user holds the floor;
  - echo is judged by timing against the system reference, not by AEC.
- The spec is `tests/turn.rs` scenes; they are built from the `pauses` fixture, so there are no new WAVs.

## 6. Verification

```
nix develop path:. -c cargo test --manifest-path src-tauri/Cargo.toml --test turn   # red after step 1, green after step 2
nix develop path:. -c cargo test --manifest-path src-tauri/Cargo.toml               # includes live-Pulse linux tests
nix develop path:. -c cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
nix develop path:. -c npm run build && nix develop path:. -c npm test
```

Manual check with `RUST_LOG=info nix develop path:. -c npm run tauri dev`:
- **Headphones:**
  - Play a question; start answering about 0.5 s after it ends. Expect one answer that arrives earlier than before.
  - Say "mm-hm" mid-question. Expect no split.
- **Speakers:** play a question at normal volume and stay silent. Expect exactly one answer; no answer to the echo.
- **Overlay:** the mic button or ctrl+shift+a opens push-to-talk, and Send submits one message.

## Commit order

1. Red (interface + scenes + boundaries)
2. Green rules
3. linux.rs, mod.rs, commands.rs, Cargo
4. TS
5. Docs

### Critical Files for Implementation
- /home/v/s/other/cluers/src-tauri/src/speaker/turn.rs
- /home/v/s/other/cluers/src-tauri/tests/turn.rs
- /home/v/s/other/cluers/src-tauri/src/speaker/commands.rs
- /home/v/s/other/cluers/src-tauri/src/speaker/linux.rs
- /home/v/s/other/cluers/src/pages/app/components/completion/Audio.tsx

## Review amendments (orchestrator)
- Lockstep stall guard: pairing the two streams in lockstep with `zip` means a mic that stalls without ending (device suspended, source muted at the Pulse level) silently freezes interviewer processing too. Fail fast: if one side's unconsumed buffer exceeds 2s of samples, end the capture with an explicit error that names the lagging device (reported like the other stream errors, via `capture-error`). Add a test that drives `drive`'s audio adapter (or the extracted pairing step) with one stream stalled and asserts the error.
- The overlay changing from auto-listen to push-to-talk is approved. The orchestrator reports it to the owner as a behaviour change.
