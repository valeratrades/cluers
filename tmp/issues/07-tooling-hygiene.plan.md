I found the root cause and confirmed it by running the tools. The whole fix is one line in `flake.nix` plus two doc rewrites.

# Plan: 07 Tooling / docs hygiene

## A. Why `cargo clippy` fails (confirmed)

Inside `nix develop` the tools don't all come from the same toolchain:
- `rustc -V` reports 1.95.0, from nix `rust-default-1.95.0`.
- `cargo-clippy -V`, found on PATH, reports 0.1.95, also from nix.
- `cargo clippy -V` reports **0.1.100 nightly (2026-09-05)**. Cargo looks for subcommands in `$CARGO_HOME/bin` before it looks in `PATH`. The user has a rustup proxy at `~/.cargo/bin/cargo-clippy`, and its default toolchain is nightly.

So `cargo clippy` runs nightly clippy-driver and nightly rustc. That nightly makes the never-type-fallback lint a hard error, and xcap 0.0.12 hits it (`src/linux/wayland_capture.rs:71,127`, `!: ReadAll`). `cargo test` does not go through a subcommand proxy, so it keeps using nix stable.

Checks I ran:
- `cargo-clippy clippy --all-targets` with the nix binary called directly exits 0. It reports 6 real lint warnings and one future-incompat warning for xcap.
- `RUSTUP_TOOLCHAIN=<nix rust store path> cargo clippy -V` reports 0.1.95. rustup 1.29 accepts a toolchain path here, so the rustup proxy forwards to the nix toolchain.

### Fix (one line, `flake.nix`)
In `devShells.default.env`, add:
```nix
RUSTUP_TOOLCHAIN = "${rust}"; # ~/.cargo/bin rustup proxies shadow PATH for cargo subcommands
```
- It pins every rustup proxy (cargo-clippy, cargo-fmt, rustfmt, clippy-driver) to the flake toolchain.
- It does not change `packages.default` or `apps.dev`: neither calls clippy, and the build sandbox has no `~/.cargo`.
- Keep the tail comment. It explains the non-obvious reason.

### Options I rejected
- **Bump xcap to 0.9.x.** It would silence the nightly error, but clippy would still run on the wrong toolchain, which is the actual bug. xcap 0.9 also changes the API: `width()`, `x()` and `is_primary()` return `XCapResult`. That means editing `src-tauri/src/capture.rs`, which issue 05 owns in the same phase and would conflict.
- **`rust-toolchain.toml`.** It duplicates the flake pin, drifts from `stable.latest`, and makes rustup download a second copy of the toolchain.
- **Setting `CARGO_HOME` per repo.** It re-downloads the registry and still misses the actual cause.

## B. Clippy output after the fix (report only, do not fix here)
- `src/lib.rs:85` unneeded `mut`
- `src/lib.rs:158` needless_borrow
- `src/db/commands.rs:59` redundant_closure
- `src/llm/provider.rs:69` collapsible_match
- `src/llm/provider.rs:385` items_after_test_module
- `src/speaker/commands.rs:324` explicit `.into_iter()`
- Future-incompat: `xcap v0.0.12` (never-type fallback). It becomes a hard error when stable picks up the nightly behaviour.

Why not fix them here: `llm/provider.rs` belongs to 02, `speaker/commands.rs` to 03/04 and `capture.rs` to 05, all in phase 1. Recommendation: after phase 1 merges, the orchestrator runs `cargo clippy --fix --all-targets` once, then hand-fixes `items_after_test_module`, before tagging v0.1.10 (or folds this into 17). The xcap bump goes to a follow-up owned by whoever holds `capture.rs` next, and 05's plan should mention it.

## C. `tmp/debug_sessions/keychain_platoform_secure_dbus_error.md`: rewrite it shorter

