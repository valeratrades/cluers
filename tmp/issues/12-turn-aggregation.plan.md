
# Plan for issue 12: turn aggregation in Rust

Worktree setup (see `tmp/issues/README.md`): run `git reset --hard riir`, then run every tool with `nix develop path:. -c …`. Issue 19 is the only other phase-3 issue, and it only touches shortcuts, so no file overlaps.

## 0. What the code does today (checked on `riir` @ ecb55f4)

- The flow is: segment → `speech-detected` (wav b64) → `onSpeechDetectedRef` → `fetchSTT` (now Rust `transcribe`, done by issue 10) → `processWithAI`.
- `processWithAI` cancels the previous request (`useSystemAudio.ts:~628`). A superseded stream returns at `if (currentRequestIdRef.current !== requestId) return null`, so that fragment never reaches the conversation.
- History is `conversation.messages` from the render closure (`:~424`), and it is kept newest-first because new messages are prepended.
- SKIP restores the old transcription and drops the new one.
- `Promise.race` with a 30s `setTimeout` is never cleared, and nothing aborts the STT request.
- `isProcessing` is one flag shared by every segment.
- `ResultsSection.tsx:139` depends on the newest-first order: `.slice(2)` skips the current pair.

Because session state is split across the capture task, the TS closures and IPC, the fix puts the whole segment → STT → turn → LLM loop inside the one capture task that already exists. That task is the sanctioned spawn: its handle is owned by `AudioState` and always joined. TS only renders events, persists answered turns, and holds session memory between capture restarts.

## 1. New pure module `src-tauri/src/speaker/turn.rs` (no tauri, no clock, no IO)

This is a sans-IO state machine. The driver runs STT and the LLM and feeds the results back in as inputs. Tests script the order of completions, so no STT trait is needed.

```rust
pub(crate) const SKIP_WORDS: [&str; 2] = ["SKIP", "COPY"];
pub(crate) const SKIP_INSTRUCTION: &str = "…"; // moved from TS SYSTEM_AUDIO_SKIP_INSTRUCTION; reword to "reply SKIP; the fragment will be prepended to the next chunk"
const TURN_GAP_MS: u64 = 2000; // silence after a segment's speech end that closes the turn; > natural mid-question pauses (~1.5s), effectively max(this, silence_ms)

pub enum Input {
    SpeechStart { at_ms: u64 },
    Segment { start_ms: u64, end_ms: u64 },      // start_ms is the segment id
    Discarded,
    Transcript { start_ms: u64, text: Result<String, String> },
    Tick { now_ms: u64 },                        // audio clock
    Flush,                                       // close the open turn now (audio ended / continuous recording)
    Prompt(String),                              // quick action
    Reply(Result<String, String>),               // outcome of the in-flight Ask
}

pub enum Output {
    Ask { message: String, history: Vec<HistoryMessage> },
    Event(TurnEvent),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum TurnEvent {                  // the IPC contract; `Delta` is only produced by the driver
    Heard { text: String },           // carry + every unanswered transcript, in chronological order
    Asked { message: String },
    Delta { delta: String },
    Answered { message: String, answer: String },
    Skipped { message: String, carried: bool }, // SKIP => carried, COPY => not
    NoSpeech,
    Failed { error: String },
}

pub struct Turns { /* private */ }
impl Turns {
    pub fn new(history: Vec<HistoryMessage>, carry: String) -> Self;
    pub fn push(&mut self, input: Input) -> Vec<Output>;
}
```

Private state:
- `history`, `carry`
- `speaking: bool`
- `last_end_ms`
- `open: BTreeMap<u64, Option<Result<String,String>>>`: segments of the open turn, keyed by start, so they stay chronological.
- `closed: VecDeque<Pending>`, where `enum Pending { Speech(BTreeMap<..>), Prompt(String) }`
- `asking: Option<String>`

Rules:
1. **Close the turn.** Before handling any timestamped input (`SpeechStart{at_ms}`, `Tick{now_ms}`), check whether the open turn should close: `!speaking && !open.is_empty() && t >= last_end_ms + TURN_GAP_MS` → move `open` to `closed`. Doing this check before `speaking = true` makes the boundary independent of chunk size.
   - `Segment` inserts into `open`, sets `speaking = false` and `last_end_ms = end_ms`.
   - `Discarded` sets `speaking = false`.
   - `Flush` closes `open` unconditionally.
