# Changelog

## Unreleased

### Changed
- **DB layer moved from TypeScript to Rust.** `tauri-plugin-sql` and the
  TypeScript `src/lib/database/` query layer are gone. SQLite is now owned by
  the Rust process via `rusqlite`, exposed as intent-named Tauri commands
  (`start_conversation`, `append_message`, `load_conversation`,
  `list_conversation_summaries`, `rename_conversation`, `delete_conversation`,
  `delete_all_conversations`, `list_system_prompts`, `create_system_prompt`,
  `edit_system_prompt`, `delete_system_prompt`).
- **Conversation/message IDs and timestamps are server-generated.** The
  frontend no longer mints IDs in the chat path; the `MESSAGE_ID_OFFSET` and
  `generate*Id` helpers are deleted.
- **History sidebar fetches summaries only.** Message bodies load on demand
  when a conversation is opened, replacing the previous "ship every message
  body to render the sidebar" pattern.
- **Per-turn persistence is append-only.** Each turn does two single-row
  `append_message` INSERTs instead of rewriting the messages table for the
  whole conversation.

### Removed
- **Pre-SQLite localStorage history migration is gone.** The
  `migrateLocalStorageToSQLite()` helper and the
  `chat_history_migrated_to_sqlite` flag check no longer run on startup. Users
  who upgrade directly from a pre-SQLite build lose any history that had not
  already been migrated. Existing `pluely.db` files written by
  `tauri-plugin-sql` are preserved; the new Rust layer stamps
  `PRAGMA user_version = 2` via a one-time legacy bridge and continues.
- `@tauri-apps/plugin-sql`, the `sql:default` / `sql:allow-execute`
  capabilities, and the matching Cargo dependency.

### Changed (LLM streaming + secrets)
- **LLM streaming moved from TypeScript to Rust.** Both Pluely-hosted and
  custom-provider chat now flow through one Rust command (`stream_chat`)
  using a per-invocation Tauri `Channel<T>`. The old global event bus +
  50 ms JS polling loop is gone; cancellation is explicit via
  `cancel_chat(request_id)`. The JS-side curl parser and outbound
  `fetch` for LLM traffic are gone.
- **API keys live in the OS keychain.** Custom-provider API keys, the
  Pluely license key, the Pluely instance ID, and the selected Pluely
  model are now stored via the OS keychain (Keychain on macOS,
  Credential Manager on Windows, libsecret on Linux). The settings UI
  no longer reads secret values back — it only knows whether a value
  is set, and lets the user replace or clear it. **No automatic
  migration is performed**: users upgrading from a previous version
  must re-enter their Pluely license / instance / selected model and
  any custom-provider API keys through the settings UI. The old
  `secure_storage.json` file on disk and stale plaintext values in
  `localStorage` are no longer read and can be safely removed.

### Removed (LLM streaming + secrets)
- The `secure_storage_save` / `secure_storage_get` / `secure_storage_remove`,
  `mask_license_key_cmd`, `chat_stream_response`, and `check_license_status`
  Tauri commands. License status is now `pluely_license_status`; the
  rest are replaced by the keychain / streaming surfaces above.
- `src/lib/functions/ai-response.function.ts` and the JS-side variable
  substitution / message-builder helpers it depended on.
