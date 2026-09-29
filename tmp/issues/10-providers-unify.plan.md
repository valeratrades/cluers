## Plan for issue 10: unify AI/STT providers, keep STT secrets only in the keychain

### What I checked in the code
- `src/lib/storage/{ai,stt}-providers.ts`, `src/hooks/{useCustomProvider,useCustomSttProviders}.ts` and `src/pages/dev/components/{ai,stt}-configs/*` are copies of each other. The diffs are label text, presets, required variables, and the streaming switch. The STT id `custom-stt-${Date.now()}` has no random part.
- The STT API key is saved as `selectedSttProvider.variables.api_key` in localStorage key `curl_selected_stt_provider`. Only the **currently selected** provider can hold one, because changing the provider resets `variables: {}`.
- `useCustomSttProviders.confirmDelete` never deletes the provider's secrets.
- `ai-configs/Providers.tsx:46-47` uses `.catch(() => setApiKeyStored(false))`, so a keychain error shows as "not stored". `submitApiKey`/`clearApiKey` are `void`ed with no error path.
- **The built-in id `groq` exists in both `AI_PROVIDERS` and `SPEECH_TO_TEXT_PROVIDERS`.** If STT reused the AI keychain entry `pluely.provider.<id>`, both kinds would share one `API_KEY`: clearing one clears the other, and the migration would overwrite the AI key. So secrets must be namespaced by kind.
- The TS STT path (`stt.function.ts`) has more bugs, which go away when it moves to Rust:
  - It always names the multipart field `file`. This breaks the Speechmatics (`data_file`) and Rev (`media`) presets.
  - It lowercases the first letter of `responseContentPath`. This breaks the Azure `DisplayText` preset.
  - It returns the raw body when the response is not JSON (the `:235` bug from issue 12).
  - The Groq preset only works *because of* that raw-body fallback (`-F response_format=text`).
- `parse_curl` in Rust has no `-F` support, and it parses `--data-binary` as JSON. So `--data-binary {{AUDIO}}` fails today.

### Scope and sibling overlap
- `useSystemAudio.ts` (owned by 12): **no edit**. `fetchSTT`'s signature stays the same and only its body changes. `AutoSpeechVad.tsx` and `AudioRecorder.tsx` also need no edit.
- `src/lib/llm/index.ts` is also edited by 09 (shared ProviderInput builder). I only touch the secret-wrapper block at lines 143-168. The two hunks are separate and merge cleanly.
- `src/contexts/app.context.tsx` is also edited by 11 (storage-event handler, lines 497-520). I touch lines 17-80, 296-312 and 405-417. The hunks are next to each other but do not overlap.
- Out of scope, with reasons:
  - Pluely-hosted STT internals (`perform_user_audio_transcription` returns `json.to_string()` when there is no text field). This is Pluely-side code; leave it for 12 or 17.
  - The Speechmatics and Rev presets return job ids, not transcripts. They are async APIs and were already broken.
  - The 30s STT timeout. It is 12's; dropping the Rust future cancels the request.
  - Removing the `@bany/curl-to-json` dependency. That is 17's.

---

### Step 1: namespace secrets by kind (`src-tauri/src/llm/secrets.rs`)
1. Add:
   ```rust
   #[derive(Debug, Clone, Copy, serde::Deserialize)]
   #[serde(rename_all = "lowercase")]
   pub enum ProviderKind { Ai, Stt }
   impl ProviderKind {
       fn service(self, id: &str) -> String // Ai => "pluely.provider.<id>" (unchanged, no AI migration), Stt => "pluely.stt-provider.<id>"
   }
   ```
   Delete `SVC_PROVIDER_PREFIX` and `provider_service`.