2. **Transcripts.** `Transcript` finds its `start_ms` in `open` or `closed`. If it is missing or already filled, panic `"transcript for unknown segment"`, because that is a driver bug. A non-empty text emits `Heard`.
3. **Pump.** Runs after every input: while `asking.is_none()` and the front of `closed` is ready (every transcript is `Some`):
   - If any segment's transcript is `Err`, emit `Failed { error }` with the errors joined, and drop the turn. `carry` is kept. A partial question is not asked, because answering fragments is the bug being fixed.
   - Otherwise join the non-empty texts with `" "` in start order. If that is empty, emit `NoSpeech`.
   - Otherwise `message = carry + " " + text` (a `Prompt` uses `"\n\n"` as the separator). Clear `carry`, set `asking`, and emit `Ask { message, history: history.clone() }` plus `Event(Asked)`.
4. **Reply.** `Reply` takes `asking` (panic if there is none).
   - Trimmed `"SKIP"` → `carry = message`, `Skipped{carried:true}`.
   - `"COPY"` → `Skipped{carried:false}`. Not added to history, same as today.
   - Empty → `Failed("model returned an empty answer")`, because `append_turn` rejects an empty assistant message.
   - Otherwise push user + assistant onto `history` (chronological) and emit `Answered`.
   - `Err(e)` → `Failed(e)`.
   - Then pump again.
5. **One LLM request at a time, FIFO, never cancelled.** A turn that closes while an answer is still streaming waits for it. Its history then includes the previous pair.

`speaker/mod.rs`: add `pub mod turn;`. `lib.rs`: add `pub use speaker::turn;` next to `pub use speaker::vad;`.

## 2. Tests first: `src-tauri/tests/turn.rs` (+ `tests/common/mod.rs`)

- Move `dir`, `load` and `truth` from `tests/vad.rs` into `tests/common/mod.rs`. Both test files then use `mod common;`.
- **Red commit** (like 08): add `turn.rs` with a naive `push` that reproduces today's TS behaviour (each transcript is asked at once, SKIP drops the text), plus the test below. Record that it fails. Then write the real machine.

Fixture harness `run(case) -> Vec<String>`:
- Load `pauses_{rate}` for each of the 16k, 44.1k and 48k rates.
- Push it through `Segmenter::new(&VadConfig::default(), rate)` in 4096-sample chunks.
- Map VAD events to `Input`s, then `Tick{now_ms = samples_seen*1000/rate}`. At EOF, push `Flush`.
- Fake STT: the transcript for a segment is the `u{i+1}` names of the truth lines that overlap `[start,end]` (for example `"u1 u2"`). It is delivered `stt_delay_ms[seg_idx]` of audio time later, in order of delivery time.
- Fake LLM: each `Ask` takes the next string from `replies` and delivers it `reply_delay_ms` later as `Reply(Ok)`.
- Trace lines:
  - `ask "<msg>" h=<history len>`
  - `answered`
  - `skipped carried|copy`
  - `nospeech`
  - `failed <e>`
  - `Heard`/`Asked` are left out of the trace.
- The trace must be identical at all three rates.

Truth for `pauses`: u1 and u2 form one segment with a 0.5s pause, then a 1.5s pause, then u3, then 3s, then u4.

| case | stt_delay | reply_delay | replies | want |
|---|---|---|---|---|
| split question | [] | 0 | A,B | `ask "u1 u2 u3" h=0`, `answered`, `ask "u4" h=2`, `answered` |
| SKIP carries | [] | 0 | SKIP,B | `ask "u1 u2 u3" h=0`, `skipped carried`, `ask "u1 u2 u3 u4" h=0`, `answered` |
| out-of-order STT | [5000] (seg 0 finishes after seg 1) | 0 | A,B | same as split |
| answer in flight | [] | 10000 | A,B | same as split (the 2nd ask waits and sees h=2) |
| COPY | [] | 0 | COPY,B | `ask "u1 u2 u3" h=0`, `skipped copy`, `ask "u4" h=0`, `answered` |

