import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, test } from "vitest";
import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";
import Overlay from "./Overlay";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

let closes: number;
let container: HTMLDivElement;
let root: Root;

beforeEach(async () => {
  closes = 0;
  mockIPC((cmd) => {
    if (cmd === "close_overlay_window") closes++;
  });
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  await act(async () => root.render(<Overlay monitorIndex={0} />));
});
afterEach(async () => {
  await act(async () => root.unmount());
  container.remove();
  clearMocks();
});

const dispatch = (el: EventTarget, event: Event) =>
  act(async () => void el.dispatchEvent(event));
const mouse = (el: Element, type: string, x = 0, y = 0) =>
  dispatch(el, new MouseEvent(type, { bubbles: true, clientX: x, clientY: y }));

const cases: Array<[string, (c: HTMLDivElement) => Promise<void>]> = [
  [
    "cancel button click",
    async (c) => {
      const button = c.querySelector("button")!;
      await mouse(button, "mousedown");
      await mouse(button, "mouseup");
      await mouse(button, "click");
    },
  ],
  [
    "escape key",
    () => dispatch(document, new KeyboardEvent("keydown", { key: "Escape", bubbles: true })),
  ],
  [
    "tiny drag cancels",
    async (c) => {
      const backdrop = c.firstElementChild!;
      await mouse(backdrop, "mousedown", 100, 100);
      await mouse(backdrop, "mouseup", 103, 103);
    },
  ],
];

test.each(cases)("%s closes the overlay once", async (_name, gesture) => {
  await gesture(container);
  expect(closes).toBe(1);
});
