# 02 LLM cancel registry: silent overwrite, swallowed failures

- `src-tauri/src/llm/commands.rs:62` `cancels.insert(request_id, cancel_tx)` does not check for an existing key. Reused id → previous stream becomes uncancellable, and the completion-path `cancels.remove` (`:109`) removes the wrong entry.
- `src/lib/llm/index.ts:136` `await pump.catch(() => {})` swallows invoke rejections that never reached the channel → caller sees empty response, treats as "nothing to persist".
- `cancelChat` failure is swallowed by every caller (`.catch(() => {})`).

Scope: Rust `llm/` + `src/lib/llm/index.ts` only. Callers in `useCompletion.ts`/`useChatCompletion.ts`/`useSystemAudio.ts` are rewritten by issues 09 and 12 — do not edit them here, but make the `src/lib/llm` API such that swallowing is no longer the natural call pattern (e.g. cancel of unknown id is a defined no-op in Rust vs. an error — decide and document which).

Acceptance: duplicate request id is an error (fail fast), invoke-level failures surface to the caller, Rust test covering duplicate-id and cancel-after-complete.
