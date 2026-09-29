# Plan for issue 08: pure VAD segmenter and WAV fixture tests

## 0. Scope decisions

- **Stays in scope:** `src-tauri/src/speaker/vad.rs` (new), `speaker/commands.rs`, `speaker/mod.rs`, `lib.rs`, `flake.nix`, `src-tauri/tests/**` (new), `ARCHITECTURE.md`. The TS side gets only the field-rename edits that the new Rust `VadConfig` makes necessary: `src/hooks/useSystemAudio.ts` (interface, `DEFAULT_VAD_CONFIG`, localStorage key) and `src/pages/app/components/speech/SettingsPanel.tsx` (reset defaults, silence slider).
- **Making Rust the only source of TS defaults is left to issue 15.** Doing it needs a new "default config" command, because `get_vad_config` returns the current state, not the defaults, and the reset button needs the defaults. Issue 15 already lists this ("if 08 left them"). Issue 10 changes the STT call site in `useSystemAudio.ts` in this same phase. Our edits there are limited to lines 28-89 and the two `"vad_config"` literals, so the hunks do not overlap.
- **The `Stream<Item = Result<f32, CaptureError>>` change is not done.** It would touch `speaker/linux.rs`, which issue 18 owns in this phase, plus the macOS and Windows backends. Also, the issue's name for the method is wrong: there is no `take_error`. The method is `SpeakerStream::error()` (`mod.rs:150`, `linux.rs:210`). Leave it for a later issue.
- **`calibrate_vad_thresholds` keeps its hardcoded 1024-sample hop.** It only sets how finely the noise floor is measured and does not decide any durations. Issue 15 can pick it up.

## 1. New module: `src-tauri/src/speaker/vad.rs` (pure: no tauri, no tokio)

Move these into `vad.rs`: `VadConfig` and its `Default` impl, `apply_noise_gate` and `calculate_audio_metrics` (both `pub(super)`, because continuous capture and calibration still use them). `normalize_audio_level` and `samples_to_wav_b64` stay in `commands.rs`, because they prepare the payload for IPC.

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VadConfig {
    pub enabled: bool,
    pub hop_ms: u32,              // 20
    pub sensitivity_rms: f32,     // 0.012
    pub peak_threshold: f32,      // 0.035
    pub silence_ms: u32,          // 1000
    pub min_speech_ms: u32,       // 160
    pub pre_speech_ms: u32,       // 300
    pub max_segment_ms: u32,      // 30000 (replaces hardcoded sr*30)
    pub noise_gate_threshold: f32,// 0.003
    pub max_recording_duration_secs: u64, // 180, continuous mode only
}
impl VadConfig { pub(crate) fn validate(&self) -> Result<(), String> }

#[derive(Debug, Clone, PartialEq)]
pub enum VadEvent {
    SpeechStart { start_ms: u64 },
    Segment { samples: Vec<f32>, start_ms: u64, end_ms: u64 }, // start/end = detected speech on/offset; samples also include pre-roll and a 150ms tail
    Discarded { start_ms: u64, end_ms: u64 },
    Metrics { rms: f32, peak: f32, in_speech: bool },
}

