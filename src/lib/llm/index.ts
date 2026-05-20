// LLM IPC wrapper. The only TS module that talks to the Rust `llm::*`
// command surface. Streaming uses Tauri 2 `Channel<T>` — no events, no
// polling. Cancellation is by per-request UUID via `cancelChat`.

import { Channel, invoke } from "@tauri-apps/api/core";
import type { AttachedFile } from "@/types";
import { MARKDOWN_FORMATTING_INSTRUCTIONS } from "@/config/constants";
import {
  RESPONSE_LENGTHS,
  LANGUAGES,
} from "@/lib/response-settings.constants";
import { getResponseSettings } from "@/lib/storage/response-settings.storage";

/**
 * Combine the user's base system prompt with the user's response-style
 * settings (length, language) and the markdown formatting policy. This
 * is the JS-side preface that's always prepended before reaching the
 * Rust streaming engine — Rust treats whatever JS passes as opaque.
 */
export function buildEnhancedSystemPrompt(baseSystemPrompt?: string): string {
  const responseSettings = getResponseSettings();
  const prompts: string[] = [];

  if (baseSystemPrompt) {
    prompts.push(baseSystemPrompt);
  }

  const lengthOption = RESPONSE_LENGTHS.find(
    (l) => l.id === responseSettings.responseLength
  );
  if (lengthOption?.prompt?.trim()) {
    prompts.push(lengthOption.prompt);
  }

  const languageOption = LANGUAGES.find(
    (l) => l.id === responseSettings.language
  );
  if (languageOption?.prompt?.trim()) {
    prompts.push(languageOption.prompt);
  }

  prompts.push(MARKDOWN_FORMATTING_INSTRUCTIONS);
  return prompts.join(" ");
}

export interface ProviderInput {
  id: string;
  curl: string;
  responseContentPath: string;
  streaming: boolean;
  isPluelyHosted: boolean;
  // Non-secret values only. Secret values live in the OS keychain via
  // `setProviderSecret` and are merged in by Rust.
  userVariables: Record<string, string>;
}

export interface HistoryMessage {
  role: "user" | "assistant" | "system";
  content: string;
}

export interface StreamChatRequest {
  provider: ProviderInput;
  message: string;
  systemPrompt?: string;
  history: HistoryMessage[];
  attachedFiles: AttachedFile[];
  requestId: string;
}

type StreamChunk =
  | { kind: "chunk"; delta: string }
  | { kind: "done"; fullResponse: string; requestId: string }
  | { kind: "error"; message: string; requestId: string };

export interface Model {
  id: string;
  name: string;
  // Rust returns a single comma-separated string (e.g. "text,image"); we
  // match that here so existing `.includes("image")` substring checks
  // continue to type-check.
  modality?: string;
  [k: string]: any;
}

export function generateRequestId(): string {
  // Browser crypto.randomUUID is available in Tauri's webview.
  return crypto.randomUUID();
}

/**
 * Stream an LLM chat turn. The generator yields token deltas as they
 * arrive and returns when the stream completes. On error, it throws.
 * Cancellation is by calling `cancelChat(request.requestId)` from
 * another async context.
 */
export async function* streamChat(
  request: StreamChatRequest
): AsyncGenerator<string, void, void> {
  const channel = new Channel<StreamChunk>();
  const queue: StreamChunk[] = [];
  let pending: ((msg: StreamChunk) => void) | null = null;
  channel.onmessage = (msg) => {
    if (pending) {
      const resolve = pending;
      pending = null;
      resolve(msg);
    } else {
      queue.push(msg);
    }
  };

  // Start the stream. Rust returns the requestId synchronously after
  // registering the cancellation handle, then proceeds to drive the
  // channel until a terminal event is sent.
  const pump = invoke<string>("stream_chat", { request, channel });

  try {
    while (true) {
      const msg: StreamChunk = queue.length
        ? queue.shift()!
        : await new Promise<StreamChunk>((r) => {
            pending = r;
          });
      if (msg.kind === "chunk") {
        yield msg.delta;
      } else if (msg.kind === "done") {
        return;
      } else {
        throw new Error(msg.message);
      }
    }
  } finally {
    // Surface any invoke-level failure that didn't make it through the
    // channel (e.g. the command rejected before opening the stream).
    await pump.catch(() => {});
  }
}

export function cancelChat(requestId: string): Promise<void> {
  return invoke("cancel_chat", { requestId });
}

// -- Provider secrets --------------------------------------------------------

export function setProviderSecret(
  providerId: string,
  name: string,
  value: string
): Promise<void> {
  return invoke("set_provider_secret", { providerId, name, value });
}

export function listProviderSecretNames(
  providerId: string
): Promise<string[]> {
  return invoke("list_provider_secret_names", { providerId });
}

export function deleteProviderSecret(
  providerId: string,
  name: string
): Promise<void> {
  return invoke("delete_provider_secret", { providerId, name });
}

export function deleteAllProviderSecrets(providerId: string): Promise<void> {
  return invoke("delete_all_provider_secrets", { providerId });
}

// -- Pluely selected model ---------------------------------------------------

export function pluelySelectedModelGet(): Promise<Model | null> {
  return invoke("pluely_selected_model_get");
}

export function pluelySelectedModelSet(model: Model): Promise<void> {
  return invoke("pluely_selected_model_set", { model });
}

