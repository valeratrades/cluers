import { STORAGE_KEYS } from "@/config";
import { ProviderKind, TYPE_PROVIDER } from "@/types";
import { setProviderSecret } from "@/lib/llm";
import { safeLocalStorage } from "./helper";

const KEY: Record<ProviderKind, string> = {
  ai: STORAGE_KEYS.CUSTOM_AI_PROVIDERS,
  stt: STORAGE_KEYS.CUSTOM_SPEECH_PROVIDERS,
};

export function getCustomProviders(kind: ProviderKind): TYPE_PROVIDER[] {
  const raw = safeLocalStorage.getItem(KEY[kind]);
  if (raw === null) return [];
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch (e) {
    throw new Error(`localStorage "${KEY[kind]}" is not valid JSON: ${e}`);
  }
  if (!Array.isArray(parsed)) {
    throw new Error(`localStorage "${KEY[kind]}" is not an array: ${raw}`);
  }
  for (const p of parsed) {
    if (
      typeof p?.id !== "string" ||
      typeof p?.curl !== "string" ||
      p?.isCustom !== true
    ) {
      throw new Error(
        `localStorage "${KEY[kind]}" has a malformed provider: ${JSON.stringify(p)}`
      );
    }
  }
  return parsed as TYPE_PROVIDER[];
}

function write(kind: ProviderKind, providers: TYPE_PROVIDER[]) {
  safeLocalStorage.setItem(KEY[kind], JSON.stringify(providers));
}

/** `p.id === ""` appends a new provider; otherwise replaces the one with that id. */
export function saveCustomProvider(kind: ProviderKind, p: TYPE_PROVIDER): void {
  const providers = getCustomProviders(kind);
  if (!p.id) {
    providers.push({ ...p, id: `custom-${crypto.randomUUID()}`, isCustom: true });
  } else {
    const i = providers.findIndex((x) => x.id === p.id);
    if (i === -1) throw new Error(`unknown custom ${kind} provider: ${p.id}`);
    providers[i] = { ...p, isCustom: true };
  }
  write(kind, providers);
}

export function removeCustomProvider(kind: ProviderKind, id: string): void {
  const providers = getCustomProviders(kind);
  const rest = providers.filter((p) => p.id !== id);
  if (rest.length === providers.length) {
    throw new Error(`unknown custom ${kind} provider: ${id}`);
  }
  write(kind, rest);
}

/** Idempotent: plaintext STT `api_key` -> keychain; `{{AUDIO_BASE64}}` -> `{{AUDIO}}`. */
export async function migrateSttLegacyStorage(): Promise<void> {
  const read = () => {
    const raw = safeLocalStorage.getItem(STORAGE_KEYS.SELECTED_STT_PROVIDER);
    return raw === null
      ? null
      : (JSON.parse(raw) as {
          provider: string;
          variables: Record<string, string>;
        });
  };
  const legacy = read();
  const key = legacy?.variables?.api_key;
  if (legacy && key) {
    await setProviderSecret("stt", legacy.provider, "API_KEY", key);
    const fresh = read()!; // re-read: the selection may have changed while awaiting
    delete fresh.variables.api_key;
    safeLocalStorage.setItem(
      STORAGE_KEYS.SELECTED_STT_PROVIDER,
      JSON.stringify(fresh)
    );
  }

  const stt = getCustomProviders("stt");
  if (stt.some((p) => p.curl.includes("AUDIO_BASE64"))) {
    write(
      "stt",
      stt.map((p) => ({ ...p, curl: p.curl.replace(/AUDIO_BASE64/g, "AUDIO") }))
    );
  }
}