The following parts are out of date:
- "No caching" and "every chat message hits the keychain": `Secrets` in `llm/secrets.rs` is a write-through cache, so the keychain is read at most once per provider per run.
- The Pluely `selected_model` path and the `api.rs:83` STT path: `llm/pluely.rs::selected_model_get` now reads the SQLite `settings` table and does not touch the keychain at all.
- Failure mode 6 (reconnect churn) and the `set_provider_secret` = 3 connections point: now one JSON entry, one access.
- Failure mode 8 (sync D-Bus blocking tokio workers): keychain I/O is now in `spawn_blocking`.
- The `__names__` / `pluely.license` layout.
- The "VAD fires constantly" call chain.

Parts that still help with debugging:
- How the error string is built: dbus, then dbus-secret-service, then keyring `PlatformFailure`/`NoStorageAccess`, then `LlmError::Keychain` with `#[error("keychain: {0}")]` at `llm/mod.rs:47`.
- Failure modes 1-5 and 7, one line each: the environment condition and the error text.
- The `dbus-send ... NameHasOwner string:org.freedesktop.secrets` check, and the note that NixOS without GNOME usually has no secret service.
- Current touch points: `Secrets::provider/set/delete/delete_all` in `llm/secrets.rs`. The first custom-provider message after launch (`provider::stream_custom`) and settings edits are the only keychain accesses.

Rewrite the file to about 30-40 lines with sections Origin, Failure modes, Diagnose, When the keychain is touched. Drop the date/status header. Keep the filename (typo included) so existing links still work, or rename to `keychain_dbus_error.md`; renaming is the implementer's choice, and nothing references the file (checked with grep). Deleting the file was considered. I decided against it, because the error-origin map and the diagnostic command are the first things needed when a user reports "keychain: Platform secure storage failure".

## D. ARCHITECTURE.md is also out of date (found during the audit; same kind of problem)
In the `## src-tauri/src/llm/` section, **Secret storage** still describes:
- per-variable accounts,
- `pluely.license`/`selected_model`,
- the `pluely.meta` migration marker,
- the names-list entry,
- `list_provider_secret_names`.

The code now uses one entry per provider (service `pluely.provider.<id>`, account `secrets`, value a JSON map), cached in `Secrets`. `selected_model` lives in the SQLite settings table. There is no migration (see the `llm/secrets.rs` module doc and `llm/mod.rs:8-10`). Also check the `secrets.rs` line in the Layout block ("+ one-time legacy bridge") and fix it.

Rewrite only that subsection: replace the table with a single row and a single sentence about the cache. Check the JS command surface against `lib.rs` `invoke_handler` (`llm::commands::*secret*`) before writing the names.

Merge risk: issue 02 may edit the Errors/Streaming paragraphs of the same section. Both are text edits in different paragraphs, so merge by hand if they conflict.

## E. CLAUDE.md `docs/spec/`
Nothing to do. Mention in the report that the repo has no `docs/spec/` (`docs/` exists, without a spec dir) and no repo-level CLAUDE.md.

## Steps
1. `flake.nix`: add `RUSTUP_TOOLCHAIN = "${rust}";` in `devShells.default.env`.
2. Verify, from the repo root:
   - `nix develop -c cargo clippy -V` must print `clippy 0.1.95`.
   - `nix develop -c sh -c 'cd src-tauri && cargo clippy --all-targets'` must exit 0 and show exactly the warnings listed in B.
   - `nix develop -c sh -c 'cd src-tauri && cargo test'` must still pass.
   - `nix develop -c cargo fmt --version` must match the stable toolchain.
3. Rewrite `tmp/debug_sessions/keychain_platoform_secure_dbus_error.md` as in C.
4. Edit the ARCHITECTURE.md Secret storage subsection as in D.
5. Report: the root cause (A), the lint list with owner issues (B), the xcap follow-up, and E.

Tests: no Rust behaviour changes, so there is no failing test to add. The acceptance check is step 2, where the `cargo clippy -V` version check reproduces the bug and then confirms the fix.
## Review amendments (orchestrator)
- The ARCHITECTURE.md Secret storage rewrite may conflict with issue 02, which also edits ARCHITECTURE.md (the llm section). Only touch the Secret storage subsection.
- Leave the 6 clippy warnings; the orchestrator runs a clippy pass after merging phase 1.