2. Every `Secrets` method takes `kind: ProviderKind, id: &str`: `provider`, `set`, `delete`, `delete_all`. The cache key becomes the service string. `load_blocking`/`store_blocking`/`ensure_loaded`/`store` take the service string.
3. Update the module doc so it describes both namespaces. Replace the sentence "intentionally orphaned — there is no migration" only as far as STT is concerned (the STT migration is in TS, step 6).
4. Tests. Change `mod tests` to `#[cfg(test)] pub(crate) mod tests` and make `install()` `pub(crate)`, so `stt.rs` tests can reuse the keyring mock. Update the existing tests to use `ProviderKind::Ai`. Add the failing-first test:
   ```rust
   #[tokio::test] async fn kinds_are_isolated() // set(Ai,"groq_iso","API_KEY","a"); set(Stt,"groq_iso","API_KEY","s");
   // provider(Ai)=={API_KEY:a}, provider(Stt)=={API_KEY:s}; delete_all(Stt) leaves Ai intact (fresh Secrets instance reload)
   ```

### Step 2: make the curl parsing usable for STT (`src-tauri/src/llm/provider.rs`)
1. Change `ParsedCurl`: replace `body: Option<serde_json::Value>` with `data: Option<String>` and add `form: Vec<(String, String)>`.
   - `-d/--data/--data-raw/--data-binary` store the raw string. JSON parsing moves to the consumer, so AI templates behave exactly as before.
   - `-F/--form <k=v>` does `split_once('=')`, with `InvalidCurl("bad -F")` on failure, and pushes to `form`.
   - The default method is POST when `data.is_some() || !form.is_empty()`.
2. Split `stream_custom` into two `pub(crate)` fns that STT reuses:
   - `pub(crate) async fn resolve_vars(secrets: &Secrets, kind: ProviderKind, p: &ProviderInput) -> Result<HashMap<String,String>, LlmError>`. This is the existing user_variables-uppercase ∪ keychain merge (keychain wins) plus the required-variable check against the reserved list.
   - `pub(crate) fn method(s: &str) -> Result<reqwest::Method, LlmError>`. This is the existing match.
3. `stream_custom` calls `resolve_vars(secrets, ProviderKind::Ai, p)` and then inserts `SYSTEM_PROMPT` as today. The body becomes `match parsed.data { None => json!({}), Some(s) => serde_json::from_str(&s).map_err(|e| CurlParse(format!("body json: {e}")))? }`. It has no other changes.

### Step 3: Rust STT entry point (new `src-tauri/src/llm/stt.rs`, `pub mod stt;` in `llm/mod.rs`)
```rust
/// Entry point for both the command and issue 12's turn engine.
pub async fn transcribe(app: &AppHandle, llm: &LlmState, provider: &ProviderInput, audio: &[u8], mime: &str) -> Result<String, LlmError>
// is_pluely_hosted => crate::api::transcribe_pluely(app, &llm.http, audio).await.map_err(LlmError::PluelyStt)
// else => transcribe_custom(&llm.http, &llm.secrets, provider, audio, mime)

async fn transcribe_custom(http: &reqwest::Client, secrets: &Secrets, p: &ProviderInput, audio: &[u8], mime: &str) -> Result<String, LlmError>
```
`transcribe_custom` does the following, in order:
1. If `p.curl` has no `{{AUDIO}}`, return `Err(UnsupportedAttachment("AUDIO"))`. This reuses the existing variant.
2. Call `parse_curl`, then `resolve_vars(secrets, ProviderKind::Stt, p)`. The URL is `substitute_string(&parsed.url, &vars)`, which covers Deepgram's `?model={{MODEL}}` and Azure's `{{REGION}}`.
3. Headers are substituted. When the body is multipart, skip a template `Content-Type` (tail comment: reqwest must set the boundary).
4. Build the body:
   - **Form non-empty:** build a `multipart::Form`. A field whose value contains `{{AUDIO}}` becomes `Part::bytes(audio).file_name("audio.wav").mime_str(mime)?` **under its own key**. Any other field becomes `.text(k, substitute_string(v,&vars))`.
   - **`data.trim() == "{{AUDIO}}"`:** `.body(audio.to_vec())` (Deepgram/Azure/IBM).
   - **Other `data`:** parse it as JSON, insert `AUDIO = base64(audio)` into vars, run `substitute_value`, then `.json(&v)` (Google).
   - **None:** `InvalidCurl("STT template needs -F, -d or --data-binary")`.
