# 12 Turn aggregation in Rust: no fragment loss, SKIP carries over, chronological history

Depends on 08 (segmenter), 10 (STT callable from Rust), 09 (shared ProviderInput builder).

Current flow: Rust segment → `speech-detected` (wav b64) → `useSystemAudio.ts` STT → `processWithAI` with a SKIP/COPY prompt → own conversation. No merging.

Bugs:
- Fragment loss: a ≥1s mid-sentence pause splits a question into A+B. B's arrival cancels A's LLM call (`useSystemAudio.ts:647`); A returns at `:715` without being added to the conversation; history is the render-time snapshot (`:407`) → B goes alone. If A's STT is slower, A cancels B instead → answer to stale fragment.
- SKIP drops the transcript (`:416-417`) though the prompt (`:62`) tells the model "reply SKIP, wait for the next chunk" → next chunk arrives without its first half.
- History order: messages prepended (`:751-765`) and sent newest-first (`:407`, `llm/provider.rs:241`); `handleQuickActionClick` (`:573`) appends to newest-first array.
- `isProcessing` shared across concurrent segments; 30s STT timeout timer never cleared and STT not aborted (`:384`); STT returns raw body as transcript on JSON parse failure (`stt.function.ts:235`).

Goal: `turn.rs` (next to `vad.rs`) — pure state machine consuming source-tagged segments (`Interviewer` now; `User` arrives in 13) + transcripts, deciding "turn complete", concatenating fragments, carrying SKIPped text forward, owning ordering. STT behind a trait (fake in tests). Tests extend 08's fixtures: split question with 1.5s pause → exactly one LLM request containing both halves; SKIP → next request contains prior text; out-of-order STT completion → chronological result. `useSystemAudio.ts` (1312 LOC) shrinks to rendering turn events.
