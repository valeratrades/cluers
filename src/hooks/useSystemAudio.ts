import { useEffect, useState, useCallback, useRef } from "react";
import { useWindowResize, useGlobalShortcuts } from ".";
import { Channel, invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useApp } from "@/contexts";
import {
  DEFAULT_QUICK_ACTIONS,
  DEFAULT_SYSTEM_PROMPT,
  STORAGE_KEYS,
} from "@/config";
import {
  safeLocalStorage,
  generateConversationTitle,
  appendTurn,
  resolveProviderInput,
  buildEnhancedSystemPrompt,
} from "@/lib";

// VAD Configuration interface matching Rust
export interface VadConfig {
  enabled: boolean;
  sensitivity_rms: number;
  peak_threshold: number;
  silence_ms: number;
  min_speech_ms: number;
  pre_speech_ms: number;
  max_segment_ms: number;
  noise_gate_threshold: number;
  max_recording_duration_secs: number;
}

// Live per-chunk metrics emitted from the Rust VAD loop (~10 Hz).
export interface VadMetrics {
  rms: number;
  peak: number;
  sensitivity_rms: number;
  peak_threshold: number;
  noise_gate_threshold: number;
  in_speech: boolean;
}

// Returned from the calibrate_vad_thresholds command.
export interface VadCalibration {
  noise_floor_rms: number;
  sensitivity_rms: number;
  peak_threshold: number;
  noise_gate_threshold: number;
}

// Mirrors `TurnEvent` in src-tauri/src/speaker/turn.rs
type TurnEvent =
  | { kind: "heard"; text: string }
  | { kind: "asked"; message: string }
  | { kind: "delta"; delta: string }
  | { kind: "answered"; message: string; answer: string }
  | { kind: "skipped"; message: string; carried: boolean }
  | { kind: "noSpeech" }
  | { kind: "failed"; error: string };

// Chat message interface (reusing from useCompletion)
interface ChatMessage {
  id: string;
  role: "user" | "assistant" | "system";
  content: string;
  timestamp: number;
}

// Conversation interface (reusing from useCompletion)
export interface ChatConversation {
  id: string;
  title: string;
  messages: ChatMessage[];
  createdAt: number;
  updatedAt: number;
}

const EMPTY_CONVERSATION: ChatConversation = {
  id: "",
  title: "",
  messages: [],
  createdAt: 0,
  updatedAt: 0,
};

export type useSystemAudioType = ReturnType<typeof useSystemAudio>;

// Forward diagnostics to the Rust log (the webview console is invisible in
// normal terminal launches).
const dbg = (msg: string) => {
  invoke("js_log", { msg }).catch(() => {}); // logging a logging failure would recurse
};

