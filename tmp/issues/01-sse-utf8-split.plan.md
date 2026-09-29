## Issue 01: SSE stream corrupts UTF-8 split across chunks. Implementation plan

**Scope:** only `src-tauri/src/llm/stream.rs`. Do not touch `llm/mod.rs`, `llm/commands.rs` or `llm/state.rs`, because issue 02 owns those in the same phase. No new `LlmError` variant is needed (see step 2), so `mod.rs` stays unchanged. Callers in `provider.rs:616` and `pluely.rs:359` keep the same `stream_sse` signature and need no edits.

### Root cause
`stream_sse` runs `buffer.push_str(&String::from_utf8_lossy(&bytes))` on each network chunk. If a chunk boundary falls inside a code point, each half becomes U+FFFD. No other place in the crate does SSE or lossy decoding (I grepped for `bytes_stream`, `from_utf8_lossy` and `text/event-stream`).

### Step 0: failing tests first (same file, `#[cfg(test)] mod tests` at the bottom of stream.rs)
The tests call the module's real interface, `stream_sse`, with no internal helpers. Every dependency can be built in-process:
- Response: `reqwest::Response::from(tauri::http::Response::new(reqwest::Body::wrap_stream(futures_util::stream::iter(chunks.into_iter().map(Ok::<_, std::io::Error>)))))`. reqwest 0.12 already has the `stream` feature. `tauri::http` re-exports http 1.x, the same version reqwest uses. `wrap_stream` keeps each item as its own frame, so `bytes_stream()` gives back exactly the chunk boundaries the test chose.
- Channel: `tauri::ipc::Channel::<StreamEvent>::new(move |body| { match body { InvokeResponseBody::Json(s) => events.lock().unwrap().push(s), InvokeResponseBody::Raw(_) => panic!() }; Ok(()) })`, with `events: Arc<Mutex<Vec<String>>>`.
- Cancel: `let (_tx, mut rx) = tokio::sync::oneshot::channel();`. Keep `_tx` alive, otherwise `rx` resolves as closed. (`_tx` is a test-local binding that holds the sender alive, not a name used to silence a warning. If the owner objects, name it `cancel_tx` and `drop(cancel_tx)` after the await.)
- Extractor: `|v| extract_by_path(v, "choices[0].delta.content")`.

One helper, `async fn run(chunks: Vec<Vec<u8>>) -> Result<(StreamOutcome, Vec<String>), LlmError>`, with `#[tokio::test]` cases. Keep them table-driven where possible:
1. **Split at every byte offset.** The body is `data: {"choices":[{"delta":{"content":"café 日本 🎉"}}]}\n\ndata: [DONE]\n\n`. For each `i in 1..body.len()`, run with `[body[..i], body[i..]]` and assert that `full_response` and the event list equal the unsplit run, which is `"café 日本 🎉"`. Also run one case with each byte in its own chunk. This test fails on the current code.
2. **Data-driven table** of `(input bytes, expected full_response or error)`:
   - CRLF: `data: {..."a"}\r\n\r\ndata: {..."b"}\r\n\r\n` gives `"ab"`.
   - Multi-line data frame: `data: {"choices":[{"delta":\ndata: {"content":"x"}}]}\n\n` gives `"x"`.
   - `event: foo`, `id: 1`, `: keepalive` and `retry: 10` lines are ignored.
   - `data: [DONE]` produces nothing.
   - Final frame with no trailing newline (`data: {...}` then EOF) is still delivered.
   - `usage` is captured from a frame that has `"usage":{...}`. Assert this on `StreamOutcome.usage`.
   - Invalid UTF-8 inside a complete data line (`data: {"choices":[{"delta":{"content":"\xff"}}]}\n\n`) gives `Err(LlmError::Json(_))`.
   - Malformed JSON in a complete frame (`data: {nope\n\n`) gives `Err(LlmError::Json(_))`.

Commit the tests first and confirm that case 1 and the error cases fail. Then make the change below.

### Step 1: rewrite the `stream_sse` loop to work on bytes
- Replace `buffer: String` with `buf: Vec<u8>` and add `data: Option<Vec<u8>>`, which holds the pending event's data field.
- On `Some(Ok(bytes))`: `buf.extend_from_slice(&bytes)`. Then walk the complete lines (`buf[start..].iter().position(|&b| b == b'\n')`), call `process_line(&buf[start..start+i], …)` on each, and finish with a single `buf.drain(..start)`. This is one drain per chunk, not one per line. Add no memchr dependency.
- At EOF: if `buf` is non-empty, `process_line(&buf, …)`. Then if `data` is `Some`, dispatch it. This keeps today's "provider omits trailing newline" behaviour. Strict SSE would discard an unterminated event, but the existing contract says providers do omit the newline.

### Step 2: split `process_line` into a line handler and an event dispatcher
- `fn process_line(line: &[u8], data: &mut Option<Vec<u8>>, <sink args>) -> Result<(), LlmError>`:
  - `let line = line.strip_suffix(b"\r").unwrap_or(line);` handles CRLF.
  - Empty line: `if let Some(d) = data.take() { dispatch(&d, …)?; }`.
  - `line.strip_prefix(b"data:")`: append the value to `data`. If `data` is already `Some`, push `b'\n'` first, as the SSE spec requires.
  - Any other line (`event:`, `id:`, `retry:`, `:` comment): ignore it. No decoding is needed.
- `fn dispatch(payload: &[u8], …) -> Result<(), LlmError>`:
  - `let payload = payload.trim_ascii();` (std, Rust 1.80+). If it is empty or `== b"[DONE]"`, return `Ok(())`.
  - `let parsed: serde_json::Value = serde_json::from_slice(payload)?;`. serde_json checks the UTF-8 itself and returns `serde_json::Error` on invalid bytes. The existing `LlmError::Json(#[from])` then covers both invalid UTF-8 and malformed JSON, so no new variant and no edit to mod.rs.
  - The usage and delta handling stays exactly as it is today.
- The functions stay private. Pass `channel`, `extract_delta`, `full_response` and `usage` as today. Do not add a sink struct: two private fns with the current argument list are enough.

### Step 3: fix the doc comments
- Rewrite the module doc: frames are events of one or more `data:` lines ended by a blank line; bytes are decoded only as complete JSON frames; invalid UTF-8 and malformed JSON are errors. Delete "malformed JSON … silently dropped (matches the JS behavior)".
- Delete the in-function comment "The provider may legitimately split a JSON object across multiple network chunks…". It is false, because only complete lines ever reach the parser.

### Verification
```
nix develop -c cargo test --manifest-path src-tauri/Cargo.toml llm::stream
nix develop -c cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
nix develop -c cargo test --manifest-path src-tauri/Cargo.toml
```
Manual check: stream a reply with emoji or CJK text from a custom OpenAI-compatible provider and from the Pluely-hosted path. Check that the UI and the stored DB message contain no U+FFFD.

### Deliberately skipped
- Bare-CR line endings (allowed by the SSE spec, but no LLM provider uses them). Add them if a provider needs them.
- A UTF-8 BOM at the start of the stream. Same reason.
- Stopping the read loop at `[DONE]`. The server closes the stream anyway.
## Review amendments (orchestrator)
- No `_tx` binding: name it `cancel_tx` and `drop(cancel_tx)` after the await (owner forbids underscore-prefixed names).
