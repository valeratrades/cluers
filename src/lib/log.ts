import { invoke } from "@tauri-apps/api/core";

type Level = "error" | "warn" | "info" | "debug";

const MAX_CHARS = 4000;
const original = { error: console.error, warn: console.warn };

const show = (x: unknown): string => {
  if (x instanceof Error) return x.stack ?? `${x}`;
  if (typeof x === "string") return x;
  try {
    return JSON.stringify(x) ?? String(x);
  } catch {
    return String(x); // cyclic
  }
};

/** To the Rust log: the webview console is invisible outside devtools. */
export const jsLog = (level: Level, ...args: unknown[]) =>
  invoke("js_log", { level, msg: args.map(show).join(" ").slice(0, MAX_CHARS) }).catch(
    (e) => original.error("js_log failed", e) // the patched console.error would recurse
  );

/** Once per webview, before render. */
export const forwardConsole = () => {
  for (const level of ["error", "warn"] as const) {
    console[level] = (...args: unknown[]) => {
      original[level](...args);
      void jsLog(level, ...args);
    };
  }
  window.addEventListener("error", (e) =>
    void jsLog("error", `${e.message} at ${e.filename}:${e.lineno}:${e.colno}`, e.error)
  );
  window.addEventListener("unhandledrejection", (e) =>
    void jsLog("error", "unhandled rejection:", e.reason)
  );
};