export function useSystemAudio() {
  const { resizeWindow } = useWindowResize();
  const globalShortcuts = useGlobalShortcuts();
  const [isPopoverOpen, setIsPopoverOpen] = useState(false);
  const [capturing, setCapturing] = useState(false);
  const [isProcessing, setIsProcessing] = useState(false);
  const [isAIProcessing, setIsAIProcessing] = useState(false);
  const [lastTranscription, setLastTranscription] = useState<string>("");
  const [lastAIResponse, setLastAIResponse] = useState<string>("");
  const [error, setError] = useState<string>("");
  const [setupRequired, setSetupRequired] = useState<boolean>(false);
  const [quickActions, setQuickActions] = useState<string[]>([]);
  const [isManagingQuickActions, setIsManagingQuickActions] =
    useState<boolean>(false);
  const [showQuickActions, setShowQuickActions] = useState<boolean>(true);
  const [vadConfig, setVadConfig] = useState<VadConfig | null>(null);
  const [vadMetrics, setVadMetrics] = useState<VadMetrics | null>(null);
  const [lastCalibration, setLastCalibration] = useState<VadCalibration | null>(
    null
  );
  const [isCalibrating, setIsCalibrating] = useState<boolean>(false);
  const [calibrationError, setCalibrationError] = useState<string>("");
  const [discardedNotice, setDiscardedNotice] = useState<string>("");
  const discardedTimeoutRef = useRef<NodeJS.Timeout | null>(null);
  // The input was received and understood, but the model explicitly decided
  // not to answer (SKIP/COPY). Distinct from `discardedNotice`, which means
  // the input never made it to the model at all.
  const [skippedNotice, setSkippedNotice] = useState<string>("");
  const skippedTimeoutRef = useRef<NodeJS.Timeout | null>(null);
  const [recordingProgress, setRecordingProgress] = useState<number>(0); // For continuous mode
  const [isContinuousMode, setIsContinuousMode] = useState<boolean>(false);
  const [isRecordingInContinuousMode, setIsRecordingInContinuousMode] =
    useState<boolean>(false);

  const [conversation, setConversation] =
    useState<ChatConversation>(EMPTY_CONVERSATION);
  // Source of truth for session memory; Rust is seeded from it on every capture start.
  const conversationRef = useRef<ChatConversation>(EMPTY_CONVERSATION);
  const carryRef = useRef<string>("");
  // One per conversation, so turns persist in order and a reset never appends to the old one.
  const persistRef = useRef<{ id: string | null; chain: Promise<void> }>({
    id: null,
    chain: Promise.resolve(),
  });
  const answerStartedRef = useRef<boolean>(false);
  const updateConversation = useCallback(
    (f: (c: ChatConversation) => ChatConversation) => {
      conversationRef.current = f(conversationRef.current);
      setConversation(conversationRef.current);
    },
    []
  );
  const resetConversation = useCallback(() => {
    conversationRef.current = EMPTY_CONVERSATION;
    setConversation(EMPTY_CONVERSATION);
    carryRef.current = "";
    persistRef.current = { id: null, chain: Promise.resolve() };
  }, []);

  // Context management states
  const [useSystemPrompt, setUseSystemPrompt] = useState<boolean>(true);
  const [contextContent, setContextContent] = useState<string>("");

  const {
    selectedSttProvider,
    allSttProviders,
    selectedAIProvider,
    allAiProviders,
    systemPrompt,
    selectedAudioDevices,
    attachedFiles,
  } = useApp();
  const scrollAreaRef = useRef<HTMLDivElement>(null);

  // State-transition log. Cheap (fires only on changes) and has repeatedly
  // been the difference between guessing and knowing during UI-state bugs.
  useEffect(() => { dbg(`capturing=${capturing}`); }, [capturing]);
  useEffect(() => { dbg(`isPopoverOpen=${isPopoverOpen}`); }, [isPopoverOpen]);
  useEffect(() => { dbg(`isProcessing=${isProcessing}`); }, [isProcessing]);
  useEffect(() => { dbg(`isAIProcessing=${isAIProcessing}`); }, [isAIProcessing]);
  useEffect(() => {
    dbg(`lastAIResponse(len=${lastAIResponse.length})="${lastAIResponse.slice(0, 40).replace(/\n/g, "\\n")}"`);
  }, [lastAIResponse]);
  useEffect(() => {
    dbg(`lastTranscription="${lastTranscription.slice(0, 60)}"`);
  }, [lastTranscription]);
  useEffect(() => { dbg(`error="${error}"`); }, [error]);
  useEffect(() => { dbg(`discardedNotice="${discardedNotice}"`); }, [discardedNotice]);
  useEffect(() => { dbg(`skippedNotice="${skippedNotice}"`); }, [skippedNotice]);
  useEffect(() => { dbg(`setupRequired=${setupRequired}`); }, [setupRequired]);
  useEffect(() => {
    dbg(`isRecordingInContinuousMode=${isRecordingInContinuousMode}`);
  }, [isRecordingInContinuousMode]);
  useEffect(() => {
    dbg(`conversation: id=${conversation.id} msgs=${conversation.messages.length}`);
  }, [conversation]);

  // Load context settings and VAD config from localStorage on mount
  useEffect(() => {
    const savedContext = safeLocalStorage.getItem(
      STORAGE_KEYS.SYSTEM_AUDIO_CONTEXT
    );
    if (savedContext) {
      try {
        const parsed = JSON.parse(savedContext);
        setUseSystemPrompt(parsed.useSystemPrompt ?? true);
        setContextContent(parsed.contextContent ?? "");
      } catch (error) {
        setError(`Failed to load system audio context: ${error}`);
      }
    }

    const savedVadConfig = safeLocalStorage.getItem("vad_config_v2");
    if (savedVadConfig) {
      try {
        setVadConfig(JSON.parse(savedVadConfig));
      } catch (error) {
        setError(`Failed to load VAD config: ${error}`);
      }
    } else {
      invoke<VadConfig>("default_vad_config")
        .then(setVadConfig)
        .catch((err) => setError(`Failed to load VAD defaults: ${err}`));
    }
  }, []);

  // Load quick actions from localStorage on mount
  useEffect(() => {
    const savedActions = safeLocalStorage.getItem(
      STORAGE_KEYS.SYSTEM_AUDIO_QUICK_ACTIONS
    );
    if (savedActions) {
      try {
        const parsed = JSON.parse(savedActions);
        setQuickActions(parsed);
      } catch (error) {
        setError(`Failed to load quick actions: ${error}`);
      }
    } else {
      setQuickActions(DEFAULT_QUICK_ACTIONS);
    }
  }, []);

  const showDiscarded = (notice: string) => {
    setDiscardedNotice(notice);
    if (discardedTimeoutRef.current) {
      clearTimeout(discardedTimeoutRef.current);
    }
    discardedTimeoutRef.current = setTimeout(() => {
      setDiscardedNotice("");
    }, 3500);
  };

  useEffect(() => {
    const unlisteners: Promise<() => void>[] = [
      listen<number>("recording-progress", (event) => {
        setRecordingProgress(event.payload);
      }),
      listen("continuous-recording-start", () => {
        setRecordingProgress(0);
        setIsRecordingInContinuousMode(true);
      }),
      listen<"sent" | "limit" | "discarded">(
        "continuous-recording-stopped",
        (event) => {
          setRecordingProgress(0);
          setIsRecordingInContinuousMode(false);
          if (event.payload !== "discarded") setIsProcessing(true);
          if (event.payload === "limit") showDiscarded("max duration reached, sent");
        }
      ),
      // Surfaced so the user can see "almost worked"; a discarded recording produces no turn event.
      listen<string>("speech-discarded", (event) => {
        setIsProcessing(false);
        showDiscarded(event.payload);
      }),
      listen<VadMetrics>("vad-metrics", (event) => {
        setVadMetrics(event.payload);
      }),
      // Backend already reset its capture state; mirror it here.
      listen<string>("capture-error", (event) => {
        setError(`System audio capture stopped: ${event.payload}`);
        setCapturing(false);
        setIsProcessing(false);
        setIsAIProcessing(false);
        setIsContinuousMode(false);
        setIsRecordingInContinuousMode(false);
        setRecordingProgress(0);
        setVadMetrics(null);
        setIsPopoverOpen(true);
      }),
    ];
    Promise.all(unlisteners).catch((err) =>
      setError(`Failed to listen for system audio events: ${err}`)
    );

    return () => {
      for (const u of unlisteners) u.then((unlisten) => unlisten(), () => {}); // rejection already reported above
      if (discardedTimeoutRef.current) {
        clearTimeout(discardedTimeoutRef.current);
      }
      if (skippedTimeoutRef.current) {
        clearTimeout(skippedTimeoutRef.current);
      }
    };
  }, []);

  // Called through a ref so the per-start Channel never holds a stale closure.
  const onTurnRef = useRef<(e: TurnEvent) => void>(() => {});
  onTurnRef.current = (e: TurnEvent) => {
    dbg(`turn ${e.kind}`);
    switch (e.kind) {
      case "heard":
        setLastTranscription(e.text);
        return;
      case "asked":
        setLastTranscription(e.message);
        setError("");
        setIsAIProcessing(true);
        setIsProcessing(false);
        carryRef.current = "";
        answerStartedRef.current = false;
        return;
      case "delta":
        if (answerStartedRef.current) {
          setLastAIResponse((prev) => prev + e.delta);
        } else {
          answerStartedRef.current = true;
          setLastAIResponse(e.delta);
        }
        return;
      case "answered": {
        setIsAIProcessing(false);
        setLastAIResponse(e.answer);
        const timestamp = Date.now();
        updateConversation((c) => ({
          ...c,
          messages: [
            ...c.messages,
            {
              id: `local_${timestamp}_user`,
              role: "user",
              content: e.message,
              timestamp,
            },
            {
              id: `local_${timestamp + 1}_assistant`,
              role: "assistant",
              content: e.answer,
              timestamp: timestamp + 1,
            },
          ],
          updatedAt: timestamp,
          title: c.title || generateConversationTitle(e.message),
        }));
        const p = persistRef.current;
        p.chain = p.chain
          .then(async () => {
            const turn = await appendTurn(p.id, {
              user: e.message,
              attachedFiles: [],
              assistant: e.answer,
            });
            p.id = turn.conversationId;
            if (persistRef.current === p) {
              updateConversation((c) => ({ ...c, id: turn.conversationId }));
            }
          })
          .catch((err) => {
            setError(`Failed to save conversation: ${err}`);
          });
        return;
      }
      case "skipped":
        setIsAIProcessing(false);
        carryRef.current = e.carried ? e.message : "";
        setSkippedNotice(
          `heard "${e.message.slice(0, 120)}" - ${
            e.carried
              ? "judged an incomplete fragment, waiting for the rest"
              : "judged to need no reply"
          }`
        );
        if (skippedTimeoutRef.current) {
          clearTimeout(skippedTimeoutRef.current);
        }
        skippedTimeoutRef.current = setTimeout(() => {
          setSkippedNotice("");
        }, 6000);
        return;
      case "noSpeech":
        setIsProcessing(false);
        showDiscarded("no speech recognized");
        return;
      case "failed":
        setError(e.error);
        setIsProcessing(false);
        setIsAIProcessing(false);
        setIsPopoverOpen(true);
        return;
    }
  };

  const buildSessionConfig = useCallback(async () => {
    const base = useSystemPrompt
      ? systemPrompt || DEFAULT_SYSTEM_PROMPT
      : contextContent || DEFAULT_SYSTEM_PROMPT;
    return {
      stt: await resolveProviderInput(selectedSttProvider, allSttProviders),
      ai: await resolveProviderInput(selectedAIProvider, allAiProviders),
      systemPrompt: buildEnhancedSystemPrompt(base),
      attachedFiles,
    };
  }, [
    selectedSttProvider,
    allSttProviders,
    selectedAIProvider,
    allAiProviders,
    useSystemPrompt,
    systemPrompt,
    contextContent,
    attachedFiles,
  ]);

  const startBackend = useCallback(
    async (cfg: VadConfig) => {
      const events = new Channel<TurnEvent>();
      events.onmessage = (e) => onTurnRef.current(e);
      await invoke("start_system_audio_capture", {
        vadConfig: cfg,
        deviceId:
          selectedAudioDevices.output.id !== "default"
            ? selectedAudioDevices.output.id
            : null,
        micDeviceId:
          selectedAudioDevices.input.id !== "default"
            ? selectedAudioDevices.input.id
            : null,
        session: {
          config: await buildSessionConfig(),
          history: conversationRef.current.messages.map(({ role, content }) => ({
            role,
            content,
          })),
          carry: carryRef.current,
        },
        events,
      });
    },
    [
      buildSessionConfig,
      selectedAudioDevices.output.id,
      selectedAudioDevices.input.id,
    ]
  );

  useEffect(() => {
    if (!capturing) return; // deliberately not a dep: the session starts with the current config
    buildSessionConfig()
      .then((config) =>
        invoke("system_audio_control", { control: { kind: "config", config } })
      )
      .catch((err) => setError(`Failed to update session: ${err}`));
  }, [buildSessionConfig]);

  // Context management functions
  const saveContextSettings = useCallback(
    (usePrompt: boolean, content: string) => {
      try {
        const contextSettings = {
          useSystemPrompt: usePrompt,
          contextContent: content,
        };
        safeLocalStorage.setItem(
          STORAGE_KEYS.SYSTEM_AUDIO_CONTEXT,
          JSON.stringify(contextSettings)
        );
      } catch (error) {
        setError(`Failed to save context settings: ${error}`);
      }
    },
    []
  );

  const updateUseSystemPrompt = useCallback(
    (value: boolean) => {
      setUseSystemPrompt(value);
      saveContextSettings(value, contextContent);
    },
    [contextContent, saveContextSettings]
  );

  const updateContextContent = useCallback(
    (content: string) => {
      setContextContent(content);
      saveContextSettings(useSystemPrompt, content);
    },
    [useSystemPrompt, saveContextSettings]
  );

  // Quick actions management
  const saveQuickActions = useCallback((actions: string[]) => {
    try {
      safeLocalStorage.setItem(
        STORAGE_KEYS.SYSTEM_AUDIO_QUICK_ACTIONS,
        JSON.stringify(actions)
      );
    } catch (error) {
      setError(`Failed to save quick actions: ${error}`);
    }
  }, []);

  const addQuickAction = useCallback(
    (action: string) => {
      if (action && !quickActions.includes(action)) {
        const newActions = [...quickActions, action];
        setQuickActions(newActions);
        saveQuickActions(newActions);
      }
    },
    [quickActions, saveQuickActions]
  );

  const removeQuickAction = useCallback(
    (action: string) => {
      const newActions = quickActions.filter((a) => a !== action);
      setQuickActions(newActions);
      saveQuickActions(newActions);
    },
    [quickActions, saveQuickActions]
  );

  const handleQuickActionClick = async (action: string) => {
    setError("");
    try {
      await invoke("system_audio_control", {
        control: { kind: "prompt", text: action },
      });
    } catch (err) {
      setError(`Quick action failed: ${err}`);
    }
  };

  const record = useCallback(async (action: "start" | "send" | "discard") => {
    try {
      await invoke("system_audio_control", {
        control: { kind: "record", action },
      });
    } catch (err) {
      setError(`Recording ${action} failed: ${err}`);
    }
  }, []);
  const startContinuousRecording = useCallback(() => record("start"), [record]);
  const manualStopAndSend = useCallback(() => record("send"), [record]);
  const ignoreContinuousRecording = useCallback(() => record("discard"), [record]);

  const startCapture = useCallback(async () => {
    try {
      setError("");

      const hasAccess = await invoke<boolean>("check_system_audio_access");
      if (!hasAccess) {
        setSetupRequired(true);
        setIsPopoverOpen(true);
        return;
      }

      if (!vadConfig) throw new Error("VAD config is still loading");
      const isContinuous = !vadConfig.enabled;

      resetConversation();

      setCapturing(true);
      setIsPopoverOpen(true);
      setIsContinuousMode(isContinuous);
      setRecordingProgress(0);
      setVadMetrics(null);
      setDiscardedNotice("");
      setIsRecordingInContinuousMode(false);

      await invoke<string>("stop_system_audio_capture");
      await startBackend(vadConfig);
    } catch (err) {
      const errorMessage = err instanceof Error ? err.message : String(err);
      setError(errorMessage);
      setIsPopoverOpen(true);
    }
  }, [vadConfig, startBackend, resetConversation]);

  const stopCapture = useCallback(async () => {
    // Resets all listen-mode state; the stack identifies which of the many
    // possible triggers (button, shortcut, effect) wiped the screen.
    dbg(`stopCapture called\n${new Error().stack}`);
    try {
      await invoke<string>("stop_system_audio_capture");

      // Reset ALL states
      setCapturing(false);
      setIsProcessing(false);
      setIsAIProcessing(false);
      setIsContinuousMode(false);
      setIsRecordingInContinuousMode(false);
      setRecordingProgress(0);
      setLastTranscription("");
      setLastAIResponse("");
      setError("");
      setIsPopoverOpen(false);
      setVadMetrics(null);
      setDiscardedNotice("");
    } catch (err) {
      const errorMessage = err instanceof Error ? err.message : String(err);
      setError(`Failed to stop capture: ${errorMessage}`);
      console.error("Stop capture error:", err);
    }
  }, []);

  const handleSetup = useCallback(async () => {
    try {
      const platform = navigator.platform.toLowerCase();

      if (platform.includes("mac") || platform.includes("win")) {
        await invoke("request_system_audio_access");
      }

      // Delay to give the user time to grant permissions in the system dialog.
      await new Promise((resolve) => setTimeout(resolve, 3000));

      const hasAccess = await invoke<boolean>("check_system_audio_access");
      if (hasAccess) {
        setSetupRequired(false);
        await startCapture();
      } else {
        setSetupRequired(true);
        setError("Permission not granted. Please try the manual steps.");
      }
    } catch (err) {
      setError(`Failed to request access (${err}). Please try the manual steps below.`);
      setSetupRequired(true);
    }
  }, [startCapture]);

  useEffect(() => {
    const shouldOpenPopover =
      capturing ||
      setupRequired ||
      isAIProcessing ||
      !!lastAIResponse ||
      !!error;
    dbg(`shouldOpenPopover=${shouldOpenPopover}`);
    setIsPopoverOpen(shouldOpenPopover);
    resizeWindow(shouldOpenPopover);
  }, [
    capturing,
    setupRequired,
    isAIProcessing,
    lastAIResponse,
    error,
    resizeWindow,
  ]);

  useEffect(() => {
    globalShortcuts.registerSystemAudioCallback(async () => {
      if (capturing) {
        await stopCapture();
      } else {
        await startCapture();
      }
    });
  }, [capturing, startCapture, stopCapture]);

  useEffect(() => {
    return () => {
      invoke("stop_system_audio_capture").catch((err) =>
        dbg(`unmount stop failed: ${err}`)
      );
    };
  }, []);

  const startNewConversation = useCallback(async () => {
    dbg(`startNewConversation called\n${new Error().stack}`);
    resetConversation();
    setLastTranscription("");
    setLastAIResponse("");
    setError("");
    setSetupRequired(false);
    setIsProcessing(false);
    setIsAIProcessing(false);
    setIsPopoverOpen(false);
    setUseSystemPrompt(true);
    // Rust holds the session memory it was started with.
    if (capturing && vadConfig) {
      try {
        await invoke("stop_system_audio_capture");
        setIsRecordingInContinuousMode(false);
        setRecordingProgress(0);
        await startBackend(vadConfig);
      } catch (err) {
        setError(`Failed to restart capture: ${err}`);
      }
    }
  }, [resetConversation, capturing, vadConfig, startBackend]);

  // Update VAD configuration
  const updateVadConfiguration = useCallback(
    async (config: VadConfig) => {
      try {
        await invoke("update_vad_config", { config });
        const modeChanged = config.enabled !== vadConfig?.enabled;
        setVadConfig(config);
        safeLocalStorage.setItem("vad_config_v2", JSON.stringify(config));

        // The mode picks the capture shape (mic + segmenters vs recorder), so it needs a restart.
        if (modeChanged && capturing) {
          await invoke("stop_system_audio_capture");
          setIsRecordingInContinuousMode(false);
          setRecordingProgress(0);
          setVadMetrics(null);
          await startBackend(config);
        }
      } catch (error) {
        setError(`Failed to update VAD config: ${error}`);
      }
    },
    [vadConfig?.enabled, capturing, startBackend]
  );

  const resetVadConfig = useCallback(async () => {
    try {
      const defaults = await invoke<VadConfig>("default_vad_config");
      if (!vadConfig) throw new Error("VAD config is still loading");
      await updateVadConfiguration({ ...defaults, enabled: vadConfig.enabled });
    } catch (error) {
      setError(`Failed to reset VAD config: ${error}`);
    }
  }, [vadConfig, updateVadConfiguration]);

  // Explicit calibration: stop capturing if needed, sample ambient audio,
  // bake the resulting thresholds into vadConfig (which persists via the
  // existing localStorage save in updateVadConfiguration).
  const calibrateVad = useCallback(
    async (durationSecs: number = 3) => {
      if (isCalibrating) return;
      if (!vadConfig) {
        setCalibrationError("VAD config is still loading");
        return;
      }
      setCalibrationError("");
      setIsCalibrating(true);
      const wasCapturing = capturing;
      try {
        // Release the audio device before calibrating.
        if (wasCapturing) {
          try {
            await invoke("stop_system_audio_capture");
            setCapturing(false);
            setIsRecordingInContinuousMode(false);
            setRecordingProgress(0);
          } catch (e) {
            setCalibrationError(
              `Failed to pause capture: ${e instanceof Error ? e.message : String(e)}`
            );
            return;
          }
        }

        const deviceId =
          selectedAudioDevices.output.id !== "default"
            ? selectedAudioDevices.output.id
            : null;

        const result = await invoke<VadCalibration>(
          "calibrate_vad_thresholds",
          { durationSecs, deviceId }
        );

        setLastCalibration(result);
        const calibrated = {
          ...vadConfig,
          sensitivity_rms: result.sensitivity_rms,
          peak_threshold: result.peak_threshold,
          noise_gate_threshold: result.noise_gate_threshold,
        };
        await updateVadConfiguration(calibrated);

        if (wasCapturing) {
          try {
            setCapturing(true);
            await startBackend(calibrated);
          } catch (e) {
            setError(
              `Calibration applied but restarting capture failed: ${e instanceof Error ? e.message : String(e)}`
            );
          }
        }
      } catch (err) {
        const msg = err instanceof Error ? err.message : String(err);
        setCalibrationError(msg);
      } finally {
        setIsCalibrating(false);
      }
    },
    [isCalibrating, capturing, selectedAudioDevices.output.id, updateVadConfiguration, vadConfig, startBackend]
  );

  useEffect(() => {
    if (capturing && vadConfig) setIsContinuousMode(!vadConfig.enabled);
  }, [vadConfig?.enabled, capturing]);

  // Keyboard arrow key support for scrolling (local shortcut)
  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      if (!isPopoverOpen) return;

      const scrollElement = scrollAreaRef.current?.querySelector(
        "[data-radix-scroll-area-viewport]"
      ) as HTMLElement;

      if (!scrollElement) return;

      const scrollAmount = 100; // pixels to scroll

      if (e.key === "ArrowDown") {
        e.preventDefault();
        scrollElement.scrollBy({ top: scrollAmount, behavior: "smooth" });
      } else if (e.key === "ArrowUp") {
        e.preventDefault();
        scrollElement.scrollBy({ top: -scrollAmount, behavior: "smooth" });
      }
    };

    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [isPopoverOpen]);

  // Keyboard shortcuts for continuous mode recording (local shortcuts)
  useEffect(() => {
    const handleRecordingShortcuts = (e: KeyboardEvent) => {
      if (!isPopoverOpen || !isContinuousMode) return;
      const t = e.target;
      if (
        t instanceof HTMLInputElement ||
        t instanceof HTMLTextAreaElement ||
        (t instanceof HTMLElement && t.isContentEditable)
      ) {
        return;
      }

      // Enter: Start recording (when not recording) or Stop & Send (when recording)
      if (e.key === "Enter" && !e.shiftKey && !e.metaKey && !e.ctrlKey) {
        e.preventDefault();
        if (!isRecordingInContinuousMode) {
          startContinuousRecording();
        } else {
          manualStopAndSend();
        }
      }

      // Escape: Ignore recording (when recording)
      if (e.key === "Escape" && isRecordingInContinuousMode) {
        e.preventDefault();
        ignoreContinuousRecording();
      }

      // Space: Start recording (when not recording) - only if not typing in input
      if (
        e.key === " " &&
        !isRecordingInContinuousMode &&
        !e.metaKey &&
        !e.ctrlKey
      ) {
        e.preventDefault();
        startContinuousRecording();
      }
    };

    window.addEventListener("keydown", handleRecordingShortcuts);
    return () =>
      window.removeEventListener("keydown", handleRecordingShortcuts);
  }, [
    isPopoverOpen,
    isContinuousMode,
    isRecordingInContinuousMode,
    startContinuousRecording,
    manualStopAndSend,
    ignoreContinuousRecording,
  ]);


  return {
    capturing,
    isProcessing,
    isAIProcessing,
    lastTranscription,
    lastAIResponse,
    error,
    setupRequired,
    startCapture,
    stopCapture,
    handleSetup,
    isPopoverOpen,
    setIsPopoverOpen,
    // Conversation management
    conversation,
    // Context management
    useSystemPrompt,
    setUseSystemPrompt: updateUseSystemPrompt,
    contextContent,
    setContextContent: updateContextContent,
    startNewConversation,
    // Window resize
    resizeWindow,
    quickActions,
    addQuickAction,
    removeQuickAction,
    isManagingQuickActions,
    setIsManagingQuickActions,
    showQuickActions,
    setShowQuickActions,
    handleQuickActionClick,
    // VAD configuration
    vadConfig,
    updateVadConfiguration,
    resetVadConfig,
    // Live VAD telemetry
    vadMetrics,
    discardedNotice,
    skippedNotice,
    // Calibration
    calibrateVad,
    isCalibrating,
    calibrationError,
    lastCalibration,
    // Continuous recording
    isContinuousMode,
    isRecordingInContinuousMode,
    recordingProgress,
    manualStopAndSend,
    startContinuousRecording,
    ignoreContinuousRecording,
    // Scroll area ref for keyboard navigation
    scrollAreaRef,
  };
}