Scripted cases (no audio), same trace format, in a `check(&[Input], replies, want)` table:
- Every transcript empty → `nospeech`.
- One segment `Err` → `failed …` and no ask.
- `Prompt` while an answer is in flight → asked after it, with carry prepended.
- An empty reply → `failed`.

## 3. LLM plumbing, so the capture task can stream a chat (net deletion)

- `llm/stream.rs`, `provider.rs`, `pluely.rs`:
  - Replace `channel: &Channel<StreamEvent>` with `on_delta: &mut impl FnMut(String) -> Result<(), LlmError>`.
  - Delete the `cancel_rx` parameter and its inner `select!`s. Dropping the future already cancels it.
  - The `stream.rs` tests collect deltas into a `Vec` instead of a `Channel::new` callback.
- `llm/commands.rs`:
  - Split the request into `#[derive(Deserialize)] pub struct Chat { provider, message, system_prompt, history, attached_files }` and `StreamChatRequest { #[serde(flatten)] chat: Chat, request_id }`. The wire shape does not change.
  - Move text-attachment inlining and the pluely/custom branch into `pub(crate) async fn complete(app: &AppHandle, llm: &LlmState, chat: Chat, on_delta: &mut impl FnMut(String) -> Result<(), LlmError>) -> Result<String, LlmError>`.
  - `stream_chat` becomes: register; `select! { biased; _ = &mut reg.rx => Err(Cancelled), r = complete(..., &mut |d| channel.send(Chunk{delta:d}).map_err(..)) => r }`; then the existing Done/Err mapping.
- `api.rs:208-222` (`perform_user_audio_transcription`): a JSON body with no `text`/`transcription`/`result` field, or a non-JSON body, becomes `Err(format!("no transcript in response: {body}"))` instead of returning the raw body. This is the remaining copy of the issue's "raw body as transcript" bug.

## 4. Driver in `src-tauri/src/speaker/commands.rs`

- **Capture slot.** Change it to `Option<Capture>`, with `pub(crate) struct Capture { task: JoinHandle<()>, control: mpsc::UnboundedSender<Control> }`.
  - `start(open: impl FnOnce(mpsc::UnboundedReceiver<Control>) -> Result<F, String>)` creates the channel.
  - `lock_idle` and `stop` use `.task`.
  - Update the lifecycle tests: their closures take `|_|`.
  - `lib.rs` `AudioState.capture` changes type to match.
- **IPC input types** (in `commands.rs`, next to `VadCalibration`):
  ```rust
  #[derive(Deserialize)] #[serde(rename_all="camelCase")]
  pub struct SessionConfig { stt: ProviderInput, ai: ProviderInput, system_prompt: String, attached_files: Vec<AttachedFile> }
  #[derive(Deserialize)] #[serde(rename_all="camelCase")]
  pub struct Session { config: SessionConfig, history: Vec<HistoryMessage>, carry: String }
  #[derive(Deserialize)] #[serde(tag="kind", rename_all="camelCase")]
  pub enum Control { Config { config: SessionConfig }, Prompt { text: String } }
  ```
