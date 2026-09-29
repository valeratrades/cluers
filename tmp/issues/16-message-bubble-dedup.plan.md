## Issue 16: Merge the three message-bubble renderers (frontend only)

### What the code does now (checked on `riir` @ 0db5978, v0.1.13)
- `src/pages/app/components/completion/MessageHistory.tsx:89-109`: bubble is `p-3 rounded-lg`, user gets `bg-primary/10 border-l-4 border-primary`, others get `bg-muted/50`. Label is `You`/`AI`, then a HH:MM time, then `<Markdown>`.
- `src/pages/app/components/completion/Input.tsx:181-204`: the same markup copied inline. The only difference is an extra `text-sm`.
- `src/pages/app/components/speech/ResultsSection.tsx:143-158`: a smaller copy (`p-2 rounded-md text-[11px]`, `border-l-2`, `/5` and `/30` alphas). Label is `System`/`AI`, there is no time, and the body is muted. The key is `message.id || index`.
- `ChatMessage.id` is always non-empty. `useSystemAudio.ts:390,396` builds ids as `local_${ts}_user` / `local_${ts+1}_assistant`, and the DB provides ids for loaded messages. So the `|| index` fallback is dead code.
- `ChatMessage.role` is `"user" | "assistant" | "system"`, but no TS or Rust code creates a `system` chat message. `Role::System` exists only in the DB enum. Today all three renderers would show a `system` message as "AI", which is a silent wrong default.
- The `MessageHistory` prop `currentConversationId` is declared but never read.

### Steps
1. **New file `src/pages/app/components/MessageBubble.tsx`.** It is shared by `completion/` and `speech/`, so it goes in the common parent. Do not re-export it from `index.ts`; the two call sites import it directly, which keeps the public surface small.
   ```tsx
   import { Markdown } from "@/components";
   import type { ChatMessage } from "@/types";

   export const MessageBubble = ({ message, userLabel }: { message: ChatMessage; userLabel: "You" | "System" }) => {
     const isUser = message.role === "user";
     if (message.role === "system") throw new Error(`system message ${message.id} in chat history`); // nothing produces these
     return (
       <div className={`p-3 rounded-lg ${isUser ? "bg-primary/10 border-l-4 border-primary" : "bg-muted/50"}`}>
         <div className="flex items-center gap-2 mb-2">
           <span className="text-xs font-medium text-muted-foreground uppercase">{isUser ? userLabel : "AI"}</span>
           <span className="text-xs text-muted-foreground">
             {new Date(message.timestamp).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })}
           </span>
         </div>
         <Markdown>{message.content}</Markdown>
       </div>
     );
   };
   ```
   - The `MessageHistory` style becomes the only style.
   - `userLabel` is a prop because the label difference is meaningful, not drift. In the speech panel the "user" role is the transcribed system audio (the interviewer), so `System` is correct there. In the overlay it is the person typing, so `You` is correct.
   - The caller sets `key`; the bubble does not.
   - `@/types` re-exports `ChatMessage`; confirm with `grep "export" src/types/index.ts`. The speech path uses the local `ChatMessage` in `useSystemAudio.ts:76`, which has the same shape minus `attachedFiles`, so it is structurally assignable.
2. **`completion/MessageHistory.tsx`:**
   - Replace the inline `<div key=...>…</div>` at lines 89-109 with `<MessageBubble key={message.id} message={message} userLabel="You" />`.
   - Remove `currentConversationId` from `MessageHistoryProps`.
   - Remove `Markdown` from the `@/components` import.
3. **`completion/Input.tsx`:**
   - Replace the block at lines 180-205 with `return <MessageBubble key={message.id} message={message} userLabel="You" />;`.
   - Keep the `index === 0` skip logic unchanged.
   - Remove the `currentConversationId={...}` prop on `<MessageHistory>` at line 72. The `currentConversationId` destructure stays because lines 59 and 66 still use it.
   - `Markdown` stays imported because line 169 uses it.
4. **`speech/ResultsSection.tsx`:**
   - Replace lines 143-158 with `<MessageBubble key={message.id} message={message} userLabel="System" />`. `.map((message) => ...)` no longer needs `index`, which removes the dead fallback.
   - Remove the `cn` import if it has no other use (it doesn't).
   - Leave the live-turn "AI"/"System" blocks at lines 93-130 alone. They render streaming state, not a `ChatMessage`.
   - The inner list container `space-y-1.5 max-h-40 overflow-y-auto` stays.

### Tests
This is a dedup refactor, not a behaviour bug, so no red test is written first. A render test would only restate the JSX, and rendering Streamdown/shiki under jsdom is expensive noise. Skip it. If the owner wants a check, the smallest one is a data-driven `MessageBubble.test.tsx` over `[role, userLabel] -> label text`, plus `system` throws.

### Verification
- `nix develop -c npx tsc --noEmit`: catches the removed prop and unused imports (`noUnusedLocals`, if it is on).
- `nix develop -c npm test`: the existing vitest suites still pass.
- `grep -rn 'role === "user" ?' src/pages/app` returns only `MessageBubble.tsx`.
- Manual GUI check (`nix develop -c npm run tauri dev`):
  - Overlay: send two messages, open the history popover, and check the bubbles.
  - Overlay: turn on conversation mode (Ctrl+K) and check the history below the response.
  - System audio: with conversation mode on, after at least two turns, check that the "Previous" list shows the `System`/`AI` labels with times.

### Scope / ordering
Phase 5 runs in parallel with 14+15 (`useSystemAudio.ts`, audio) and 17 (dead code). This plan touches only the three renderers plus the new file. `src/pages/chats/components/View.tsx` has a fourth, chat-layout renderer with avatars and left/right alignment. It is a different design, so it is out of scope.