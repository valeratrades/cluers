# Architecture

Living notes on cross-cutting structure. Per-feature detail belongs near the
code; this file records decisions that span modules.

## `src-tauri/src/db/` — SQLite layer

All persistence lives in Rust. The TypeScript side talks to it only through
Tauri commands; there is no direct SQL on the renderer.

### Layout

```
src-tauri/src/db/
├── mod.rs           Db struct, DbError, public exports
├── migrations.rs    run_migrations() + MIGRATIONS slice + legacy bridge
├── schema.rs        serde IPC types (camelCase on the wire)
├── queries.rs       pure-sync fn(&Connection) -> Result<T, DbError>; all SQL
├── commands.rs      #[tauri::command] async wrappers
└── migrations/      .sql files included via include_str!
```

- `queries.rs` is the only place SQL strings appear. Sync, no Tauri/async
  dependency in its signatures — directly testable against
  `Connection::open_in_memory()`.
- `commands.rs` is a thin async shim — no SQL.
- `schema.rs` is the shared IPC contract; the TS `src/lib/database/index.ts`
  mirrors the same types.

### Concurrency

Single `Arc<Mutex<rusqlite::Connection>>` held in `tauri::State`. Each
`#[tauri::command]` is `async`, clones the `Arc`, and runs its query inside
`tokio::task::spawn_blocking(...).await`. Structured concurrency: every
blocking task is awaited at the call site, no detached work, no actor task,
no pool. Mutex poisoning panics (fail-fast).

### Migrations

Hand-rolled, tracked via `PRAGMA user_version`. The `MIGRATIONS` slice is a
sequence of `include_str!`'d SQL files; `run_migrations` applies any with
version greater than the current `user_version`.

A one-time **legacy bridge** detects the `_sqlx_migrations` table left by the
previous `tauri-plugin-sql` deployment: when present alongside
`user_version == 0`, it stamps `user_version = 2` and continues without
re-running the migrations (the schema is already in place). The
`_sqlx_migrations` table is left untouched for forensic value.

### Command surface

Commands are named after intent, not SQL CRUD. There is no
`save_conversation` or `update_conversation`: the frontend
`start_conversation`s once and `append_message`s per turn. The list/detail
split is enforced — `list_conversation_summaries` returns summaries (no
message bodies); `load_conversation` returns the full conversation on demand.

### Errors

`DbError` is a `thiserror` enum (`Sqlite`, `ConversationNotFound`,
`SystemPromptNotFound`, `InvalidInput`, `AttachedFilesJson`) with a manual
`serde::Serialize` impl that emits `self.to_string()`. Validation rejects the
whole batch — no silent row skipping.

### IDs and timestamps

Generated in Rust. Conversation IDs are uuid v4. Message timestamps are
`max(now_ms(), prev_max_for_conv + 1)` computed atomically inside
`append_message` — replaces the previous TS-side `MESSAGE_ID_OFFSET`
ordering hack.

## `src-tauri/src/llm/` — LLM streaming + provider secrets

All LLM HTTP traffic and all API-key storage live in Rust. The TypeScript
side talks to it through one streaming command and a small set of secret
helpers; the renderer never sees a secret value after it's been set.

### Layout

```
src-tauri/src/llm/
├── mod.rs           LlmState (reqwest::Client + cancel registry); LlmError
├── commands.rs      #[tauri::command] surface
├── secrets.rs       keyring-rs wrappers + `Secrets` write-through cache
├── provider.rs      curl parsing, variable substitution, message builder
├── stream.rs        SSE chunking and `responseContentPath` extraction
├── stt.rs           `transcribe`: Pluely-hosted or custom curl template (-F / --data-binary / -d)
└── pluely.rs        Pluely-hosted path: /api/response config, user activity
```

### Streaming engine

- **Transport**: `tauri::ipc::Channel<StreamEvent>` passed as a command
  argument. Per-request, no global event bus, no polling. The channel
  is dropped when `stream_chat` returns.
- **Concurrency**: structured. `stream_chat` registers a
  `oneshot::Sender` in `LlmState.cancels` keyed by request id, then
  `tokio::select!`s between the streaming future and the receiver. No
  detached `tokio::spawn` / `tauri::async_runtime::spawn`. The
  registration is an RAII guard owning the id until the stream exits
  (even after cancel), so a duplicate in-flight `request_id` is rejected
  with `DuplicateRequestId`. `cancel_chat(request_id)` fires the sender;
  it is an idempotent no-op for unknown or finished ids.
- **Termination**: the channel carries `Chunk`s then `Done`; failures
  are the command's `Err` (the renderer listens to the invoke rejection).
- **HTTP**: a single `reqwest::Client` lives in `LlmState`. SSE bodies
  are parsed via `bytes_stream()` + newline buffering; deltas are
  extracted with the provider's `response_content_path` JSON path.
- **One command for both paths.** Pluely-hosted vs custom is an
  internal branch on `provider.is_pluely_hosted`; the renderer doesn't
  pick a transport. STT mirrors this: `transcribe` (non-streaming) routes
  through `stt::transcribe`, which in-process callers use directly.

### Secret storage

`keyring-rs` v3 (Keychain on macOS, Credential Manager on Windows,
libsecret on Linux). One entry per (kind, provider) — built-in ids such as
`groq` exist in both kinds:

| Domain               | Service                             | Account   | Value                    |
|----------------------|-------------------------------------|-----------|--------------------------|
| AI provider secrets  | `pluely.provider.<provider_id>`     | `secrets` | JSON map `{name: value}` |
| STT provider secrets | `pluely.stt-provider.<provider_id>` | `secrets` | JSON map `{name: value}` |

`Secrets` (in `LlmState`) is a write-through cache, so the keychain is read
at most once per provider per run. The Pluely `selected_model` is a
preference and lives in the SQLite `settings` table. The JS surface is
set / list-names / delete / delete-all, each scoped by `kind: "ai" | "stt"`;
secret values are never exposed to the renderer.

### Errors

`LlmError` is a `thiserror` enum (`Reqwest`, `Keychain`,
`MissingVariable`, `InvalidCurl`, `PluelyUnlicensed`, `PluelyConfig`,
`ProviderApi { status, body }`, `CurlParse`, `Json`, `PluelyStt`,
`SttResponse`, `DuplicateRequestId`, `Cancelled`) with
a manual `serde::Serialize` impl emitting `self.to_string()`. The
previous fire-and-forget `report_api_error` spawns are awaited inline.

## `src-tauri/src/speaker/` — capture lifecycle

- `AudioState.capture: tokio::sync::Mutex<Option<JoinHandle<()>>>` is the
  single lifecycle state. Live capture = unfinished handle; a task that ends
  on its own reads as idle. No separate flag.
- `start` while live is `Err("Capture already running")`; `stop` is idempotent
  and aborts *and awaits* the task, so the stream is dropped and the device
  released before it returns. No sleep-based sequencing.
- The capture task is the one sanctioned `tokio::spawn`: it outlives the
  command, but its handle is owned by `AudioState` and always joined.
- Calibration holds the slot while sampling, so a concurrent start waits.
- `vad.rs` is a pure segmenter (no Tauri, no clocks): samples + sample rate in,
  `VadEvent`s out. `VadConfig` durations are in ms and validated before use, so
  behaviour is identical across sample rates. `run_vad_capture` only forwards
  events to IPC. The spec is `tests/vad.rs` plus the fixtures, regenerated with
  `nix develop -c src-tauri/tests/fixtures/vad/gen.sh`.