5. Send. A non-2xx status returns `ProviderApi{status, body}`. Then `resp.text()` and `tracing::debug!` it, which replaces the TS `js_log`.
6. If the response is not JSON, return `Err(LlmError::SttResponse(text))`. If `extract_by_path(&json, &p.response_content_path)` is `None`, return `Err(SttResponse(format!("`{path}` is not a string in {text}")))`. An empty string is `Ok("")`, which means no speech. This separates "silence" from "misconfigured path"; both used to collapse into NoTranscription or raw-body.

In `llm/mod.rs`, add the `LlmError` variants `#[error("pluely stt: {0}")] PluelyStt(String)` and `#[error("stt response: {0}")] SttResponse(String)`.

### Step 4: command surface (`src-tauri/src/llm/commands.rs`, `src-tauri/src/api.rs`, `src-tauri/src/lib.rs`)
1. `commands.rs`:
   ```rust
   #[derive(Debug, Deserialize)] #[serde(rename_all = "camelCase")]
   pub struct TranscribeRequest { pub provider: ProviderInput, pub audio_base64: String, pub mime: String }
   #[tauri::command]
   pub async fn transcribe(app: AppHandle, state: State<'_, LlmState>, request: TranscribeRequest) -> Result<String, String>
   // STANDARD.decode(audio_base64) (TS blobToBase64 already strips the data-URL prefix) -> stt::transcribe
   ```
   - The four secret commands gain a `kind: ProviderKind` argument and pass it through.
   - `stream_chat` is unchanged apart from `provider::stream_custom` using `Ai` internally.
2. `api.rs`:
   - `transcribe_audio` becomes `pub(crate) async fn transcribe_pluely(app: &AppHandle, http: &reqwest::Client, audio: &[u8]) -> Result<String, String>`. Drop `#[tauri::command]`, use the passed `http` instead of `reqwest::Client::new()`, and return the transcription string.
   - Delete `AudioResponse` and `decode_audio_base64`.
   - Fix the module doc line about "Phase 1.3 will relocate STT".
3. `lib.rs`: in `generate_handler!`, replace `api::transcribe_audio` with `llm::commands::transcribe`.

### Step 5: Rust tests (in `stt.rs`, `#[cfg(test)] mod tests`, data-driven, through `transcribe_custom`)
The harness:
- `accept_one(listener, status, body) -> Vec<u8>`: read the request head, then `Content-Length` bytes of body (assert the header exists), then write a canned response.
- Run it with `tokio::join!(accept_one(..), transcribe_custom(..))`. This is structured, with no `spawn`.
- Secrets come from `secrets::tests::install()` and are seeded through `Secrets::set(ProviderKind::Stt, ...)`.

Table of cases `(curl, stt secrets, user_vars, response, path) -> (expected request substrings, expected result)`:
1. OpenAI form. The request contains `authorization: Bearer k` (the key comes from the **keychain**, not from variables), a `name="file"; filename="audio.wav"` part carrying the audio bytes, and `name="model"` followed by `whisper-1`. Response `{"text":"hi"}` gives `Ok("hi")`.
2. `-F "data_file={{AUDIO}}"`: the audio part is named `data_file`. This is the regression case for the TS bug.
3. `--data-binary {{AUDIO}}` with `?model={{MODEL}}`: the request line contains `model=nova` and the body equals the audio bytes.
4. Google `-d` JSON: the body JSON has `audio.content == base64(audio)`.
5. Response body `hello` (not JSON) gives `Err(SttResponse)`. Before this change, TS returned "hello" as the transcript.
6. `{"text":""}` gives `Ok("")`. `{"foo":1}` with path `text` gives `Err(SttResponse)`.
7. No STT `API_KEY`, while an **AI** secret with the same id exists, gives `Err(MissingVariable("API_KEY"))` and no connection is accepted. The listener gets a short timeout; use `select!` against the transcribe future, which must finish first.

The existing `provider.rs` `build_messages` tests remain. Add one `stream_custom`-free assertion that `parse_curl` with `-d '{...}'` still yields JSON-parsable `data` (AI no-regression). This goes in the same table style as the existing `check`.

