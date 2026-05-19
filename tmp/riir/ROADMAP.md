# Pluely — Rewrite-in-Rust Roadmap

Survey snapshot (2026-05-19):
- TypeScript: ~21.4k LOC across 174 files (mostly misplaced backend, not UI)
- Rust: ~5.3k LOC, system-integration heavy (`capture`, `speaker/`, `shortcuts`, `window`, `activate`, partial `api`)

## Framing

The "TypeScript problem" is mostly a **layering problem**: business logic, persistence, and network code drifted into the renderer because Tauri's JS plugins made it frictionless. Roughly 60% of TS LOC isn't UI. The naive ordering ("pick a Rust UI lib, start porting pages") would lock in a renderer before the boundary is right, and force dragging SQL/HTTP/IPC mess into whichever Rust UI lib was chosen.

**Correct architecture first:** most of the rewrite isn't a UI rewrite at all — it's relocating logic across the IPC boundary. Do that first; the eventual UI swap becomes small and reversible.

## Phase 0 — Decide the final renderer (no code yet)

Two viable forks. Pick before doing UI work; the answer affects whether Tauri itself stays.

| Path | Stack | What dies | What survives |
|---|---|---|---|
| **A. Stay in webview, Rust-via-WASM** | Tauri + Dioxus or Leptos | npm, vite, React, Radix, all `@tauri-apps/plugin-*` JS shims | Tauri shell, current windowing/shortcuts/capture |
| **B. Escape webview entirely** | Slint or egui or iced (no Tauri webview) | Tauri webview, JS toolchain, all CSS, markdown stack (shiki/streamdown/remark-*) | Only the pure-Rust crates (`capture`, `speaker`, `shortcuts`, db) |

Tradeoffs:
- **A** is lower-risk; keep keyboard shortcuts/windowing/permissions plugins, replace renderer in place.
- **B** is the bigger architectural win (no IPC at all, dramatic binary shrink, no JS toolchain) but you'll personally own the markdown + KaTeX + syntax-highlight pipeline that today comes free via `streamdown`/`shiki`/`rehype-katex`.

For a stealth low-latency assistant, **B** is probably the right destination — but only commit after Phase 1 finishes, when you can see how small the UI actually is.

## Phase 1 — Push the IPC boundary deeper (the real rewrite)

Ordered by **architectural leverage**, not size. Each step ends with a smaller, more honest frontend and is independently shippable.

1. **Database → Rust.** `tauri-plugin-sql` + `src/lib/database/*` (~740 LOC) → `sqlx` or `rusqlite` owned in Rust, exposed as narrow commands (`list_chats`, `append_message`, `delete_chat`, `list_prompts`, …). Migrations already live in `src-tauri/src/db/migrations/`; queries are in TS. Biggest single bug-class deletion in the project.
2. **LLM streaming → Rust.** `src/lib/functions/ai-response.function.ts` + `useChatCompletion.ts` (~700 LOC) → finish what `src-tauri/src/api.rs` (1167 LOC) started. `reqwest` + structured concurrency, emit tokens as Tauri events. Keys never enter JS memory.
3. **STT → Rust.** `src/lib/functions/stt.function.ts` + `useSystemAudio.ts` → move next to `capture.rs` / `speaker/`. Audio bytes currently round-trip through JS for no reason.
4. **Storage / config → Rust.** `src/lib/storage/*` (~620 LOC) — providers, shortcuts, response settings, prompt config. Rust becomes the source of truth; frontend reads via commands and listens for change events. Kills `customizable.storage.ts`, `helper.ts`, and most of `useSettings`/`useShortcuts`/`useCustomProvider`/`useCustomSttProviders`.
5. **Trivia.** `@bany/curl-to-json` → tiny Rust parser. `moment` → delete (deprecated). `analytics.ts` already has a Rust plugin counterpart.
6. **Collapse `app.context.tsx` (794 LOC).** After 1–5 it's almost entirely UI state + selectors over backend events. Sheds >half its LOC on its own.

End state: TS surface is roughly **pages + components + a thin IPC client + a markdown view**. That's the real rewrite scope — knowable precisely before committing to a renderer.

## Phase 2 — Hooks audit

With logic gone, ~half of `src/hooks/` becomes one-liner `invoke()` wrappers. Many disappear: `useChatCompletion`, `useHistory`, `useSystemPrompts`, `useCustomProvider`, `useCustomSttProviders`, `useShortcuts`, `useSystemAudio` (mostly). Do not preserve them across the UI rewrite — they're React idioms, not domain concepts.

## Phase 3 — Renderer rewrite

Execute the Phase-0 decision. Suggested sub-order:
1. **`src/routes` + `layouts`** — view-state enum + top-level shell. Small, mechanical, validates framework choice on a real surface before sinking time.
2. **Settings/audio/dev/screenshot/shortcuts/responses pages** — forms-over-state; trivial once storage is Rust-native.
3. **Dashboard** — drop `recharts`; native widget or `plotters`.
4. **Chats list / view** — biggest remaining piece; surfaces framework rough edges.
5. **`pages/app/` (completion + speech)** — last, most behavioral, benefits most from clean backend. Audio visualizer becomes genuinely nice in egui/Slint.

## Deletable any time (no rewrite needed)

- `moment` (deprecated; native `Intl` short-term, `chrono` later)
- `@bany/curl-to-json` after Phase 1.5
- `react-error-boundary` once the renderer changes
- `tauri-plugin-posthog-api` — worth questioning for a privacy-first product

## Open question for Phase 0

Markdown + math + syntax highlighting parity is the one non-trivial loss in path **B**. `pulldown-cmark` + `syntect` + a KaTeX-equivalent renderer covers it, but estimate this before committing — it's the single most likely place to regret the choice.
