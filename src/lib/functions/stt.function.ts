import { blobToBase64 } from "./common.function";
import { invoke } from "@tauri-apps/api/core";

import { TYPE_PROVIDER } from "@/types";
import { shouldUsePluelyAPI } from "./pluely.api";

/**
 * Thrown when the STT provider returns a successful response that contains
 * no transcribable speech (e.g. keyboard noise, silence, background hum).
 * Callers in auto-listen modes should treat this as a discarded segment
 * rather than a hard error.
 */
export class NoTranscriptionError extends Error {
  constructor(message = "No transcription found") {
    super(message);
    this.name = "NoTranscriptionError";
  }
}

export interface STTParams {
  provider: TYPE_PROVIDER | undefined;
  selectedProvider: {
    provider: string;
    variables: Record<string, string>;
  };
  audio: File | Blob;
}

export async function fetchSTT(params: STTParams): Promise<string> {
  const { provider, selectedProvider, audio } = params;
  const pluely = await shouldUsePluelyAPI();
  if (!pluely && !provider) throw new Error("Provider not provided");
  const text = await invoke<string>("transcribe", {
    request: {
      provider: pluely
        ? {
            id: "pluely",
            curl: "",
            responseContentPath: "",
            streaming: false,
            isPluelyHosted: true,
            userVariables: {},
          }
        : {
            id: provider!.id!,
            curl: provider!.curl,
            responseContentPath: provider!.responseContentPath ?? "",
            streaming: false,
            isPluelyHosted: false,
            userVariables: Object.fromEntries(
              Object.entries(selectedProvider.variables).map(([k, v]) => [
                k.toUpperCase(),
                v,
              ])
            ),
          },
      audioBase64: await blobToBase64(audio),
      mime: audio.type,
    },
  });
  if (!text.trim()) throw new NoTranscriptionError();
  return text;
}