### Step 6: TS unification
1. **Types.** In `src/types/provider.type.ts`, add `export type ProviderKind = "ai" | "stt";`.
2. **Secret wrappers.** In `src/lib/llm/index.ts`, `setProviderSecret/listProviderSecretNames/deleteProviderSecret/deleteAllProviderSecrets` take `kind: ProviderKind` first and pass it to `invoke(..., { kind, providerId, ... })`.
3. **Store.** Delete `src/lib/storage/ai-providers.ts` and `stt-providers.ts`. Add `src/lib/storage/providers.ts`:
   ```ts
   const KEY: Record<ProviderKind, string> = { ai: STORAGE_KEYS.CUSTOM_AI_PROVIDERS, stt: STORAGE_KEYS.CUSTOM_SPEECH_PROVIDERS };
   export function getCustomProviders(kind: ProviderKind): TYPE_PROVIDER[]   // absent => []; bad JSON / non-array / entry without string id+curl or isCustom!==true => throw Error naming the key (no silent filtering)
   export function saveCustomProvider(kind: ProviderKind, p: TYPE_PROVIDER): void // p.id === "" => append with id `custom-${crypto.randomUUID()}`; else replace, throw if id unknown
   export function removeCustomProvider(kind: ProviderKind, id: string): void // throw if id unknown
   export async function migrateSttLegacyStorage(): Promise<void>
   ```
   Read and write through `safeLocalStorage`. How `migrateSttLegacyStorage` works:
   - (a) Read `SELECTED_STT_PROVIDER`. If `variables.api_key` is non-empty, `await setProviderSecret("stt", provider, "API_KEY", key)`. Then **re-read** the item, delete `variables.api_key`, and write it back. Re-reading avoids clobbering a change made while the keychain call was awaited.
   - (b) Persistently rewrite `{{AUDIO_BASE64}}` to `{{AUDIO}}` in stored STT customs. This replaces the read-time rewrite in `app.context.tsx`.
   - It is idempotent, so running it twice (main and dashboard windows) is harmless.
   - Update `src/lib/storage/index.ts` exports.
4. **Hook.** Delete `src/hooks/useCustomProvider.ts` and `useCustomSttProviders.ts`. Add `src/hooks/useCustomProviders.ts`:
   - `export function useCustomProviders(kind: ProviderKind)`. It returns the same shape as today.
   - Presets are `kind === "ai" ? AI_PROVIDERS : SPEECH_TO_TEXT_PROVIDERS`.
   - Validation is `validateCurl(curl, kind === "ai" ? ["TEXT"] : ["AUDIO"])`.
   - For stt, `streaming` is forced to `false`.
   - `confirmDelete` calls `await deleteAllProviderSecrets(kind, id); removeCustomProvider(kind, id)` for **both** kinds. Drop the try/catch-console.error wrappers and let rejections surface.
   - A single `EMPTY_FORM` const replaces the four copies of the reset literal.
   - Update `src/hooks/index.ts`.
5. **Components.** Delete `src/pages/dev/components/ai-configs/` and `stt-configs/`. Add `src/pages/dev/components/providers/`:
   - `index.tsx`: `export const ProviderSection = ({ kind, ...settings }: UseSettingsReturn & { kind: ProviderKind })`. It maps `settings` once to `{ kind, providers, selected, onSelect, variables }`: ai uses `allAiProviders/selectedAIProvider/onSetSelectedAIProvider/variables`; stt uses the `Stt` equivalents and `sttVariables`. It renders the header ("AI Providers" / "STT Providers", id `ai-providers`/`stt-providers`), `CustomProviders`, and `Providers`.
   - `Providers.tsx` is the AI (keychain) version, generic over `kind`. `listProviderSecretNames(kind, id)` rejections set `keyError` state, rendered under the input, instead of "not stored". `submitApiKey`/`clearApiKey` route rejections to the same state. STT therefore gains keychain-backed key entry; the plaintext write path is deleted.
   - `CustomProviders.tsx` and `CreateEditProvider.tsx` take `kind` and use one per-kind `FORM` record: label, curl placeholder, required-variable help rows, response-path placeholder/notes, and `streaming: boolean`. The streaming switch is hidden for stt, not disabled.
   - `customProviderHook` becomes a **required** prop. This removes the conditional `customProviderHook || useCustom...()` hook call, which breaks the rules of hooks.
   - `src/pages/dev/components/index.ts` exports `./providers`. `src/pages/dev/index.tsx` renders `<ProviderSection kind="ai" {...settings} />` and `<ProviderSection kind="stt" {...settings} />`.