pub struct Segmenter { /* private */ }
impl Segmenter {
    pub fn new(config: &VadConfig, sample_rate: u32) -> Result<Self, String>; // calls validate; sr must be in 8000..=96000
    pub fn push(&mut self, samples: &[f32]) -> Vec<VadEvent>;
}
```

`validate` must reject all of the following, each with its own message:
- `hop_ms == 0` or `hop_ms > 100`. The zero case fixes the infinite loop at `commands.rs:197`.
- `silence_ms < hop_ms`.
- `max_segment_ms <= silence_ms`.
- `max_recording_duration_secs > 3600` (moved from `update_vad_config`).
- Any threshold that is not finite or is outside `0.0..=1.0`.

Internals: convert every ms value to hops once in `new`, rounding up (`div_ceil`). Hop length in samples is `sr * hop_ms / 1000`. That gives 320, 882 and 960 samples at 16k, 44.1k and 48k, so the hop counts are the same at every sample rate. Timestamps come from the absolute sample index, `idx * 1000 / sr` (u64), not from hop counts.

State:
- A pending partial-hop `Vec<f32>`.
- `samples_seen: u64`.
- A pre-roll `VecDeque<f32>`.
- `Option<Active { buf, start_sample, last_speech_end_sample, speech_hops, silence_hops }>`.
- A hop counter for metrics. Metrics are throttled every 100ms of audio time, not wall-clock time, so the result is deterministic.

The cap: when the active segment reaches `max_segment_ms`, emit a `Segment` whose `end_ms` is the current position. If the current hop is still speech, start a new segment at the same sample without pre-roll. The stream ending while a segment is active does not flush it; the fixtures end with 2s of trailing silence.

**Step 1a is a faithful port.** The per-hop logic stays exactly as it is now: soft-knee gate, then `rms > sensitivity || peak > peak_threshold`, a silence counter, a min-speech check leading to `Discarded`, and tail trim to 150ms (`const TAIL_MS`). The only changes are that the units are now ms and that the timestamps are added.

## 2. `commands.rs`: thin loop

- Delete the body of `run_vad_capture` and replace it with:

  ```rust
  async fn run_vad_capture(app: AppHandle, stream: impl Stream<Item=f32>+Unpin, sr: u32, mut vad: Segmenter, config: &VadConfig)
    let mut chunks = stream.ready_chunks(4096);
    while let Some(chunk) = chunks.next().await { for ev in vad.push(&chunk) { match ev { ... } } }
  ```

  Map each event to the same IPC events as today:
  - `SpeechStart` becomes `"speech-start"`. No TS code listens to it; keep it anyway, because deleting unused events is issue 17's job.
  - `Segment` becomes normalize, then `samples_to_wav_b64`, then `"speech-detected"`. On an encode error, emit `"audio-encoding-error"`.
  - `Discarded` becomes `"speech-discarded"` with the existing message.
  - `Metrics` becomes a `VadMetrics` built from the event plus the thresholds in `config`. The IPC shape does not change.
- In `start_system_audio_capture`:
  - Call `config.validate()?` *before* writing it into `state.vad_config`. Today an invalid config is stored first.
  - When `enabled`, build the `Segmenter` inside the sync `open` closure (`Segmenter::new(&cfg, sr)?`), so a bad config is returned as `Err` before the task is spawned.
- In `update_vad_config`, replace the two ad-hoc checks with `config.validate()?`.
- Remove the `VecDeque`, `Duration`/`Instant` and metrics-throttle imports if nothing else uses them. Calibration still uses `VecDeque` and `Instant`.

`mod.rs`: add `pub mod vad;`. `lib.rs`: add `pub use speaker::vad;`, which is the only new public path, needed so `tests/` can reach it. Change `use speaker::VadConfig` to `use speaker::vad::VadConfig`.

## 3. TS rename (forced by the new serde shape)

- `useSystemAudio.ts`:
  - Interface: replace `hop_size`, `silence_chunks`, `min_speech_chunks`, `pre_speech_chunks` with `hop_ms`, `silence_ms`, `min_speech_ms`, `pre_speech_ms`, `max_segment_ms`.
  - `DEFAULT_VAD_CONFIG`: use the same values as the Rust `Default`.
  - localStorage key: change `"vad_config"` to `"vad_config_v2"` (lines 227 and 1086). An old saved config would otherwise fail serde on every start with "missing field hop_ms".
- `SettingsPanel.tsx`:
  - `handleResetDefaults`: use the same new fields.
  - Silence slider: display `(silence_ms/1000).toFixed(1)`s, range 500..4000, step 100. This also removes the 44100 assumption that issue 15 mentions.

## 4. Fixtures

- `flake.nix`: add `pkgs.espeak-ng pkgs.sox` to `devShells.default.packages`.
- `src-tauri/tests/fixtures/vad/gen.sh` (bash, `set -euo pipefail`, uses `mktemp -d` with a `trap` cleanup):
  1. `espeak-ng -v en-us -s 160 -w` for three one-sentence interviewer utterances (no commas). Use `-v en-gb+f3` for the second speaker's utterance.
  2. Trim each utterance: `sox in out silence 1 0.001 -60d reverse silence 1 0.001 -60d reverse norm -6`.
  3. Concatenate at 22050 Hz in this order: 1.0s silence, u1, 0.5s, u2, 1.5s, u3, 3.0s, u4 (speaker 2 after 2.0s), then 2.0s trailing silence.
  4. Write `pauses.truth`, one `start_ms end_ms` line per utterance, computed from `soxi -s` sample counts.
  5. Resample to 16000, 44100 and 48000 Hz with `sox -R -D ... -b 16 pauses_<rate>.wav rate -v <rate>`. `-D` turns off dither and `-R` keeps output repeatable; together they make the output deterministic.
- Commit the three WAVs (about 3MB in total) and `pauses.truth`. `*.wav` is already `binary` in `.gitattributes`.
- Hum and click variants are **not** committed. The test applies them as transforms of the decoded samples. They are exact and deterministic at every rate, and this keeps the repo 3x smaller.

## 5. Test: `src-tauri/tests/vad.rs`

This is data-driven and uses only the public interface: `Segmenter::new`, `push`, `VadEvent`.

- `fixtures()`: for each rate in `[16000, 44100, 48000]` and each variant in `[Clean, Hum, Clicks]`:
  1. Decode with `hound`, i16 to f32.
  2. Apply the variant:
     - `Hum`: add `0.03*sin(2π·50t) + 0.01*sin(2π·150t)`. That is about 0.022 RMS, above `sensitivity_rms`.
     - `Clicks`: add a 1ms burst at ±0.6 every 700ms inside every gap of at least 1s (lead-in, 1.5s, 3s, 2s and trailing), keeping 100ms clear of each edge.
  3. Push the samples through `Segmenter::new(&VadConfig::default(), rate)` in 4096-sample chunks.
  4. Collect `Segment` as `(start_ms, end_ms)` and count `Discarded`.
- Expected segments come from merging the truth intervals whose gap is under `silence_ms`. This oracle is 6 lines, and the pauses are far from the threshold. The result is 3 segments: [u1+u2], [u3], [u4].
- Assert each boundary is within ±50ms, `Discarded == 0`, and that every rate for a variant gives the same number of segments with boundaries within `hop_ms` of each other. Every failure message names (rate, variant).
- `config_rejected()`: a table of `(mutation, is_ok)`: hop 0, silence < hop, cap ≤ silence, a NaN threshold, sr 4000, and the default config (ok).
- `max_segment_cap()`: run the 48k clean fixture with `max_segment_ms: 1500`. Every segment must be at most 1500ms long, and capped pieces must be contiguous (`end == next.start`).

**Expected on the faithful port:** Clean passes. Hum fails because it never cuts. Clicks fails: the 1.5s pause merges into one segment and the lead-in click produces a `Discarded`. Commit this red state (fixtures plus test) before step 6, so the bug is on record.

## 6. Detector fixes, driven by the fixtures, all inside `vad.rs`

- **Clicks (time hysteresis):** `const MIN_RUN_MS: u32 = 60`. A hop counts as speech only once the current run of consecutive speech hops reaches `min_run_hops`. When the run qualifies, the onset is back-dated to the start of the run; the pre-roll ring holds `pre_hops + min_run_hops` hops. Short runs inside a segment count as silence and do not move `last_speech_end`. As a result, an isolated click never resets the silence counter and never starts a segment.
- **Hum (adaptive floor):** keep a ring of per-hop RMS values over `const FLOOR_WINDOW_MS: u32 = 2000`; `floor = min(ring)`. The effective thresholds are `max(sensitivity_rms, floor*2.0)` for RMS and `max(peak_threshold, floor*4.0)` for peak. Add a comment: `// ponytail: min-statistics floor; long gap-free speech (>window) raises threshold, switch to percentile/decaying floor if fixtures or users show early cuts`.
- **If offsets miss ±50ms** (TTS fricative tails under 0.012 RMS): add level hysteresis. Once in a segment, continue while above half of the start threshold. If Hum still cannot meet ±50ms, widen that variant's tolerance **visibly in the test table**; do not weaken the detector in secret.
- The `VadMetrics` IPC shape stays the same; the floor is not exposed.

