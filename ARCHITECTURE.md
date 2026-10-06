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
`save_conversation`, `update_conversation` or rename: chat writes go only
through `append_turn`, which persists a user+assistant pair in one transaction
and creates the conversation when the id is null. The list/detail split is
enforced — `list_conversation_summaries` returns summaries (no message
bodies); `load_conversation` returns the full conversation on demand.

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
├── stt.rs           `stt::transcribe`: Pluely-hosted or custom curl template (-F / --data-binary / -d)
└── pluely.rs        Pluely-hosted path: /api/response config, user activity
```

### Streaming engine

- **Transport**: `tauri::ipc::Channel<StreamEvent>` passed as a command
  argument. Per-request, no global event bus, no polling. The channel
  is dropped when `stream_chat` returns.
- **Concurrency**: structured. `stream_chat` registers a
  `oneshot::Sender` in `LlmState.cancels` keyed by request id, then
  `tokio::select!`s between `complete(..)` and the receiver; dropping the
  future is the cancellation. The `stream_*` paths only see an `on_delta`
  callback, so the capture task streams through the same `complete`. No
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
  pick a transport. STT mirrors this: every caller goes through
  `stt::transcribe`; there is no STT command, the renderer never holds audio.

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

- `AudioState.capture: tokio::sync::Mutex<Option<Capture>>` is the single
  lifecycle state; `Capture` is `{task, control, record}`. Live capture =
  unfinished task; a task that ends on its own reads as idle. No separate flag.
  `system_audio_control` goes through `AudioState::control`: `Config`/`Prompt`
  into the live task, `Record` to the continuous recorder (`Err` in VAD mode).
- A capture lives for the whole session in both modes. In continuous mode
  `Record` start/send/discard only drive the recorder inside it; each recording
  is one turn (`Flush`), and the limit auto-sends. Stop drops everything:
  the recording and any in-flight STT/LLM.
- The VAD config is a `watch` in `AudioState`; `update_vad_config` reaches the
  live segmenters and `Turns`. `default_vad_config` is the only source of
  defaults. The hop is the constant `vad::HOP_MS`, so a live change never
  touches the stream state.
- `start` while live is `Err("Capture already running")`; `stop` is idempotent
  and aborts *and awaits* the task, so the stream is dropped and the device
  released before it returns. No sleep-based sequencing.
- The capture task is the one sanctioned `tokio::spawn`: it outlives the
  command, but its handle is owned by `AudioState` and always joined.
- Calibration holds the slot while sampling, so a concurrent start waits.
- `vad.rs` is a pure segmenter (no Tauri, no clocks): samples + sample rate in,
  `VadEvent`s out. `VadConfig` durations are in ms and validated before use, so
  behaviour is identical across sample rates. The spec is `tests/vad.rs` plus
  the fixtures, regenerated with `nix develop -c src-tauri/tests/fixtures/vad/gen.sh`.
- `turn.rs` is the pure turn machine (sans-IO: segments, transcripts, replies
  and the audio clock in; asks and `TurnEvent`s out). It decides when a turn
  closes, joins its fragments chronologically, carries SKIPped text into the
  next ask and orders history. The spec is `tests/turn.rs`.
- The mic is captured through Pulse (`@DEFAULT_SOURCE@` or a named source),
  next to the monitor. VAD mode requires it; there is no cpal. Continuous
  mode records system audio only.
- Push-to-talk (`speaker/push_to_talk.rs`) is the only other mic consumer
  and the renderer captures no audio. `record_push_to_talk` owns its own
  Pulse stream for the whole call (no spawn) and returns the transcript;
  `finish_push_to_talk` ends it. It is independent of the capture slot (a
  second Pulse stream would work), but the renderer ignores the shortcut
  while a capture runs: capture mode hides the completion row whose popover
  would show the answer.
- Both channels go through their own `Segmenter` in lockstep on one sample
  clock (the system sample count); interviewer events are fed first. A
  device that stops delivering for 2s while the other keeps going ends the
  capture with a `capture-error` naming it.
- `turn.rs` attributes speakers:
  - the interviewer's turn closes on `TURN_GAP_MS` silence, or when the user
    has mic speech after the echo tail (`BLEED_TAIL_MS` past the
    interviewer's last speech end);
  - user speech is never transcribed or asked;
  - interviewer segments shorter than `BACKCHANNEL_MS` are dropped while the
    user holds the floor;
  - echo is judged by timing against the system reference, not by AEC, so
    the user barging in mid-question still waits for `TURN_GAP_MS`.
- The attribution spec is the `tests/turn.rs` scenes, assembled from the
  `pauses` fixture (no extra WAVs).
- The capture task (`drive`) owns every STT and LLM future: one answer at a
  time, FIFO, never cancelled by a newer turn. Stopping the capture drops
  them all. Events reach the renderer over the `Channel<TurnEvent>` passed
  to each `start_system_audio_capture`.
- Session memory (history, SKIP carry) lives in the renderer and is seeded
  into each start; the renderer persists answered turns via `append_turn`.

## Global shortcuts

- Config source is the main renderer's localStorage. `useApp` (main-window
  root hook) sends it through `update_shortcuts` on mount; a failure is shown
  in the main bar and forwarded to `js_log`.
- `update_shortcuts` validates the whole config first (unknown action ids and
  unparsable keys reject it untouched), then each plugin registration owns its
  `Action` via `on_shortcut`. No lookup map, no re-parsing per press.
- Handlers run on the global-hotkey thread: errors are `tracing::error!`ed,
  never panicked (a panic there kills every shortcut for the session).
- Renderer listeners are mounted exactly once per webview, by
  `useGlobalShortcutListeners` in `useApp`. `useGlobalShortcuts` is only a
  stable callback registry.
- The move-window loop is a sanctioned `spawn`: its handle lives in
  `MoveWindowState` and is aborted on key release or re-registration.
- Linux: X11 key grabs (global-hotkey 0.7) plus a CLI trigger.
  `pluely --action <action_id>` is forwarded to the running instance by
  tauri-plugin-single-instance (D-Bus) and runs the same `run()`. This is the
  Wayland path: the GlobalShortcuts portal is missing on wlroots (sway) and
  Hyprland ignores its proposed triggers. Invalid argv exits 2 before startup;
  `move_window` needs key release, so it stays X11-only. Sway config (defaults
  from `src/config/shortcuts.ts`):

  ```
  bindsym ctrl+shift+d exec pluely --action toggle_dashboard
  bindsym ctrl+backslash exec pluely --action toggle_window
  bindsym ctrl+shift+i exec pluely --action focus_input
  bindsym ctrl+shift+m exec pluely --action system_audio
  bindsym ctrl+shift+a exec pluely --action audio_recording
  bindsym ctrl+shift+s exec pluely --action screenshot
  ```

## Overlay input region

The main window is larger than what it paints (GTK floors it at 200px; it
grows to 600px for popovers). On Linux only the painted parts take input:
`useApp` sends the rects of `[data-input-region]` elements and Radix
popper wrappers to `set_input_region`, which sets the GTK input shape.
Anything else painted in the main window must carry `data-input-region`,
or clicks on it fall through to the app below.