6. **Context** (`src/contexts/app.context.tsx`):
   - Delete `validateAndProcessCurlProviders` and its `curl2Json` import if nothing else uses it.
   - `loadData` uses `setCustomAiProviders(getCustomProviders("ai")); setCustomSttProviders(getCustomProviders("stt"));`.
   - The mount effect: `loadData(); migrateSttLegacyStorage().then(loadData);`. There is no catch: a keychain failure leaves the plaintext in place and surfaces as an unhandled rejection. It retries on next start.
7. **STT call** (`src/lib/functions/stt.function.ts`). Keep `NoTranscriptionError`, `STTParams` and the `fetchSTT(params)` signature, so callers are untouched. The body becomes:
   ```ts
   const pluely = await shouldUsePluelyAPI();
   if (!pluely && !provider) throw new Error("Provider not provided");
   const text = await invoke<string>("transcribe", { request: {
     provider: pluely
       ? { id: "pluely", curl: "", responseContentPath: "", streaming: false, isPluelyHosted: true, userVariables: {} }
       : { id: provider!.id!, curl: provider!.curl, responseContentPath: provider!.responseContentPath ?? "", streaming: false, isPluelyHosted: false,
           userVariables: Object.fromEntries(Object.entries(selectedProvider.variables).map(([k, v]) => [k.toUpperCase(), v])) },
     audioBase64: await blobToBase64(audio), mime: audio.type } });
   if (!text.trim()) throw new NoTranscriptionError();
   return text;
   ```
   Delete `fetchPluelySTT`, the tauriFetch/curl2Json path, and the try/catch rethrow. In `common.function.ts`, delete `deepVariableReplacer` and `getByPath`; they have no remaining callers.
8. **Preset.** In `src/config/stt.constants.ts`, remove `-F response_format=text` from `groq`. Its default JSON `{"text":...}` then matches `responseContentPath: "text"`.

### Step 7: docs
In `ARCHITECTURE.md`, under the `llm/` section:
- Add `stt.rs` to the layout.
- Add an STT row to the secret table (`pluely.stt-provider.<id>`).
- Note that `transcribe` is one command for both Pluely and custom providers, mirroring `stream_chat`, and that the JS secret surface is kind-scoped.

### Order
Steps 1 → 2 → 3 → 5 (tests green) → 4 → 6 → 7. Rust first so TS compiles against the final IPC.

### Verification
- `nix develop -c cargo test --manifest-path src-tauri/Cargo.toml llm::` covers the new stt table, `kinds_are_isolated`, and the existing provider/secrets/state tests.
- `nix develop -c cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings`
- `nix develop -c npx tsc --noEmit` (or `nix develop -c npm run build`).
- `grep -rn "ai-providers\|stt-providers\|useCustomSttProviders\|useCustomAiProviders\|transcribe_audio\|deepVariableReplacer" src src-tauri/src` should return nothing.
- Manual check in `nix develop -c npm run tauri dev`:
  1. Seed `localStorage.curl_selected_stt_provider = {"provider":"openai-whisper","variables":{"api_key":"sk-..","model":"whisper-1"}}` and restart. The key is gone from localStorage, and `secret-tool search service pluely.stt-provider.openai-whisper` shows it. The STT settings show "(stored)", and system-audio transcription works.
  2. Set different Groq keys for AI and STT and clear one; the other survives.
  3. Add, then delete, a custom STT provider; its `pluely.stt-provider.custom-*` entry is gone.
  4. Stop gnome-keyring or lock the collection; the settings page shows the keychain error instead of "not stored".
## Review amendments (orchestrator)
- Fail-fast on corrupt provider JSON is approved, but the thrown error must be visible in the UI (the error boundary renders the message, including which storage key is corrupt), not a blank window.
- If the plaintext STT key migration fails, the error must be shown to the user (toast or inline on the dev page), not only left as an unhandled rejection.
