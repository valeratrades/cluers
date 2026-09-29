# Stabilization issues (2026-09-29 audit, branch `riir`)

Scope: Linux only. macOS/Windows-specific bugs intentionally not tracked.

| Phase | Issues | Tag |
|---|---|---|
| 1 | 01 02 03 04 05 06 07 | v0.1.10 |
| 2 | 08 09 10 11 18 | v0.1.11 |
| 3 | 12 19 | v0.1.12 |
| 4 | 13 | v0.1.13 |
| 5 | 14+15 (one agent) 16 17 | v0.1.14 |

Phases are chosen so that issues within one phase touch disjoint files.
Approved plans land next to each issue as `NN-slug.plan.md`.

Workflow worktrees fork from `master`, not `riir`. Implementers must `git reset --hard riir` on their fresh branch before any work. Inside worktrees use `nix develop path:. -c ...` (a flake-with-git-worktree bug).

Blocked on the owner (needs a GUI repro): 20.