- **`start_system_audio_capture`** gains the arguments `session: Session, events: Channel<TurnEvent>`.
- **New command `system_audio_control(app, control: Control) -> Result<(), String>`.** It locks the slot and calls `slot.as_ref()…control.send(control)`. A missing slot or a closed receiver both return `Err("System audio capture is not running")`. Register it in `lib.rs`.
- **Driver:**
  ```rust
  async fn drive(app: &AppHandle, sr: u32, vad_cfg: &VadConfig,
                 audio: impl Stream<Item = (Vec<VadEvent>, u64)> + Unpin,
                 session: Session, control: mpsc::UnboundedReceiver<Control>, events: Channel<TurnEvent>)
  ```
  - `tokio::select!` over four branches:
    1. `audio.next()`: map `VadEvent`s to `Input`s. Keep the `vad-metrics` and `speech-discarded` emits. Delete the `speech-start` emit, which has no listener. Then send `Tick`.
    2. `Some((start, r)) = stt.next()`: `stt` is a `FuturesUnordered` of `timeout(30s, stt::transcribe(app, &llm, &cfg.stt, &wav, "audio/wav"))`.
    3. `Some(r) = &mut answer`: `answer` is an `OptionFuture<BoxFuture<Result<String, LlmError>>>`, reset to `None` inside the handler.
    4. `Some(c) = control.recv()`: `Config` replaces the config; `Prompt` becomes `Input::Prompt`.
  - When `audio` ends, push `Flush`, then return once `stt.is_empty()` and `answer` is `None`.
  - For `Output::Ask`, build an owned `Chat` from the current config:
    - `system_prompt = format!("{}\n\n{SKIP_INSTRUCTION}", cfg.system_prompt)`
    - `history` and `attached_files` are cloned.
    - `answer = Some(Box::pin(complete(app, &llm, chat, &mut on_delta)))`.
    - `on_delta` buffers while `SKIP_WORDS.iter().any(|w| w.starts_with(full.trim()))`, then sends `TurnEvent::Delta`.
  - If `events.send` fails, log it and return, which ends the capture. The renderer that owned the channel is gone, and a blind pipeline must not keep answering.
  - WAV encoding: rename `samples_to_wav_b64` to `samples_to_wav -> Vec<u8>` and drop base64. Call it with `.expect("segment non-empty, sr validated at start")`.
  - Nothing is spawned. Every STT and LLM future is owned by the capture task, so `stop` (abort) cancels all of them. This removes the TS cancel paths and the uncleared timer.
- **VAD path:** `audio = stream.ready_chunks(4096).map(|c| { seen += c.len(); (vad.push(&c), seen*1000/sr) })`.
- **Continuous path:** `run_continuous_capture` returns `Option<Vec<f32>>` instead of emitting `speech-detected`. Its silent and empty branches keep their emits and return `None`. The task then calls `drive` with `stream::iter(rec.map(|s| (vec![VadEvent::Segment{start_ms:0,end_ms,samples:normalize(gate(s))}], end_ms)))`, which is one segment followed by Flush, so it answers and returns. The event `speech-detected` no longer exists.

## 5. TS: `src/hooks/useSystemAudio.ts` shrinks to rendering

- **Delete:**
  - `processWithAI`, `onSpeechDetectedRef` and the `speech-detected` listener
  - the STT timeout race
  - `currentRequestIdRef` and all `cancelChat`
  - `lastTranscriptionRef`
  - `SYSTEM_AUDIO_SKIP_INSTRUCTION` and `SKIP_WORDS`
  - the debounced diff persistence (`saveTimeoutRef`, `isSavingRef`, `persistedIdsRef`, `startConversation`/`appendMessage` loop)
  - `processWithAI` and `setConversation` from the return value (no consumers)
