import { safeLocalStorage } from "../storage";
import { STORAGE_KEYS } from "@/config";

/**
 * UI-side gate for "should we treat the user as on the Pluely-hosted
 * path right now?". Used to render Pluely-specific UI (model picker,
 * banner, etc.) and to set the `isPluelyHosted` flag on the next
 * `streamChat` request. The actual transport decision is made inside
 * Rust based on the same flag.
 */
export async function shouldUsePluelyAPI(): Promise<boolean> {
  try {
    const pluelyApiEnabled =
      safeLocalStorage.getItem(STORAGE_KEYS.PLUELY_API_ENABLED) === "true";
    return pluelyApiEnabled;
  } catch (error) {
    console.warn("Failed to check Pluely API availability:", error);
    return false;
  }
}
