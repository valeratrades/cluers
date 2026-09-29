## Plan for issue 02: LLM cancel registry

### What is broken (verified against the code)
1. `src-tauri/src/llm/commands.rs:62`: `cancels.insert(request_id, cancel_tx)` overwrites whatever is already under that id. The first stream's sender is dropped, so it can no longer be cancelled. The removal at `:109` then deletes by id, so whichever of the two streams finishes first removes the other one's entry.
2. There is a second overwrite path the issue does not mention. `cancel_chat` removes the entry as soon as it fires, but the cancelled stream keeps running until its `select!` notices. If a new `stream_chat` reuses that id in this window, it inserts, and then the old stream's cleanup at `:109` removes the new entry. Stopping duplicates at insert time does not close this hole. The id has to stay reserved until the owning stream exits.
3. `src/lib/llm/index.ts:136`: `await pump.catch(() => {})`. The issue says this makes the caller see an empty response. Worse: while the loop is still running, nothing watches `pump`. If the invoke rejects without first sending a channel event (IPC or deserialize failure, or the new duplicate-id rejection), the generator waits on `pending` forever. The caller hangs; it never sees an empty result.
4. `StreamEvent::Error` exists only because of (3). Every Rust error path already returns `Err(msg)` from the command. Once TS listens to the invoke rejection, the channel `Error` event duplicates it and can be deleted.

### Decision: what cancelling an unknown id does
`cancel_chat` of an unknown, finished or already-cancelled id is a **defined no-op**, and the command stays infallible (it returns `()`). Reason: cancelling after the stream finished is the normal case (unmount cleanup, cancelling the previous id before a new submit). If that were an error, every caller would have to catch it, which is exactly the swallowing pattern this issue removes. On the TS side, `cancelChat` can then only reject on an IPC-level bug, so the docs say not to catch it. Callers in 09/12 drop their `.catch(() => {})` and use `await cancelChat(id)` (or `void cancelChat(id)` in sync cleanup).

### Step 1: registry type (`src-tauri/src/llm/state.rs`)
Replace `pub cancels: Mutex<HashMap<String, oneshot::Sender<()>>>` with a private field `cancels: Cancels` and add:

```rust
#[derive(Default)]
pub struct Cancels(Mutex<HashMap<String, Option<oneshot::Sender<()>>>>); // None = cancelled, id still owned by a live stream

pub struct Registration<'a> {
    cancels: &'a Cancels,
    id: String,
    pub rx: oneshot::Receiver<()>,
}

impl Cancels {
    pub fn register(&self, id: String) -> Result<Registration<'_>, LlmError> // Entry::Occupied => Err(LlmError::DuplicateRequestId(id))
    pub fn cancel(&self, id: &str) // unknown or None => no-op; Some(tx) => take() and send while holding the lock
}

impl Drop for Registration<'_> {
    fn drop(&mut self) { lock; remove(&self.id).expect("registration owns its entry"); }
}
```
- Inside `cancel`, replace the swallowed `let _: Result<(), ()> = tx.send(())` with `tx.send(()).expect("receiver outlives its registry entry")`. This is safe because the send happens under the lock, the entry exists, and `Registration::drop` removes the entry before its `rx` field is dropped. So the receiver is always alive and the old race comment in `cancel_chat` can be deleted.
- Keep the existing `.expect("... mutex poisoned")` style.
- `LlmState` exposes the registry as a field with `pub(crate)` visibility (`pub(crate) cancels: Cancels`), so there is no new pub surface outside the crate. `LlmState::new()` sets it with `Cancels::default()`.
- Map value is `Option<Sender>` instead of the sender itself. A cancelled id stays reserved until its stream drops the guard. This closes hole (2) without switching to `Notify`, which would change the `&mut oneshot::Receiver<()>` signatures in `stream.rs`, `provider.rs` and `pluely.rs`. `stream.rs` is owned by issue 01 in the same phase.

### Step 2: error variant (`src-tauri/src/llm/mod.rs`)
- Add `#[error("request id already in flight: {0}")] DuplicateRequestId(String)` to `LlmError`.
- Delete `StreamEvent::Error`. Change the doc comment to "zero or more `Chunk`s, terminated by `Done`; failures are the command's `Err`".
- Update the module doc bullet on cancellation to say: duplicate ids are rejected; cancel is idempotent.

