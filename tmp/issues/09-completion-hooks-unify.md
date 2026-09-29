# 09 Unify useCompletion / useChatCompletion; fix their shared bugs

`src/hooks/useCompletion.ts` (915) and `src/hooks/useChatCompletion.ts` (690) are ~70% the same hook:

| Block | useCompletion | useChatCompletion |
|---|---|---|
| Screenshot capture + permission + listener wiring | 714-841 | 511-643 |
| Provider-input construction | 135-184, again 481-531 | 162-241 |
| Stream loop + request-id guard | 186-213, again 533-556 | 243-297 |
| Cancel/unmount cleanup | 257-264, 848-857 | 360-367, 645-654 |
| handleKeyPress | 618-625 | 467-474 |

`useCompletion` also re-implements `submit` inside screenshot-submit (444-611).
`ProviderInput` is built a 4th time in `useSystemAudio.ts:682` — provide the shared builder in `src/lib/llm`, but do not edit `useSystemAudio.ts` (owned by issue 12).

Bugs to fix in the process:
- Screenshot `captured-selection` listener leak (upstream #218): effect depends on `handleScreenshotSubmit` whose identity changes per state change; async `listen` resolves after sync cleanup → old listener never removed → one capture triggers N submits and N DB writes. (`useCompletion.ts:785-829`, `useChatCompletion.ts:586-631`)
- Stale closure: `useChatCompletion.submit` rebuilds final `setMessages` from the snapshot at call start (`:198-202,266-283,325-329`) → interleaved updates reverted.
- `isScreenshotLoading` can stick true forever if `start_screen_capture` throws or overlay never emits (`useCompletion.ts:778-782`, `useChatCompletion.ts:579-583`).
- `persistTurn` (`useCompletion.ts:306-368`) writes user and assistant messages as two calls; failure between → orphan; retry with null conversationId → duplicate conversation. Prefer a single Rust command that persists a turn atomically.
- Cancel callers `.catch(() => {})` — adopt the API from issue 02.
- `useCompletion.ts:671` `scrollAreaRef.current || scrollAreaRef.current` typo.
- `useHistory.ts:37` `selectedConversationId` has no setter anywhere — wire or remove.
