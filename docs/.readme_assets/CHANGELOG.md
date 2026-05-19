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
