import { Markdown } from "@/components";
import type { ChatMessage } from "@/types";

export const MessageBubble = ({
  message,
  userLabel,
}: {
  message: ChatMessage;
  userLabel: "You" | "System";
}) => {
  if (message.role === "system")
    throw new Error(`system message ${message.id} in chat history`); // nothing produces these
  const isUser = message.role === "user";
  return (
    <div
      className={`p-3 rounded-lg ${
        isUser ? "bg-primary/10 border-l-4 border-primary" : "bg-muted/50"
      }`}
    >
      <div className="flex items-center gap-2 mb-2">
        <span className="text-xs font-medium text-muted-foreground uppercase">
          {isUser ? userLabel : "AI"}
        </span>
        <span className="text-xs text-muted-foreground">
          {new Date(message.timestamp).toLocaleTimeString([], {
            hour: "2-digit",
            minute: "2-digit",
          })}
        </span>
      </div>
      <Markdown>{message.content}</Markdown>
    </div>
  );
};
