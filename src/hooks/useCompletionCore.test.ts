import { StrictMode, act, createElement } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, expect, test } from "vitest";
import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";
import { emit } from "@tauri-apps/api/event";
import type { ScreenshotConfig } from "@/types";
import { useScreenshotCapture } from "./useCompletionCore";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

afterEach(() => clearMocks());

const config: ScreenshotConfig = { mode: "auto", autoPrompt: "describe", enabled: false };

const settle = () => act(() => new Promise((r) => setTimeout(r, 0)));

type Case = {
  name: string;
  callbacks: string[];
  startFails: boolean;
  capture: boolean;
  events: ("selection" | "closed")[];
  expectCalls: string[];
  expectLoading: boolean;
  expectErrors: number;
};

const cases: Case[] = [
  { name: "latest callback, once", callbacks: ["A", "B", "C"], startFails: false, capture: true, events: ["selection"], expectCalls: ["C"], expectLoading: false, expectErrors: 0 },
  { name: "start fails clears spinner", callbacks: ["A"], startFails: true, capture: true, events: [], expectCalls: [], expectLoading: false, expectErrors: 1 },
  { name: "closed then late selection ignored", callbacks: ["A"], startFails: false, capture: true, events: ["closed", "selection"], expectCalls: [], expectLoading: false, expectErrors: 0 },
  { name: "not initiated ignored", callbacks: ["A"], startFails: false, capture: false, events: ["selection"], expectCalls: [], expectLoading: false, expectErrors: 0 },
];

test.each(cases)("$name", async (c) => {
  mockIPC(
    (cmd) => {
      if (cmd === "start_screen_capture" && c.startFails) throw new Error("no overlay");
    },
    { shouldMockEvents: true }
  );
  const calls: string[] = [];
  const errors: string[] = [];
  let hook!: ReturnType<typeof useScreenshotCapture>;
  const Probe = ({ label }: { label: string }) => {
    hook = useScreenshotCapture(
      config,
      () => {
        calls.push(label);
      },
      (m) => errors.push(m)
    );
    return null;
  };
  const root = createRoot(document.createElement("div"));
  for (const label of c.callbacks) {
    await act(async () => root.render(createElement(StrictMode, null, createElement(Probe, { label }))));
    await settle();
  }
  if (c.capture) {
    await act(() => hook.captureScreenshot());
    await settle();
  }
  for (const e of c.events) {
    await act(() => (e === "selection" ? emit("captured-selection", "b64") : emit("capture-closed")));
    await settle();
  }
  expect(calls).toEqual(c.expectCalls);
  expect(hook.isScreenshotLoading).toBe(c.expectLoading);
  expect(errors.length).toBe(c.expectErrors);
  act(() => root.unmount());
});
