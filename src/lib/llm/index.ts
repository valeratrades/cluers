// LLM IPC wrapper. The only TS module that talks to the Rust `llm::*`
// command surface. Streaming uses Tauri 2 `Channel<T>` — no events, no
// polling. Cancellation is by per-request UUID via `cancelChat`.

import { Channel, invoke } from "@tauri-apps/api/core";
import type { AttachedFile, ProviderKind, TYPE_PROVIDER } from "@/types";
import { shouldUsePluelyAPI } from "@/lib/functions/pluely.api";
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

interface ProviderInput {
  id: string;
  curl: string;
  responseContentPath: string;
  streaming: boolean;
  isPluelyHosted: boolean;
  // Non-secret values only. Secret values live in the OS keychain via
  // `setProviderSecret` and are merged in by Rust.
  userVariables: Record<string, string>;
}

export async function resolveProviderInput(
  selected: { provider: string; variables: Record<string, string> },
  providers: TYPE_PROVIDER[]
): Promise<ProviderInput> {
  if (await shouldUsePluelyAPI()) {
    return {
      id: "pluely",
      curl: "",
      responseContentPath: "",
      streaming: true,
      isPluelyHosted: true,
      userVariables: {},
    };
  }
  if (!selected.provider) {
    throw new Error("Please select a provider in settings");
  }
  const provider = providers.find((p) => p.id === selected.provider);
  if (!provider) {
    throw new Error("Invalid provider selected");
  }
  return {
    id: selected.provider,
    curl: provider.curl,
    responseContentPath: provider.responseContentPath ?? "",
    streaming: provider.streaming ?? false,
    isPluelyHosted: false,
    userVariables: Object.fromEntries(
      Object.entries(selected.variables)
        .filter(([, v]) => v !== "")
        .map(([k, v]) => [k.toUpperCase(), v])
    ),
  };
}

interface HistoryMessage {
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
  | { kind: "done"; fullResponse: string; requestId: string };

type Msg = StreamChunk | { kind: "failed"; error: unknown };

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
 * arrive and returns when the stream completes. Any failure, including
 * the command rejecting before streaming (e.g. duplicate `requestId`),
 * is thrown. Cancellation is by calling `cancelChat(request.requestId)`
 * from another async context.
 */
export async function* streamChat(
  request: StreamChatRequest
): AsyncGenerator<string, void, void> {
  const channel = new Channel<StreamChunk>();
  const queue: Msg[] = [];
  let pending: ((msg: Msg) => void) | null = null;
  const deliver = (msg: Msg) => {
    if (pending) {
      const resolve = pending;
      pending = null;
      resolve(msg);
    } else {
      queue.push(msg);
    }
  };
  channel.onmessage = deliver;
  invoke("stream_chat", { request, channel }).catch((error) =>
    deliver({ kind: "failed", error }) // unread if the consumer abandoned the stream
  );

  while (true) {
    const msg: Msg = queue.length
      ? queue.shift()!
      : await new Promise<Msg>((r) => {
          pending = r;
        });
    if (msg.kind === "chunk") {
      yield msg.delta;
    } else if (msg.kind === "done") {
      return;
    } else {
      throw new Error(String(msg.error));
    }
  }
}

/**
 * Idempotent: unknown/finished ids are a no-op. Rejects only on IPC
 * failure (a bug) — do not catch.
 */
export function cancelChat(requestId: string): Promise<void> {
  return invoke("cancel_chat", { requestId });
}

// -- Provider secrets --------------------------------------------------------

export function setProviderSecret(
  kind: ProviderKind,
  providerId: string,
  name: string,
  value: string
): Promise<void> {
  return invoke("set_provider_secret", { kind, providerId, name, value });
}

export function listProviderSecretNames(
  kind: ProviderKind,
  providerId: string
): Promise<string[]> {
  return invoke("list_provider_secret_names", { kind, providerId });
}

export function deleteProviderSecret(
  kind: ProviderKind,
  providerId: string,
  name: string
): Promise<void> {
  return invoke("delete_provider_secret", { kind, providerId, name });
}

export function deleteAllProviderSecrets(
  kind: ProviderKind,
  providerId: string
): Promise<void> {
  return invoke("delete_all_provider_secrets", { kind, providerId });
}

// -- Pluely selected model ---------------------------------------------------

export function pluelySelectedModelGet(): Promise<Model | null> {
  return invoke("pluely_selected_model_get");
}

export function pluelySelectedModelSet(model: Model): Promise<void> {
  return invoke("pluely_selected_model_set", { model });
}

