import { StrictMode, act, useEffect } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";
import { emit } from "@tauri-apps/api/event";
import { useGlobalShortcutListeners, useGlobalShortcuts } from "./useGlobalShortcuts";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

type Registry = ReturnType<typeof useGlobalShortcuts>;

beforeEach(() => {
  vi.useFakeTimers({ toFake: ["setTimeout"] });
  mockIPC(() => {}, { shouldMockEvents: true });
});
afterEach(() => {
  clearMocks();
  vi.useRealTimers();
});

const cases: Array<[string, (r: Registry, hit: () => void) => void]> = [
  ["start-audio-recording", (r, hit) => r.registerAudioCallback(hit)],
  ["trigger-screenshot", (r, hit) => r.registerScreenshotCallback(hit)],
  ["toggle-system-audio", (r, hit) => r.registerSystemAudioCallback(hit)],
  [
    "focus-text-input",
    (r, hit) => {
      const input = document.createElement("input");
      input.focus = hit;
      r.registerInputRef(input);
    },
  ],
];

test.each(cases)("%s fires once per emit and stops after unmount", async (event, register) => {
  const hit = vi.fn();
  // Mirrors production: several consumers of useGlobalShortcuts in the main window.
  const MainWindow = () => {
    const shortcuts = useGlobalShortcuts();
    useGlobalShortcuts();
    useEffect(() => register(shortcuts, hit), [shortcuts]);
    useGlobalShortcutListeners(() => false);
    return null;
  };
  const root = createRoot(document.createElement("div"));
  await act(async () =>
    root.render(
      <StrictMode>
        <MainWindow />
      </StrictMode>
    )
  );
  await act(async () => {});

  await emit(event);
  vi.runAllTimers();
  expect(hit).toHaveBeenCalledTimes(1);

  await act(async () => root.unmount());
  await emit(event);
  vi.runAllTimers();
  expect(hit).toHaveBeenCalledTimes(1);
});