- **Add:**
  - A `TurnEvent` TS type that mirrors the Rust enum.
  - `conversationRef` and `carryRef` (refs, to avoid stale closures), `persistRef: Promise<void>`, and `answerStartedRef`.
  - `buildSessionConfig()`:
    - `stt`: `resolveProviderInput(selectedSttProvider, allSttProviders)`
    - `ai`: `resolveProviderInput(selectedAIProvider, allAiProviders)`
    - `systemPrompt`: `buildEnhancedSystemPrompt(useSystemPrompt ? systemPrompt||DEFAULT : contextContent||DEFAULT)`
    - `attachedFiles`
  - `startBackend(cfg: VadConfig)`: create a `new Channel<TurnEvent>()` whose `onmessage` goes through `onTurnRef.current`, then invoke `start_system_audio_capture` with `{ vadConfig, deviceId, session: { config, history: conversationRef.current.messages.map(({role,content})=>({role,content})), carry: carryRef.current }, events }`. `startCapture`, `startContinuousRecording`, the mode switch in `updateVadConfiguration`, and the calibrate restart all call it.
  - `onTurn`:
    - `heard` → `setLastTranscription(text)`
    - `asked` → set the transcription to `message`, `isAIProcessing=true`, `isProcessing=false`, `carryRef=""`, `answerStartedRef=false`
    - `delta` → the first delta replaces `lastAIResponse`, later deltas append
    - `answered` → `isAIProcessing=false`; **append** user+assistant to `conversation.messages` (chronological); chain `appendTurn(conversation.id || null, {user: message, attachedFiles: [], assistant: answer})` on `persistRef` so turns are saved in order and only one conversation is created; store the returned `conversationId`; a failure goes to `setError`
    - `skipped` → set `carryRef = carried ? message : ""`, show `skippedNotice`
    - `noSpeech` → `discardedNotice` "no speech recognized", `isProcessing=false`
    - `failed` → `setError`, clear both busy flags, open the popover
  - An effect on `[selected providers, prompts, contextContent, useSystemPrompt, attachedFiles]` sends `system_audio_control {kind:"config"}` while the backend is running (`capturing && (vadConfig.enabled || isRecordingInContinuousMode)`). Errors go to `setError`.
  - `handleQuickActionClick(a)` → `invoke("system_audio_control", {control:{kind:"prompt", text:a}})`. Errors go to `setError`.
  - `startNewConversation`: clear the conversation and carry. If VAD capture is running, `stop` + `startBackend` so Rust is re-seeded with the empty session.
  - `isProcessing` now means only "manual send pending": set by `manualStopAndSend`, cleared by the next turn event or by `speech-discarded`.
- `src/pages/app/components/speech/ResultsSection.tsx:139`: change `.slice(2).sort(desc)` to `.slice(0, -2).reverse()`, since the array is now chronological. This is a two-line touch in a file owned by 16, which is in phase 5, so there is no conflict.
- `src/lib/llm/index.ts` `resolveProviderInput`: its "Please select an AI provider" error now also serves STT. Change the text to "Please select a provider in settings". Do not add a parameter.

## 6. Docs

`ARCHITECTURE.md`, speaker section. Add or replace these points:
- The capture slot holds `{task, control}`.
- `turn.rs` is the pure turn machine, and `tests/turn.rs` is its spec.
- The capture task owns STT and LLM futures: FIFO, one answer at a time, and `Channel<TurnEvent>` per start.
- Session memory (history, carry) lives in the renderer and is seeded on each start.

In the llm section, `stream_*` now take `on_delta` and cancellation is the outer `select!` in `stream_chat`.

## 7. Verification

- `nix develop path:. -c cargo test --manifest-path src-tauri/Cargo.toml --test turn` (red on the naive commit, green after)
- `nix develop path:. -c cargo test --manifest-path src-tauri/Cargo.toml`
- `nix develop path:. -c cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings`
- `nix develop path:. -c npm run build && nix develop path:. -c npm test`
- Manual check, with `RUST_LOG=info nix develop path:. -c npm run tauri dev`: play interview audio that has a ~1.5s pause mid-question. Expect one answer, the SKIP notice followed by a merged next request, and Previous listed newest-first. Continuous mode: record → Stop & Send → one answer, and history carries over to the next recording.

Sequencing: step 2 (red) → 1 → 3 → 4 → 5 → 6. Steps 3 and 4 can land as separate commits, since 3 is a pure refactor with green tests.

### Critical Files for Implementation
- /home/v/s/other/cluers/src-tauri/src/speaker/turn.rs (new)
- /home/v/s/other/cluers/src-tauri/tests/turn.rs (new)
- /home/v/s/other/cluers/src-tauri/src/speaker/commands.rs
- /home/v/s/other/cluers/src-tauri/src/llm/commands.rs (+ stream.rs, provider.rs, pluely.rs)
- /home/v/s/other/cluers/src/hooks/useSystemAudio.ts

## Review amendments (orchestrator)
- Add scripted boundary cases around `TURN_GAP_MS`: a next segment starting at gap−100ms merges into the same turn, and one starting at gap+100ms opens a new turn. Also cover `Tick`s arriving in large chunks (4096 samples at 16k, about 256ms per chunk): the boundary decision must not depend on chunk size.
- Give `TURN_GAP_MS` a tail comment naming its ceiling. Issue 13 adds user speech start as an immediate turn close, and this constant stays as the no-mic fallback.
