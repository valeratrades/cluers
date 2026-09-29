
# Plan: Issue 04, system-audio stop/start race

## What is actually broken (traced)
- `AudioState` (`src-tauri/src/lib.rs:22-27`) holds two separate pieces of lifecycle state: `stream_task: Arc<Mutex<Option<JoinHandle>>>` and `is_capturing: AtomicBool`. Nothing keeps them consistent with each other.
- `stop_system_audio_capture` (`commands.rs:580-609`) does three things in order: `take()` + `abort()` without awaiting, then sleeps 300ms, then stores `is_capturing=false`, then sleeps 200ms. Two ways this leaks a capture:
  1. Two stops overlap. The unmount `invoke(stop)` at `useSystemAudio.ts:955` can land next to a `startCapture` stop→start. Stop#1 clears the flag, start#2 wins the CAS and spawns B, then stop#2 wakes up and clears the flag while B is still running. The next start stores C over B's handle without aborting it. B is orphaned and every utterance is emitted twice.
  2. A stop lands between the start's CAS (`:84`) and the handle store (`:166`). The stop finds `None`, sleeps, and clears the flag while the task runs. The next start orphans that task.
- `calibrate_vad_thresholds` checks the flag (`:706`) but holds nothing, so a start can open the device mid-calibration.
- In `:760`, `partial_cmp(..).unwrap_or(Equal)` swallows NaN.

## Design: one lock, and the JoinHandle is the state
```rust
// lib.rs
#[derive(Default)]
pub struct AudioState {
    capture: tokio::sync::Mutex<Option<JoinHandle<()>>>, // replaces stream_task + is_capturing
    vad_config: Arc<Mutex<VadConfig>>,                  // untouched (issue 15 owns)
}
```
- "Capturing" means `Some(handle)` and `!handle.is_finished()`. A task that ends on its own (EOF, max duration, the producer death from issue 03) is idle by definition. The self-clearing tail inside the spawned task goes away, and so does the CAS and rollback closure.
- `tokio::sync::Mutex` is needed because stop holds the lock while awaiting the aborted task. It is FIFO-fair, so concurrent IPC calls apply in arrival order.
- Awaiting the aborted handle guarantees the future and `SpeakerStream` were dropped. `linux.rs:436-448` `Drop` joins the producer thread, so the device is released before `stop` returns. That makes both sleeps unnecessary.
- Start while running returns `Err("Capture already running")`, same as today. The frontend always stops first, so no restart semantics are needed.
- Stop is idempotent: with `None` it does nothing.
- `tokio::spawn` stays. Justification: the capture outlives the command, and its handle is owned by `AudioState` and always aborted and awaited by `stop`, so it is not detached. This goes in ARCHITECTURE.md, not in a code comment.

## Steps

### 1. Red: make the old semantics testable (pure move, no behaviour change)
In `src-tauri/src/speaker/commands.rs`, add a private inherent impl. Private methods in this module are visible to the command fns and to its `tests` mod.
```rust
impl crate::AudioState {
    async fn start<F>(&self, open: impl FnOnce() -> Result<F, String>) -> Result<(), String>
    where F: std::future::Future<Output = ()> + Send + 'static;
    async fn stop(&self);
}
```
- First, port the existing logic into these methods unchanged: CAS on the flag, `open()`, spawn, overwrite the handle; stop = take/abort/sleep 300/flag=false. Make the two commands call them.
- Add the test (below). It must fail: an orphan stays live after the final stop.
- Do not commit the red state separately unless the owner wants that. Otherwise just record in the PR that it was observed failing.

### 2. Green: rewrite `AudioState` and the methods
- `lib.rs`: replace the fields as shown above. Remove `is_capturing`. Keep `use tokio::task::JoinHandle`.
- `commands.rs` impl:
```rust
impl crate::AudioState {
    /// Locks the capture slot; Err if a capture is live. A finished capture is reaped.
    async fn lock_idle(&self) -> Result<tokio::sync::MutexGuard<'_, Option<JoinHandle<()>>>, String> {
        let mut slot = self.capture.lock().await;
        if let Some(done) = slot.take_if(|t| t.is_finished()) { reap(done).await }
        if slot.is_some() { return Err("Capture already running".into()) }
        Ok(slot)
    }
    async fn start<F>(&self, open: impl FnOnce() -> Result<F, String>) -> Result<(), String>
    where F: Future<Output = ()> + Send + 'static {
        let mut slot = self.lock_idle().await?;
        *slot = Some(tokio::spawn(open()?));
        Ok(())
    }
    async fn stop(&self) {
        let mut slot = self.capture.lock().await;
        if let Some(task) = slot.take() { task.abort(); reap(task).await }
    }
}
async fn reap(task: JoinHandle<()>) {
    if let Err(e) = task.await {
        if e.is_panic() { std::panic::resume_unwind(e.into_panic()) } // Cancelled is our own abort
    }
}
```
  `reap` is a private free fn with 2 call sites, justified by the rule to fail fast on a panicked capture.
- `start_system_audio_capture` becomes `app.state::<AudioState>().start(|| { ...existing setup: store vad_config if given, open SpeakerInput, validate sr, clone config, clone app...; Ok(async move { if cfg.enabled { run_vad_capture(..).await } else { run_continuous_capture(..).await } }) }).await`.
  - Delete the CAS, the rollback closure, the `state_clone` store, and the self-clearing tail (`:147-169`).
  - Device opening now happens only after the slot is confirmed idle.
  - `run_vad_capture` and `run_continuous_capture` are not modified at all (issue 08 owns their internals).
