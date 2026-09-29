# 01 SSE stream corrupts multi-byte UTF-8 split across network chunks

`src-tauri/src/llm/stream.rs:40` does `buffer.push_str(&String::from_utf8_lossy(&bytes))` per `bytes_stream()` chunk. A code point split across two reads becomes two U+FFFD. Routine with emoji/CJK/accented text.

Failure: model streams "café" and the chunk boundary falls inside `é` → UI and DB get `caf��`.

Acceptance:
- Buffer raw bytes; decode only complete lines (split on `\n` in bytes). Invalid UTF-8 within a complete line is an error, not lossy.
- Also audit the rest of the SSE parser for: CRLF line endings, multi-line `data:` frames, `[DONE]`, trailing frame without newline.
- Test: feed the parser a byte stream split at every byte offset of a multi-byte payload; output identical to unsplit.
