# `keychain: Platform secure storage ...` error

## Origin

`dbus` transport error → `dbus-secret-service` (`Dbus`/`Unavailable`/`Locked`/`NoResult`/`Prompt`) → `keyring` (`Locked`/`NoResult`/`Prompt` → `NoStorageAccess`: "Couldn't access platform secure storage: {err}"; everything else → `PlatformFailure`: "Platform secure storage failure: {err}") → `LlmError::Keychain`, `#[error("keychain: {0}")]` in `src-tauri/src/llm/mod.rs`.

## Failure modes

1. No session bus (`DBUS_SESSION_BUS_ADDRESS` unset: SSH, systemd service, container) → `Platform secure storage failure: DBus error: ...`
2. No Secret Service provider (`org.freedesktop.secrets` unowned: no gnome-keyring / kwallet / keepassxc) → `... DBus error: org.freedesktop.DBus.Error.ServiceUnknown: ...`
3. Keyring locked → `Couldn't access platform secure storage: SS error: prompt dismissed` (dismissed) or `... object locked` (headless, can't prompt)
4. No `default` collection (WSL, broken providers) → `Couldn't access platform secure storage: SS error: result not returned from SS API`
5. `libdbus-1.so` missing at runtime → `Platform secure storage failure: DBus error: ...` (dlopen-level)
6. `dbus-daemon` restarted mid-run → `... Connection was disconnected before a reply was received`

Dead ends: crypto/DH (we use `EncryptionType::Plain`), `BadEncoding`/`TooLong`/`Ambiguous` (our entries are short UTF-8 written by us).

## Diagnose

```sh
dbus-send --session --dest=org.freedesktop.DBus --print-reply \
  /org/freedesktop/DBus org.freedesktop.DBus.NameHasOwner \
  string:org.freedesktop.secrets
```

`boolean false` → mode 2. NixOS outside GNOME usually has no secret service running (and no `pam_gnome_keyring` unlock).

## When the keychain is touched

Only through `Secrets::{provider,set,delete,delete_all}` in `src-tauri/src/llm/secrets.rs` (write-through cache, keychain I/O in `spawn_blocking`). So: the first custom-provider message per provider after launch (`provider::stream_custom`), and settings edits of provider secrets. A failed load is not cached, so with a broken secret service every custom-provider message retries and errors. The Pluely-hosted chat/STT path never touches it (`selected_model` is in SQLite).