- `stop_system_audio_capture` becomes `app.state::<AudioState>().stop().await; Ok(())`. It keeps the `Result<(), String>` signature, so the TS side does not change. Both sleeps are deleted.
- Delete the `capture-started` emit (`:143`) and the `capture-stopped` emit (`:605`). Grep shows no listener anywhere under `src/`.
- Delete the stale memory-ordering comment block (`:12-16`). `Arc`, `AtomicBool` and `Ordering` imports stay because `run_continuous_capture` still uses them.

### 3. Calibration
- Replace `:705-708` with `let idle = state.lock_idle().await?;`. The lock is held while sampling, so a concurrent start waits (FIFO) instead of fighting over the device.
- Right after the sampling loop add `drop(stream); drop(idle);` with the tail comment `// release device before admitting a start`.
- NaN: after the loop, return `Err("Audio source produced NaN samples")` if `floor_samples.iter().any(|r| r.is_nan())`. The data comes from the device, which is a trust boundary, so this is an explicit error, not a panic. Then use `floor_samples.sort_by(f32::total_cmp);` (no unwrap, no fallback).

### 4. Remove instead of fix
- `get_audio_sample_rate` (`:812-823`): it opens the default device, but it has **zero callers** in `src/`. Delete the fn and its `lib.rs:137` handler entry.
- `get_capture_status` (`:806-810`): also zero callers, and it would otherwise need rewriting. Delete it and `lib.rs:136`.
- Check with `grep -rn "get_audio_sample_rate\|get_capture_status\|is_capturing" src src-tauri/src`; it should return nothing.

### 5. ARCHITECTURE.md
Add a short `## src-tauri/src/speaker/ — capture lifecycle` section (3-5 lines):
- `AudioState.capture` is the single lifecycle state behind one `tokio::sync::Mutex`.
- A live capture is an unfinished handle.
- `stop` aborts and awaits (the stream is dropped and the device released before it returns).
- The capture task is the one sanctioned `tokio::spawn`, because its handle is owned and always joined.
- Calibration holds the slot.

## Test design (`#[cfg(test)] mod tests` at the bottom of `speaker/commands.rs`)
It tests through the lifecycle interface (`start`/`stop`), which is what the commands are thin shims over. It is data-driven and uses a fake `Stream<f32>`.
- `struct FakeStream { live: Arc<Counters>, remaining: Option<usize> }` with `Counters { now: AtomicUsize, max: AtomicUsize }`:
  - `new` does `now += 1; max = max(max, now)`.
  - `Drop` does `now -= 1`.
  - `poll_next` returns `Pending` forever when `remaining` is None. When it is `Some(n)`, it yields n samples and then `None`.
- The fake capture future is `async move { let mut s = stream; while s.next().await.is_some() {} }`. It is built inside the `open` closure so the stream is created only when start is admitted. No underscore bindings.
- Table-driven cases, each with an `Op` sequence plus expected per-op results and final `now`:
  - `[Start, Stop, Start]` gives `[Ok, Ok, Ok]`, now=1.
  - `[Start, Start]` gives `[Ok, Err]`, now=1, and `max==1` (the second open is never called).
  - `[Stop, Stop]` gives ok, now=0.
  - `[StartFinite(10), WaitFinished, Start]` gives `Ok, Ok`, now=1 (natural end reads as idle).
  - `[Start, Stop]` gives now==0 immediately after `stop` returns (no sleep: device released).
- Race case, `#[tokio::test(flavor = "multi_thread", worker_threads = 4)]`: 20 iterations. Each iteration `tokio::spawn`s `stop`, `stop` and `start(fake)` on an `Arc<AudioState>` and joins them. Start may return Err; ignore it only with the justification comment `// Err = lost the race to a live capture, which is allowed`. After the loop, `stop().await`. Assert `now == 0` and `max <= 1`. The old implementation fails this, because the orphan keeps `now >= 1`.
- These are tests, so spawning in them is fine.

## Verification
- `nix develop -c cargo test --manifest-path src-tauri/Cargo.toml speaker::` must be red after step 1 and green after step 2.
- `nix develop -c cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings`
- `nix develop -c cargo test --manifest-path src-tauri/Cargo.toml` (whole suite).
- `nix develop -c npx tsc --noEmit`. There are no TS changes; this confirms nothing referenced the deleted commands.
- Manual check on Linux: run the app, spam the listen hotkey and switch modes quickly. `pactl list source-outputs short` should show at most one app stream at any time and 0 after stop. Each utterance should be sent once.
- macOS/Windows: the changes are platform-agnostic (no cfg code touched), so they compile unchanged.

## Coordination
- Issue 03 (same phase) must not reintroduce a flag. With this design, "reset capture state on producer death" comes for free: the stream ends, `run_*` returns, the task finishes, and the slot reads as idle. 03 only needs to emit its `capture-error` event.
- Both issues touch `commands.rs`. 03 edits `:624-672`, 04 edits start/stop/calibrate and the removed getters. Land 04 first in phase 1, or rebase 03 on it. The only likely conflict is the spawned-future tail, if 03 emits there.
- Issue 08 later replaces `run_vad_capture`'s body. The `start(open)` closure signature is what it plugs into; it is untouched here.

## Review amendments (orchestrator)
- Before deleting `get_audio_sample_rate` / `get_capture_status`, grep the whole `src/` again to confirm there are zero invokes.
