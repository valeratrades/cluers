import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { cancelChat, generateRequestId, streamChat, type StreamChatRequest } from "@/lib";
import type { AttachedFile, ScreenshotConfig } from "@/types";

const SCREEN_RECORDING_DENIED =
  "Screen Recording permission required. Please enable it by going to System Settings > Privacy & Security > Screen & System Audio Recording. If you don't see Pluely in the list, click the '+' button to add it. If it's already listed, make sure it's enabled. Then restart the app.";

export function useScreenshotCapture(
  config: ScreenshotConfig,
  onCapture: (shot: AttachedFile, autoPrompt: string | null) => void | Promise<void>,
  onError: (message: string) => void
): { captureScreenshot: () => Promise<void>; isScreenshotLoading: boolean } {
  const [isScreenshotLoading, setIsScreenshotLoading] = useState(false);
  const latest = useRef({ config, onCapture, onError });
  useLayoutEffect(() => {
    latest.current = { config, onCapture, onError };
  });
  const awaitingSelection = useRef(false); // captured-selection is broadcast to every window
  const hasCheckedPermission = useRef(false);

  const deliver = useCallback(async (base64: string) => {
    const { config, onCapture, onError } = latest.current;
    const now = Date.now();
    const shot: AttachedFile = {
      id: now.toString(),
      name: `screenshot_${now}.png`,
      type: "image/png",
      base64,
      size: base64.length,
    };
    if (config.mode !== "auto") return onCapture(shot, null);
    if (!config.autoPrompt.trim()) return onError("Auto screenshot prompt is empty");
    return onCapture(shot, config.autoPrompt);
  }, []);

  useEffect(() => {
    const selection = listen<string>("captured-selection", async (event) => {
      if (!awaitingSelection.current) return;
      awaitingSelection.current = false;
      setIsScreenshotLoading(false);
      await deliver(event.payload);
    });
    const closed = listen("capture-closed", () => {
      awaitingSelection.current = false;
      setIsScreenshotLoading(false);
    });
    return () => {
      void selection.then((f) => f());
      void closed.then((f) => f());
    };
  }, [deliver]);

  const captureScreenshot = useCallback(async () => {
    const { config, onError } = latest.current;
    setIsScreenshotLoading(true);
    try {
      if (navigator.platform.toLowerCase().includes("mac") && !hasCheckedPermission.current) {
        const { checkScreenRecordingPermission, requestScreenRecordingPermission } =
          await import("tauri-plugin-macos-permissions-api");
        if (!(await checkScreenRecordingPermission())) {
          await requestScreenRecordingPermission();
          await new Promise((resolve) => setTimeout(resolve, 2000));
          if (!(await checkScreenRecordingPermission())) {
            setIsScreenshotLoading(false);
            onError(SCREEN_RECORDING_DENIED);
            return;
          }
        }
        hasCheckedPermission.current = true;
      }

      if (config.enabled) {
        const base64 = await invoke<string>("capture_to_base64");
        setIsScreenshotLoading(false);
        await deliver(base64);
      } else {
        awaitingSelection.current = true;
        await invoke("start_screen_capture");
      }
    } catch (e) {
      awaitingSelection.current = false;
      setIsScreenshotLoading(false);
      onError(`Failed to capture screenshot: ${e}`);
    }
  }, [deliver]);

  return { captureScreenshot, isScreenshotLoading };
}

/** One in-flight LLM request per hook; a new `run` or `cancel` supersedes the previous one. */
export function useStream(): {
  run: (
    req: Omit<StreamChatRequest, "requestId">,
    onDelta: (d: string) => void
  ) => Promise<string | null>;
  cancel: () => void;
} {
  const currentRef = useRef<string | null>(null);

  const run = useCallback(
    async (req: Omit<StreamChatRequest, "requestId">, onDelta: (d: string) => void) => {
      const previous = currentRef.current;
      const requestId = generateRequestId();
      currentRef.current = requestId;
      if (previous) await cancelChat(previous);
      let full = "";
      try {
        for await (const delta of streamChat({ ...req, requestId })) {
          if (currentRef.current !== requestId) return null;
          full += delta;
          onDelta(delta);
        }
      } catch (e) {
        if (currentRef.current !== requestId) return null; // superseded requests reject with `Cancelled`
        currentRef.current = null;
        throw e;
      }
      if (currentRef.current !== requestId) return null;
      currentRef.current = null;
      return full;
    },
    []
  );

  const cancel = useCallback(() => {
    const id = currentRef.current;
    currentRef.current = null;
    if (id) void cancelChat(id);
  }, []);

  useEffect(() => cancel, [cancel]);

  return { run, cancel };
}
