import { act } from "react";
import { createRoot, Root } from "react-dom/client";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { clearMocks, mockIPC, mockWindows } from "@tauri-apps/api/mocks";
import { emit } from "@tauri-apps/api/event";
import { useSystemAudio } from "./useSystemAudio";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const APP = {
  selectedSttProvider: { provider: "", variables: {} },
  allSttProviders: [],
  selectedAIProvider: { provider: "", variables: {} },
  allAiProviders: [],
  systemPrompt: "",
  selectedAudioDevices: { input: { id: "default" }, output: { id: "default" } },
  attachedFiles: [],
};
vi.mock("@/contexts", () => ({ useApp: () => APP }));
vi.mock("@/lib", async (importOriginal) => ({
  ...(await importOriginal<typeof import("@/lib")>()),
  resolveProviderInput: async () => ({}),
  buildEnhancedSystemPrompt: (s: string) => s,
}));

const CONTINUOUS = {
  enabled: false,
  sensitivity_rms: 0.012,
  peak_threshold: 0.035,
  silence_ms: 1000,
  min_speech_ms: 160,
  pre_speech_ms: 300,
  max_segment_ms: 30000,
  noise_gate_threshold: 0.003,
  max_recording_duration_secs: 180,
};

let calls: Array<[string, unknown]> = [];
let hook: { current: ReturnType<typeof useSystemAudio> | null } = { current: null };
let root: Root;
const Harness = () => {
  hook.current = useSystemAudio();
  return null;
};
const h = () => hook.current!;
const invoked = (cmd: string) => calls.filter(([c]) => c === cmd).map(([, a]) => a);
const record = (action: string) => ({ control: { kind: "record", action } });

beforeEach(async () => {
  calls = [];
  hook = { current: null };
  localStorage.setItem("vad_config_v2", JSON.stringify(CONTINUOUS));
  mockWindows("main");
  mockIPC(
    (cmd, args) => {
      calls.push([cmd, args]);
      if (cmd === "check_system_audio_access") return true;
      if (cmd === "update_vad_config") throw "min_speech_ms must be < max_segment_ms";
    },
    { shouldMockEvents: true }
  );
  root = createRoot(document.createElement("div"));
  await act(async () => root.render(<Harness />));
  await act(async () => h().startCapture());
});
afterEach(async () => {
  await act(async () => root.unmount());
  clearMocks();
  localStorage.clear();
  document.body.innerHTML = "";
});

const keydown = async (target: EventTarget) => {
  await act(async () => {
    target.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
  });
};
const recording = async () => {
  await act(async () => emit("continuous-recording-start", 180));
  calls = [];
};

test("continuous startCapture starts the session backend", () => {
  expect(invoked("start_system_audio_capture")).toHaveLength(1);
});

test("startContinuousRecording sends record/start and never stops", async () => {
  calls = [];
  await act(async () => h().startContinuousRecording());
  expect(invoked("system_audio_control")).toEqual([record("start")]);
  expect(invoked("stop_system_audio_capture")).toEqual([]);
});

test("Enter in an input does not send", async () => {
  await recording();
  const input = document.body.appendChild(document.createElement("input"));
  await keydown(input);
  expect(calls.filter(([c]) => c !== "set_window_height")).toEqual([]);
});

test("Enter on the body sends", async () => {
  await recording();
  await keydown(document.body);
  expect(invoked("system_audio_control")).toEqual([record("send")]);
});

test("limit auto-send shows processing and a notice", async () => {
  await recording();
  await act(async () => emit("continuous-recording-stopped", "limit"));
  expect(h().isProcessing).toBe(true);
  expect(h().isRecordingInContinuousMode).toBe(false);
  expect(h().discardedNotice).not.toBe("");
});

test("rejected VAD config is shown", async () => {
  await act(async () => h().updateVadConfiguration({ ...CONTINUOUS, min_speech_ms: 30000 }));
  expect(h().error).toContain("min_speech_ms");
  expect(h().vadConfig).toEqual(CONTINUOUS);
});
