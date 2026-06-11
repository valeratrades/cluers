# `keychain: Platform secure storage ...` error — analysis

> **Date**: 2026-06-11
> **Branch**: `riir`
> **Status**: analyzed (no code changed)

## Origin

Error string composed from two crates and our code:

| Layer | File | What it contributes |
|-------|------|---------------------|
| `dbus` | dbus crate | Raw D-Bus transport errors (`"Unable to autolaunch..."`, `"ServiceUnknown"`, etc.) |
| `dbus-secret-service` 4.1.0 | `~/.cargo/registry/.../dbus-secret-service-4.1.0/src/error.rs` | Maps D-Bus errors → `Dbus`, `Unavailable`, `Locked`, `NoResult`, `Prompt` |
| `keyring` 3.6.3 | `~/.cargo/registry/.../keyring-3.6.3/src/error.rs:61-86` | `PlatformFailure` → `"Platform secure storage failure: {err}"`, `NoStorageAccess` → `"Couldn't access platform secure storage: {err}"` |
| Our code | `src-tauri/src/llm/mod.rs:45` | `#[error("keychain: {0}")]` — prepends `"keychain: "` |

The `keyring` crate's `secret_service.rs:581-588` maps underlying `secret-service` errors:
- `Locked`, `NoResult`, `Prompt` → `ErrorCode::NoStorageAccess(…)`
- Everything else → `ErrorCode::PlatformFailure(…)`

## Every chat message hits the keychain

| Path | File:line | Keychain operation | DBus connections created |
|------|-----------|-------------------|--------------------------|
| Pluely chat | `pluely.rs:238` | `pluely_selected_model_get()` → `read_opt()` → `Entry::new()` + `get_password()` | 1 |
| Pluely chat (success) | `pluely.rs:196` | `pluely_selected_model_get()` again (in `user_activity`) | 1 |
| Pluely chat (error) | `pluely.rs:148` | `pluely_selected_model_get()` again (in `report_api_error`) | 1 |
| Custom provider | `provider.rs:393-396` | `list_provider_secret_names()` + N× `get_provider_secret()` | 1 + N |

**Every single keychain call creates a fresh D-Bus session connection.** The `keyring` crate's `secret_service.rs` calls `SecretService::connect()` on every `get_password()`/`set_password()`/`delete_credential()`, which calls `Connection::new_session()`. Nothing is pooled. Nothing is cached.

This is well-known to the `keyring` authors — from `keyring-3.6.3/src/lib.rs:176-181`:

> *for RPC-based credential stores such as the dbus-based Secret Service, accesses from multiple threads (and even the same thread very quickly) are not recommended, as they may cause the RPC mechanism to fail*

## Failure modes

### 1. No D-Bus session bus (`DBUS_SESSION_BUS_ADDRESS` unset)

When: SSH sessions, systemd services, cron, containers without D-Bus socket, headless contexts without a display manager.

Error: `"keychain: Platform secure storage failure: DBus error: ..."`

### 2. No Secret Service provider

`org.freedesktop.secrets` not owned on the session bus. When: no `gnome-keyring-daemon`, no `kwalletd`+secret-service, no `keepassxc`+secret-service, minimal WMs without autostart.

Error: `"keychain: Platform secure storage failure: DBus error: org.freedesktop.DBus.Error.ServiceUnknown: ..."`

### 3. Keyring daemon locked

gnome-keyring login keyring locked (screen lock, never unlocked, headless with no unlock possible).

- Prompt dismissed → `"keychain: Couldn't access platform secure storage: SS error: prompt dismissed"`
- Can't prompt (headless) → `"keychain: Couldn't access platform secure storage: SS Error: object locked"`

### 4. No default collection

No collection with alias `default` in the secret service. Known to happen on WSL and with broken/misconfigured secret-service providers.

Error: `"keychain: Couldn't access platform secure storage: SS error: result not returned from SS API"`

### 5. `libdbus-1.so` not available

The `dbus` crate links `libdbus` via FFI. Missing shared lib at runtime.

Error: `"keychain: Platform secure storage failure: DBus error: ..."` (dlopen-level)

### 6. Rapid reconnection overwhelming D-Bus

Our code hits the keychain in tight loops (e.g., `set_provider_secret` = 3 separate DBus connections; `stream_custom` = 1 + N connections). Each opens a new DBus connection, authenticates, negotiates session. This churns the D-Bus daemon and the secret service, exactly the pattern the keyring docs warn against.

Error: Various D-Bus errors (connection refused, message too large, timeout, disconnect)

### 7. D-Bus daemon restart mid-operation

If `dbus-daemon` restarts (e.g., during package upgrades), connections established before the restart break.

Error: `"keychain: Platform secure storage failure: DBus error: Connection was disconnected before a reply was received"` (or similar)

### 8. Sync DBus blocking tokio worker threads

All `llm/commands.rs` entrypoints are `async fn` on tokio. `keyring` with `sync-secret-service` uses `dbus::blocking::Connection`, which blocks the worker thread for every round-trip. If the D-Bus daemon or secret service is slow (or needs user interaction for unlock), the worker is blocked and other tasks stall.

The keyring docs warn in `secret_service.rs:49-54` about deadlock with tokio — that's for `async-secret-service`, but the underlying mix of sync DBus + async runtime is risky regardless.

### 9. Secret service connection timeout

Not currently configured — we don't use `connect_with_max_prompt_timeout`. If the secret service hangs (e.g., waiting for user to unlock keyring interactively), the call blocks indefinitely on the tokio worker.

## Not relevant / dead ends (don't investigate these)

- **Crypto/DH failure**: We use `EncryptionType::Plain` (no `crypto-rust` or `crypto-openssl` feature on the `keyring` dep). DH key exchange failures don't apply.
- **`BadEncoding` error**: Only triggered when an existing secret was written as raw bytes by a third party and we try to decode it as UTF-8. Our code always writes UTF-8 strings.
- **`TooLong` / `Invalid` attribute errors**: Our service/account strings are short (`pluely.provider.<id>`, `__names__`, `pluely.license`) — unlikely to exceed platform limits.
- **`Ambiguous` errors**: We write new entries with explicit attributes in the default collection, matching our own search attributes. Third-party ambiguity is possible but rare; our own usage won't cause it.
- **Migration code**: The `tmp/secret-migration/` code was excised and is no longer compiled. Not a source of current errors.

## Architectural observations (for decision-making)

1. **No caching**: Secrets are re-read from the keychain on every chat message. A single in-memory cache (or `OnceCell`-per-provider) would cut the keychain calls from "every message" to "on settings change."

2. **No connection reuse**: The `keyring` crate itself provides no connection pooling. Any fix that keeps using `keyring` for the secret-service path must either:
   - Add a caching layer above `keyring`
   - Switch to `linux-native` (`keyutils`) feature for a kernel-backed store that doesn't need D-Bus
   - Use `linux-native-sync-persistent` feature (combo of keyutils + secret-service) which the keyring docs say makes secret-service the fallback

3. **Linux specifically**: On macOS, `apple-native` uses the Security framework directly (no D-Bus). On Windows, `windows-native` uses the Credential Store API (no D-Bus). Only Linux hits this D-Bus path. A Linux-only fix could use `linux-native` (kernel key retention service via `keyctl`) which requires no D-Bus and no daemon.

4. **`set_provider_secret` does 3 DBus connections**: One `write()` + one `read_names()` + one `write_names()`. These could be done in a single connection if we move the names-list logic into an atomic write or use a different storage scheme.
