# tmp/issues: stabilization backlog

The 2026-09-29 stabilization pass (Linux only) closed issues 01-19 in five phases, tagged `v0.1.10`..`v0.1.14` on branch `riir`. Their issue and plan files were deleted once implemented. To find what a past issue did, use `git log --grep "issue/NN"` or read the file at the tag.

## Open

| # | Issue | Blocked on |
|---|---|---|
| 20 | Keyboard focus stuck in pluely | Owner repro on sway |
| 21 | Push-to-talk recorded in Rust | Nothing, ready to plan |

## Known limits (accepted trade-offs, not scheduled)

Each needs a decision before it becomes an issue.

- **Barge-in.** If you talk over the interviewer mid-question, it doesn't count as taking the turn. `turn.rs` judges echo by timing, and real detection needs an echo canceller (e.g. webrtc AEC). Until then the 2s `TURN_GAP_MS` ends the turn.
- **User speech is never transcribed.** The LLM history lacks what you answered, which saves STT cost. Flip it if follow-up answers need that context.
- **Answers run one at a time and are never cancelled.** A follow-up question waits for the current answer to finish.
- **Startup shortcut gap.** Shortcut config lives in webview localStorage, so presses before the webview boots are dropped. Fixing it means moving the config to SQLite.
- **Wayland shortcuts.** On Wayland, shortcuts work only through the CLI (`pluely --action <id>` bound in the compositor; sway lines are in ARCHITECTURE.md). There is no portal backend: wlroots has none, and it is only useful on KDE/GNOME. Hold-to-move is X11-only.
- **Other platforms.** macOS doesn't compile (`lib.rs` reassigns a non-`mut` `builder`), and Windows is untested.
- **App behaviour is unchecked.** Every stabilization agent ran headless. The first live session should check: a question with a mid-sentence pause gets one answer; you speaking closes the turn in about 1s; echo on speakers is ignored; continuous-mode record/send/discard; quick actions between recordings.

## How to continue

Each phase went through this loop:

1. **Scope.** Write `NN-slug.md`: the problem, evidence (`file:line`), a concrete failure scenario, acceptance criteria, and which sibling issues own which files.
2. **Phase.** Run issues in parallel only when they touch disjoint files. Hot spots are `speaker/commands.rs`, `useSystemAudio.ts`, `lib.rs` and `package.json`/`package-lock.json` (JSON is `binary` in `.gitattributes`, so merges need a manual pick, then `npm install`).
3. **Plan.** One read-only Plan agent per issue writes `NN-slug.plan.md`. The orchestrator reviews the trade-offs and appends a `## Review amendments` section, which overrides the plan body.
4. **Implement.** One fresh-context agent per plan, in its own worktree, on branch `issue/NN-slug`. Workflow worktrees fork from `master`, so the agent must run `git reset --hard riir` on its empty branch first. Inside worktrees use `nix develop path:. -c …`.
5. **Merge and release.** Merge `--no-ff` into `riir`. Run `npx tsc --noEmit -p .`, `npx vitest run`, `cargo clippy --all-targets -- -D warnings` and `cargo test` (the live-Pulse tests need PipeWire running). Bump the version in `package.json`, `tauri.conf.json`, `Cargo.toml` and `package-lock.json`, then tag `v0.1.N` and push the tag. If a phase goes bad, roll back to the previous tag and stop to sync with the owner.

Test specs to extend rather than duplicate:
- `src-tauri/tests/vad.rs` and `tests/turn.rs`, with the fixtures in `tests/fixtures/vad/`. Regenerate them with `nix develop -c src-tauri/tests/fixtures/vad/gen.sh`.
- The in-module `lifecycle`, `recordings` and `lockstep` tests in `speaker/commands.rs`.
- Vitest for hooks.