## 7. Documentation

Add 4 lines to the `speaker/` section of `ARCHITECTURE.md`:
- `vad.rs` is a pure segmenter; config is in ms and validated.
- Events are forwarded by `run_vad_capture`.
- The spec is `tests/vad.rs` plus the fixtures, regenerated with `nix develop -c bash src-tauri/tests/fixtures/vad/gen.sh`.

## Verification

```
nix develop -c bash src-tauri/tests/fixtures/vad/gen.sh && git status --short src-tauri/tests   # second run must show no diff (determinism)
nix develop -c cargo test --manifest-path src-tauri/Cargo.toml --test vad
nix develop -c cargo test --manifest-path src-tauri/Cargo.toml
nix develop -c cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
nix develop -c npm run build
nix run .#dev   # manual: play speech with 0.5s/1.5s pauses into the monitored sink; check one speech-detected per >1s pause
```
(In a worktree use `nix develop path:. -c ...`.)

## Commit order

1. `vad.rs` faithful port, `commands.rs` thin loop, TS rename (app behaviour unchanged apart from the ms units).
2. `flake.nix`, generator, fixtures, and `tests/vad.rs` (Hum and Clicks red).
3. Detector fixes (green).
4. `ARCHITECTURE.md`.

Skipped: panicking or erroring on NaN samples inside `Segmenter`, and flushing a segment at end of stream. Add them when a backend is shown to produce NaN, or when issue 12 needs a flush on stop.
## Review amendments (orchestrator)
- The adaptive floor must not rise during speech: an interviewer talking continuously for more than 2s is the normal case, and it must never raise the threshold to the point of cutting the question off. Update the floor estimate only from hops classified as non-speech (or use a window of 10s or more), and add a fixture with at least 8s of continuous speech (espeak with no pauses) that asserts it stays one segment. If the adaptive floor can't pass that and the hum fixture at the same time, ship the faithful port plus click debouncing and record the failing hum case as a known limitation. Do not tune the detector to pass by widening tolerances beyond ±100ms.
- Issue 11 also adds vitest/jsdom this phase. Your changes don't need JS tests, so do not add JS test deps.
