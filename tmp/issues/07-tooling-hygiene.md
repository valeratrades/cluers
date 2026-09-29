# 07 Tooling / docs hygiene

- `cargo clippy --all-targets` (inside `nix develop`, from `src-tauri/`) fails compiling dependency `xcap`: `error[E0277]: the trait bound '!: ReadAll' is not satisfied` (never-type fallback). `cargo test` compiles fine, so it is toolchain/flag specific. Lint coverage is currently zero. Find root cause (xcap version bump, toolchain pin, or clippy invocation) and fix so clippy runs clean-or-with-real-warnings.
- `tmp/debug_sessions/keychain_platoform_secure_dbus_error.md` claims secrets are re-read from keychain per message with no caching; `src-tauri/src/llm/secrets.rs:28-51` now has an in-process cache. Update the doc to current state (compact; keep only what still guides debugging) or delete it if nothing remains relevant.
- `CLAUDE.md` global instructions reference `docs/spec/`; this repo has none. No action beyond noting in the report.
