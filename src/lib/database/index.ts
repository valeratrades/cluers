import { invoke } from "@tauri-apps/api/core";
import type { AttachedFile, ChatConversation } from "@/types/completion";
import type { SystemPrompt } from "@/types/system-prompts";

// -- IPC-only types ----------------------------------------------------------
// `ChatConversation`, `ChatMessage`, `AttachedFile` (in @/types/completion) and
// `SystemPrompt` (in @/types/system-prompts) are the canonical shapes —
// the Rust IPC schema mirrors them. Nothing else in this module needs to
// re-export them.

export interface ConversationSummary {
  id: string;
  title: string;
  createdAt: number;
  updatedAt: number;
  messageCount: number;
}

interface AppendedMessageResponse {
  id: string;
  timestamp: number;
}

interface NewTurn {
  user: string;
  attachedFiles: AttachedFile[];
  assistant: string;
}

interface AppendedTurn {
  conversationId: string;
  user: AppendedMessageResponse;
  assistant: AppendedMessageResponse;
}

// -- chat history ------------------------------------------------------------

export function listConversationSummaries(): Promise<ConversationSummary[]> {
  return invoke("list_conversation_summaries");
}

export function loadConversation(id: string): Promise<ChatConversation> {
  return invoke("load_conversation", { id });
}

export function appendTurn(
  conversationId: string | null,
  turn: NewTurn
): Promise<AppendedTurn> {
  return invoke("append_turn", { conversationId, turn });
}

export function deleteConversation(id: string): Promise<void> {
  return invoke("delete_conversation", { id });
}

export function deleteAllConversations(): Promise<void> {
  return invoke("delete_all_conversations");
}

// -- system prompts ----------------------------------------------------------

export function listSystemPrompts(): Promise<SystemPrompt[]> {
  return invoke("list_system_prompts");
}

export function createSystemPrompt(
  name: string,
  prompt: string
): Promise<SystemPrompt> {
  return invoke("create_system_prompt", { name, prompt });
}

export function editSystemPrompt(
  id: number,
  name?: string,
  prompt?: string
): Promise<SystemPrompt> {
  return invoke("edit_system_prompt", { id, name, prompt });
}

export function deleteSystemPrompt(id: number): Promise<void> {
  return invoke("delete_system_prompt", { id });
}
