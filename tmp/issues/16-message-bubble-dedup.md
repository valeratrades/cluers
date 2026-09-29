# 16 Three drifted message-bubble renderers

`src/pages/app/components/completion/MessageHistory.tsx:80-116`, inline block in `completion/Input.tsx:172-195`, `speech/ResultsSection.tsx:120-160`: same `role === "user" ? ... : ...` bubble styling, label drift (`You/AI` vs `System/AI`). One `MessageBubble` component. `ResultsSection.tsx:142` `key={message.id || index}` fallback is dead weight.
