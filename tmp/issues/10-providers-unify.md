# 10 Unify AI / STT provider storage, hooks and config forms; STT secrets to keychain

Duplication (~85% identical):
- `src/lib/storage/ai-providers.ts` vs `stt-providers.ts` (localStorage CRUD; STT id `custom-stt-${Date.now()}` lacks random suffix → same-ms collision).
- `src/hooks/useCustomProvider.ts` vs `useCustomSttProviders.ts`.
- `src/pages/dev/components/{ai-configs,stt-configs}/{CustomProvider,CreateEditProvider,Providers}.tsx` (~1250 LOC).

Bugs that fell out of the copy-paste:
- STT API keys stored in plain `selectedSttProvider.variables[...]` (settings/localStorage), not the keychain the AI path uses (`stt-configs/Providers.tsx:29-120` vs `ai-configs/Providers.tsx:20-70`). Needs a migration of existing plaintext STT keys into the keychain (see `src-tauri/src/llm/secrets.rs` legacy bridge for precedent).
- Deleting a custom STT provider never calls `deleteAllProviderSecrets` (`useCustomSttProviders.ts:57-69` vs `useCustomProvider.ts:58-74`) → stale secrets resurrect on re-add.
- `ai-configs/Providers.tsx:47` keychain errors reported as "not stored".
- `getShortcutsConfig` bypasses `safeLocalStorage` — ignore here (issue 11).

Consumers of STT secrets: TS `src/lib/functions/stt.function.ts` sends custom STT from the renderer; once keys are keychain-only, STT requests with secrets must be issued from Rust (reuse `llm/provider.rs` curl parsing + variable substitution). Issue 12 needs STT callable from Rust anyway — design the Rust STT entry point so 12 can call it directly. Do not edit `useSystemAudio.ts` beyond the call-site swap.

Goal: one generic provider store/hook/form parameterized by kind; secrets only in keychain for both kinds.
