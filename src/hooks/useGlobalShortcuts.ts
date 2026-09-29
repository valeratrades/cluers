import { listen } from "@tauri-apps/api/event";
import { useEffect } from "react";

let inputEl: HTMLInputElement | null = null;
let onAudio: (() => void) | null = null;
let onScreenshot: (() => void | Promise<void>) | null = null;
let onSystemAudio: (() => void) | null = null;

const registry = {
  registerInputRef: (input: HTMLInputElement | null) => {
    inputEl = input;
  },
  registerAudioCallback: (callback: () => void) => {
    onAudio = callback;
  },
  registerScreenshotCallback: (callback: () => void | Promise<void>) => {
    onScreenshot = callback;
  },
  registerSystemAudioCallback: (callback: () => void) => {
    onSystemAudio = callback;
  },
};

export const useGlobalShortcuts = () => registry;

/** Mount once per webview, from the main-window root hook. */
export const useGlobalShortcutListeners = () =>
  useEffect(() => {
    // Non-null: callbacks are registered by child/earlier effects before this one runs.
    const pending = [
      listen("focus-text-input", () => setTimeout(() => inputEl!.focus(), 100)),
      listen("start-audio-recording", () => onAudio!()),
      listen("trigger-screenshot", () => void onScreenshot!()),
      listen("toggle-system-audio", () => onSystemAudio!()),
    ];
    return () => pending.forEach((p) => p.then((unlisten) => unlisten()));
  }, []);
