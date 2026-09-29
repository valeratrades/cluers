# 06 Small UI bugs

- `src/components/CustomCursor.tsx:38` sets `style.display = "0"` (invalid) — cursor never hides on blur.
- `src/pages/app/components/completion/Input.tsx:174` `conversationHistory.sort(...)` mutates parent state during render.
- `src/pages/chats/components/ChatFiles.tsx:20`, `ChatScreenshot.tsx:7-8` use `any[]`/`any` although `AttachedFile` (`src/types/completion.ts`) mirrors `db/schema.rs`.
- `src/components/Overlay.tsx:33-36` silent catch on `close_overlay_window`; `:58-62` capture failure only console-logged with contradictory comment.
- `src/pages/dev/components/ai-configs/Providers.tsx:47` `.catch(() => setApiKeyStored(false))` hides keychain errors as "not stored" (file is rewritten by issue 10 next phase — only fix if trivially local, otherwise leave to 10).
- `src/pages/system-prompts/PluelyPrompts.tsx:245` index-based key in filtered list.

Acceptance: each fixed; user-visible failures surface to the user, not just console.