### Step 3: commands (`src-tauri/src/llm/commands.rs`)
- `stream_chat`: replace lines 55-63 with `let mut reg = state.cancels.register(request.request_id.clone()).map_err(|e| e.to_string())?;`. This early return is now fine because TS surfaces the invoke rejection. Keep `let request_id = request.request_id.clone();` for the `Done` payload.
- Pass `&mut reg.rx` in place of `&mut cancel_rx` to `stream_pluely` / `stream_custom`.
- Delete the manual removal block at `:104-110`. The `reg` guard drops at function end. If you want the entry freed before the final `channel.send`, call `drop(reg)` right after `result` is computed; either placement is fine.
- Delete the sentence "Errors must flow through `result` — an early return would leave the JS-side generator waiting on the channel forever." It is no longer true.
- `Err(e)` arm: just `Err(e.to_string())`, with no channel send.
- `cancel_chat`: body becomes `state.cancels.cancel(&request_id)`, with the race comment removed.
- Remove imports that become unused (`oneshot`, possibly `HashMap` is still needed by `ProviderInput`).

### Step 4: TS (`src/lib/llm/index.ts`)
- `StreamChunk` loses the `error` variant. Add a local `type Msg = StreamChunk | { kind: "failed"; error: unknown };`.
- Write the channel callback and the invoke rejection into the same queue:
```ts
const deliver = (msg: Msg) => { /* existing pending/queue logic */ };
channel.onmessage = deliver;
invoke("stream_chat", { request, channel }).catch((error) => deliver({ kind: "failed", error }));
```
  In the loop, `failed` becomes `throw new Error(String(error))` (Tauri rejects with the serialized string).
- Delete the `try/finally` and `await pump.catch(() => {})`. If the consumer stops iterating early, a later rejection lands in a queue that nobody reads. That is intentional because the consumer abandoned the stream. Justify it with one tail comment on the `.catch` line.
- Side effect: `break` out of `for await` no longer blocks until the Rust stream finishes (it used to await `pump`).
- `cancelChat` doc: "Idempotent: unknown/finished ids are a no-op. Rejects only on IPC failure (a bug) — do not catch." Signature unchanged.
- Update the stale `streamChat` doc sentence "Rust returns the requestId synchronously after registering..."; it was never true.

### Step 5: docs (`ARCHITECTURE.md`)
In the LLM "Concurrency" bullet, add: duplicate `request_id` is rejected with `DuplicateRequestId`; `cancel_chat` is an idempotent no-op for unknown or finished ids; the registration is an RAII guard that owns the id until the stream exits. Remove "Error" from the channel description. Add `DuplicateRequestId` to the error list.

### Tests (write them first, in `src-tauri/src/llm/state.rs` under `#[cfg(test)] mod tests`)
Data-driven, through the registry interface that both commands route through:
```rust
fn check(ops: &str, expect: &str)
```
- `ops` is a `;`-separated script: `reg a`, `drop a`, `cancel a`, `poll a` (`poll` does `rx.try_recv()` and reports `fired` / `idle`).
- The harness keeps live registrations in a `HashMap<&str, Registration>`, appends one trace token per op, and asserts the joined trace equals `expect`.

Cases:
- Duplicate: `reg a; reg a; cancel a; poll a` gives `ok; err duplicate; -; fired`. The first stream is still cancellable.
- Cancel after complete: `reg a; drop a; cancel a; reg a; poll a` gives `ok; -; -; ok; idle`. A stale cancel is a no-op and does not hit the reused id.
- Reuse while a cancelled stream winds down: `reg a; cancel a; reg a; drop a; reg a; poll a` gives `ok; -; err duplicate; -; ok; idle`.
- Unknown id / double cancel: `cancel x; reg a; cancel a; cancel a` gives `-; ok; -; -` with no panic.

Honest note: before Step 1 these fail by not compiling, not by asserting. A real failing test against the current code would need the Tauri IPC harness (see trade-offs).

### Verification
- `nix develop -c cargo test --manifest-path src-tauri/Cargo.toml llm::state`
- `nix develop -c cargo test --manifest-path src-tauri/Cargo.toml`
- `nix develop -c cargo build --manifest-path src-tauri/Cargo.toml`
- `nix develop -c npx tsc --noEmit`: the hooks still type-check. They never matched `kind: "error"` and `cancelChat`'s signature is unchanged.
- `grep -rn "StreamEvent::Error\|cancels.insert\|pump.catch" src-tauri/src src/lib` returns nothing.
- Manual: `nix develop -c npm run tauri dev`; start a custom-provider chat and press stop mid-stream, and the stream stops. Submit twice quickly; each submit cancels the previous and the new one streams. Bad curl gives an error toast instead of a hang.

### Out of scope / handoff
- Callers `.catch(() => {})` in `useCompletion.ts`, `useChatCompletion.ts` (issue 09) and `useSystemAudio.ts` (issue 12): replace with `await cancelChat(id)` / `void cancelChat(id)`.
- Not touching `stream.rs` (issue 01).