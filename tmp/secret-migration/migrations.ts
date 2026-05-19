// One-time migration off plaintext secret storage. Lifts API-key-bearing
// variables out of `localStorage[curl_selected_ai_provider].variables`
// into the OS keychain, then blanks the values in localStorage (the
// variable *names* stay so the settings UI keeps rendering the same
// inputs). Mirrors Phase 1.1's SQLite legacy bridge: probe, migrate,
// mark done, never run again.

import { STORAGE_KEYS } from "@/config";
import { setProviderSecret, markSecretMigrationComplete } from "@/lib/llm";
import { safeLocalStorage } from "@/lib/storage";

const MIGRATION_MARKER = "keychain_migration_v1";
const MIGRATION_DONE = "done";

// Reserved per-message tokens the request path always supplies itself —
// never user-set, never a secret. Skip them when shoveling variables
// into the keychain.
const RESERVED_TOKENS = new Set([
  "SYSTEM_PROMPT",
  "TEXT",
  "IMAGE",
  "IMAGE_MIME",
  "AUDIO",
  "DOCUMENT",
]);

export async function runSecretMigration(): Promise<void> {
  if (safeLocalStorage.getItem(MIGRATION_MARKER) === MIGRATION_DONE) {
    return;
  }

  const raw = safeLocalStorage.getItem(STORAGE_KEYS.SELECTED_AI_PROVIDER);
  if (raw) {
    try {
      const parsed = JSON.parse(raw) as {
        provider?: string;
        variables?: Record<string, string>;
      };
      const providerId = parsed?.provider;
      const variables = parsed?.variables ?? {};
      if (providerId) {
        const blanked: Record<string, string> = { ...variables };
        for (const [key, value] of Object.entries(variables)) {
          if (typeof value !== "string" || !value.trim()) continue;
          const upperName = key.toUpperCase();
          if (RESERVED_TOKENS.has(upperName)) continue;
          // We can't distinguish "secret" from "non-secret" from JS
          // without a schema; the convention in this app is that
          // anything stored in the variables dict that isn't a reserved
          // token is user-managed and should live in the keychain. The
          // settings UI's secret/non-secret split is then driven by
          // listProviderSecretNames going forward.
          await setProviderSecret(providerId, upperName, value);
          blanked[key] = "";
        }
        safeLocalStorage.setItem(
          STORAGE_KEYS.SELECTED_AI_PROVIDER,
          JSON.stringify({ ...parsed, variables: blanked })
        );
      }
    } catch (e) {
      // If the JSON is corrupt there's nothing to migrate; fail loud
      // so we don't silently strand secrets in plaintext.
      throw new Error(
        `Secret migration: failed to parse ${
          STORAGE_KEYS.SELECTED_AI_PROVIDER
        }: ${e instanceof Error ? e.message : String(e)}`
      );
    }
  }

  // Signal Rust to drop secure_storage.json and stamp the keychain
  // marker; only then mark the JS side done.
  await markSecretMigrationComplete();
  safeLocalStorage.setItem(MIGRATION_MARKER, MIGRATION_DONE);
}
