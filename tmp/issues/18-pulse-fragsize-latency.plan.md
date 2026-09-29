## Issue 18: ~2s first-sample capture latency on PipeWire-pulse

### What causes it (checked in code)
`src-tauri/src/speaker/linux.rs`, `SpeakerInput::new`, calls `Simple::new(..., &spec, None, None)`. With `attr = None`, libpulse falls back to the server's default record buffering. libpulse's `pa_simple_new` always connects with `PA_STREAM_ADJUST_LATENCY`. That means an explicit `fragsize` directly sets the source latency, so a one-field change fixes the issue. The existing test has a comment at line 412 that already notes the symptom: `// default pa_simple fragsize delivers the first fragment after ~2s`.

The consumer side does not add delay. `capture()` reads 4096 bytes per `simple.read`, which is 1024 f32 mono samples, about 23ms at 44.1kHz. `SpeakerStream` hands samples out one at a time. So once Pulse sends fragments quickly, samples reach the consumer quickly. No change is needed in `capture`, `SpeakerStream` or `mod.rs`.

### Step 1: write the failing test first (`src-tauri/src/speaker/linux.rs`, `mod tests`)
Add one live-Pulse test next to `stream_ends_with_error_when_capture_is_killed`. It uses the existing `NullSink` fixture, so it does not touch the user's real devices:

```rust
#[tokio::test]
async fn first_sample_arrives_within_200ms() {
    let sink = NullSink::new();
    let started = std::time::Instant::now();
    let mut stream = SpeakerInput::new_with_device(Some(sink.name.clone())).unwrap().stream();
    stream.next().await.expect("stream alive");
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_millis(200), "first sample after {elapsed:?}");
}
```
- The clock starts before `new_with_device`, so the test measures the whole wait the user sees: connect, source lookup, stream open and first fragment.
- There is no `tokio::time::timeout` wrapper. The assertion is the check, and today the call returns after about 2s anyway. If it hangs, that is a separate bug and the test harness shows it.
- It goes through the public `crate::speaker::SpeakerInput` path that the other tests use, not internal functions.
- Run it before the fix and record the failing elapsed value (expected about 1.9–2s). This confirms the bug on the owner's machine.

### Step 2: the fix (`SpeakerInput::new`, linux.rs lines 115–131)
After building `spec`, build a `BufferAttr` and pass `Some(&attr)` as the last argument of `Simple::new`:

```rust
let attr = BufferAttr {
    maxlength: u32::MAX,
    tlength: u32::MAX,
    prebuf: u32::MAX,
    minreq: u32::MAX,
    fragsize: spec.usec_to_bytes(MicroSeconds(20_000)).try_into().unwrap(), // ~20ms keeps VAD decisions close to speech
};
```
- `u32::MAX` means "server default" for each field. `tlength`, `prebuf` and `minreq` are ignored for record streams.
- Imports: add `use pulse::def::BufferAttr;` and `use pulse::time::MicroSeconds;` next to the other `pulse::` imports. `Spec::usec_to_bytes(MicroSeconds) -> usize` exists in libpulse-binding 2.30.1.
- The value is 3528 bytes at 44100 Hz, 1 channel, f32. The `try_into().unwrap()` cannot fail; it only avoids a silent `as` cast.
- No new pub items, no config knob, no fallback. If the server rejects the attrs, `Simple::new` already returns an error, and the existing `map_err` passes it on.

### Step 3: remove the stale comment
In `stream_ends_with_error_when_capture_is_killed`, delete the comment at line 412 (`// default pa_simple fragsize delivers the first fragment after ~2s`). It is no longer true. Keep the 5s timeouts: they are hang guards, not latency checks.

### Verification
From the repo root (inside a worktree use `nix develop path:. -c ...`):
- `nix develop -c cargo test --manifest-path src-tauri/Cargo.toml speaker::linux -- --nocapture`: the new test fails before step 2 and passes after it. Write the before and after elapsed values in the commit message.
- `nix develop -c cargo test --manifest-path src-tauri/Cargo.toml`: run the full suite.
- `nix develop -c cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings`
- Manual check in the running app: play audio in system output and confirm that VAD / turn detection responds in well under a second.

### Scope
Only `src-tauri/src/speaker/linux.rs` changes. No sibling issue in phase 2 (08, 09, 10, 11) touches this file, so the phase stays file-disjoint. macOS and Windows code is not touched, and `linux.rs` is behind `#[cfg(target_os = "linux")]`, so their builds are unaffected.

Skipped: sizing the 4096-byte read buffer to match fragsize. It is already about 23ms. Change it only if measurements show per-read delay matters.