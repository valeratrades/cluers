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
├── secrets.rs       keyring-rs wrappers + one-time legacy bridge
├── provider.rs      curl parsing, variable substitution, message builder
├── stream.rs        SSE chunking and `responseContentPath` extraction
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
  `cancel_chat(request_id)` command pulls the sender out of the map
  and fires it.
- **HTTP**: a single `reqwest::Client` lives in `LlmState`. SSE bodies
  are parsed via `bytes_stream()` + newline buffering; deltas are
  extracted with the provider's `response_content_path` JSON path.
- **One command for both paths.** Pluely-hosted vs custom is an
  internal branch on `provider.is_pluely_hosted`; the renderer doesn't
  pick a transport.

### Secret storage

`keyring-rs` v3 (Keychain on macOS, Credential Manager on Windows,
libsecret on Linux). Namespacing:

| Domain                  | Service                          | Account                            |
|-------------------------|----------------------------------|------------------------------------|
| Provider secrets        | `pluely.provider.<provider_id>`  | `<UPPERCASE_VAR_NAME>`             |
| Pluely license          | `pluely.license`                 | `license_key` / `instance_id`      |
| Pluely selected model   | `pluely.license`                 | `selected_model` (JSON)            |
| Migration marker        | `pluely.meta`                    | `keychain_migrated_v1`             |

The JS surface is set/list-names/delete only — `get_provider_secret` is
not exposed to the renderer. A names-list helper keychain entry per
provider lets `list_provider_secret_names` work without relying on a
platform-specific "list items by service" call.

### Errors

`LlmError` is a `thiserror` enum (`Reqwest`, `Keychain`,
`MissingVariable`, `InvalidCurl`, `PluelyUnlicensed`, `PluelyConfig`,
`ProviderApi { status, body }`, `CurlParse`, `Json`, `Cancelled`) with
a manual `serde::Serialize` impl emitting `self.to_string()`. The
previous fire-and-forget `report_api_error` spawns are awaited inline.
