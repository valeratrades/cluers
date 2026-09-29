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
  hop_ms: number;
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

// Mirrors `VadConfig::default()` in src-tauri/src/speaker/vad.rs
const DEFAULT_VAD_CONFIG: VadConfig = {
  enabled: true,
  hop_ms: 20,
  sensitivity_rms: 0.012,
  peak_threshold: 0.035,
  silence_ms: 1000,
  min_speech_ms: 160,
  pre_speech_ms: 300,
  max_segment_ms: 30000,
  noise_gate_threshold: 0.003,
  max_recording_duration_secs: 180,
};

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
  invoke("js_log", { msg }).catch(() => {});
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
  const [vadConfig, setVadConfig] = useState<VadConfig>(DEFAULT_VAD_CONFIG);
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
        console.error("Failed to load system audio context:", error);
      }
    }

    // Load VAD config
    const savedVadConfig = safeLocalStorage.getItem("vad_config_v2");
    if (savedVadConfig) {
      try {
        const parsed = JSON.parse(savedVadConfig);
        setVadConfig(parsed);
      } catch (error) {
        console.error("Failed to load VAD config:", error);
      }
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
        console.error("Failed to load quick actions:", error);
        setQuickActions(DEFAULT_QUICK_ACTIONS);
      }
    } else {
      setQuickActions(DEFAULT_QUICK_ACTIONS);
    }
  }, []);

  // Handle continuous recording progress events AND error events
  useEffect(() => {
    let progressUnlisten: (() => void) | undefined;
    let startUnlisten: (() => void) | undefined;
    let stopUnlisten: (() => void) | undefined;
    let errorUnlisten: (() => void) | undefined;
    let discardedUnlisten: (() => void) | undefined;
    let metricsUnlisten: (() => void) | undefined;
    let captureErrorUnlisten: (() => void) | undefined;

    const setupContinuousListeners = async () => {
      try {
        // Progress updates (every second)
        progressUnlisten = await listen("recording-progress", (event) => {
          const seconds = event.payload as number;
          setRecordingProgress(seconds);
        });

        // Recording started
        startUnlisten = await listen("continuous-recording-start", () => {
          setRecordingProgress(0);
          setIsRecordingInContinuousMode(true);
        });

        // Recording stopped
        stopUnlisten = await listen("continuous-recording-stopped", () => {
          setRecordingProgress(0);
          setIsRecordingInContinuousMode(false);
        });

        // Audio encoding errors
        errorUnlisten = await listen("audio-encoding-error", (event) => {
          const errorMsg = event.payload as string;
          console.error("Audio encoding error:", errorMsg);
          setError(`Failed to process audio: ${errorMsg}`);
          setIsProcessing(false);
          setIsAIProcessing(false);
          setIsRecordingInContinuousMode(false);
        });

        // Speech discarded (too short / silent) - surface so the user can
        // see "almost worked"
        discardedUnlisten = await listen("speech-discarded", (event) => {
          const reason = event.payload as string;
          // A manual stop optimistically enters the processing state; a
          // discarded recording produces no turn event to leave it.
          setIsProcessing(false);
          setDiscardedNotice(reason);
          if (discardedTimeoutRef.current) {
            clearTimeout(discardedTimeoutRef.current);
          }
          discardedTimeoutRef.current = setTimeout(() => {
            setDiscardedNotice("");
          }, 3500);
        });

        // Live VAD metrics (~10 Hz)
        metricsUnlisten = await listen("vad-metrics", (event) => {
          setVadMetrics(event.payload as VadMetrics);
        });

        // Backend already reset its capture state; mirror it here.
        captureErrorUnlisten = await listen<string>("capture-error", (event) => {
          setError(`System audio capture stopped: ${event.payload}`);
          setCapturing(false);
          setIsProcessing(false);
          setIsAIProcessing(false);
          setIsContinuousMode(false);
          setIsRecordingInContinuousMode(false);
          setRecordingProgress(0);
          setVadMetrics(null);
          setIsPopoverOpen(true);
        });
      } catch (err) {
        console.error("Failed to setup continuous recording listeners:", err);
      }
    };

    setupContinuousListeners();

    return () => {
      if (progressUnlisten) progressUnlisten();
      if (startUnlisten) startUnlisten();
      if (stopUnlisten) stopUnlisten();
      if (errorUnlisten) errorUnlisten();
      if (discardedUnlisten) discardedUnlisten();
      if (metricsUnlisten) metricsUnlisten();
      if (captureErrorUnlisten) captureErrorUnlisten();
      if (discardedTimeoutRef.current) {
        clearTimeout(discardedTimeoutRef.current);
      }
      if (skippedTimeoutRef.current) {
        clearTimeout(skippedTimeoutRef.current);
      }
    };
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

  // Continuous mode only has a backend while recording (and answering it).
  const backendLiveRef = useRef(false);
  backendLiveRef.current =
    capturing && (vadConfig.enabled || isRecordingInContinuousMode);
  useEffect(() => {
    if (!backendLiveRef.current) return;
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
        console.error("Failed to save context settings:", error);
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
      console.error("Failed to save quick actions:", error);
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

  // Start continuous recording manually
  const startContinuousRecording = useCallback(async () => {
    try {
      setRecordingProgress(0);
      setError("");

      // Stop any existing capture (auto-listen / leftover task) before starting a new one
      await invoke<string>("stop_system_audio_capture");
      await startBackend(vadConfig);
    } catch (err) {
      console.error("Failed to start continuous recording:", err);
      setError(`Failed to start recording: ${err}`);
    }
  }, [vadConfig, startBackend]);

  // Ignore current recording (stop without transcription)
  const ignoreContinuousRecording = useCallback(async () => {
    try {
      if (!isContinuousMode || !isRecordingInContinuousMode) return;

      // Stop the capture without processing
      await invoke<string>("stop_system_audio_capture");

      // Reset states
      setRecordingProgress(0);
      setIsProcessing(false);
      setIsRecordingInContinuousMode(false);
    } catch (err) {
      console.error("Failed to ignore recording:", err);
      setError(`Failed to ignore recording: ${err}`);
    }
  }, [isContinuousMode, isRecordingInContinuousMode]);

  const startCapture = useCallback(async () => {
    try {
      setError("");

      const hasAccess = await invoke<boolean>("check_system_audio_access");
      if (!hasAccess) {
        setSetupRequired(true);
        setIsPopoverOpen(true);
        return;
      }

      const isContinuous = !vadConfig.enabled;

      resetConversation();

      setCapturing(true);
      setIsPopoverOpen(true);
      setIsContinuousMode(isContinuous);
      setRecordingProgress(0);
      setVadMetrics(null);
      setDiscardedNotice("");

      // If continuous mode
      if (isContinuous) {
        setIsRecordingInContinuousMode(false);
        return;
      }

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

  // Manual stop for continuous recording
  const manualStopAndSend = useCallback(async () => {
    try {
      if (!isContinuousMode) {
        console.warn("Not in continuous mode");
        return;
      }

      // Show processing state immediately
      setIsProcessing(true);

      // Trigger manual stop event
      await invoke("manual_stop_continuous");
    } catch (err) {
      const errorMessage = err instanceof Error ? err.message : String(err);
      setError(`Failed to manually stop: ${errorMessage}`);
      setIsProcessing(false); // Clear processing state on error
      console.error("Manual stop error:", err);
    }
  }, [isContinuousMode]);

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
      setError("Failed to request access. Please try the manual steps below.");
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
      invoke("stop_system_audio_capture").catch(() => {});
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
    if (capturing && vadConfig.enabled) {
      try {
        await invoke("stop_system_audio_capture");
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
        const modeChanged = config.enabled !== vadConfig.enabled;
        setVadConfig(config);
        safeLocalStorage.setItem("vad_config_v2", JSON.stringify(config));
        await invoke("update_vad_config", { config });

        // Switching modes mid-session must also switch the backend capture:
        // the running VAD loop keeps listening (and auto-transcribing) until
        // explicitly stopped, so Manual mode would otherwise stay hot.
        if (modeChanged && capturing) {
          await invoke("stop_system_audio_capture");
          setIsRecordingInContinuousMode(false);
          setVadMetrics(null);
          if (config.enabled) {
            await startBackend(config);
          }
        }
      } catch (error) {
        console.error("Failed to update VAD config:", error);
      }
    },
    [vadConfig.enabled, capturing, startBackend]
  );

  // Explicit calibration: stop capturing if needed, sample ambient audio,
  // bake the resulting thresholds into vadConfig (which persists via the
  // existing localStorage save in updateVadConfiguration).
  const calibrateVad = useCallback(
    async (durationSecs: number = 3) => {
      if (isCalibrating) return;
      setCalibrationError("");
      setIsCalibrating(true);
      const wasCapturing = capturing;
      try {
        // Release the audio device before calibrating.
        if (wasCapturing) {
          try {
            await invoke("stop_system_audio_capture");
            setCapturing(false);
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
        await updateVadConfiguration({
          ...vadConfig,
          sensitivity_rms: result.sensitivity_rms,
          peak_threshold: result.peak_threshold,
          noise_gate_threshold: result.noise_gate_threshold,
        });

        // Restart capture if we paused it, picking up the new thresholds.
        if (wasCapturing) {
          try {
            setCapturing(true);
            await startBackend({
              ...vadConfig,
              sensitivity_rms: result.sensitivity_rms,
              peak_threshold: result.peak_threshold,
              noise_gate_threshold: result.noise_gate_threshold,
            });
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
    if (capturing) {
      setIsContinuousMode(!vadConfig.enabled);

      if (!vadConfig.enabled) {
        setIsRecordingInContinuousMode(false);
      }
    }
  }, [vadConfig.enabled, capturing]);

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
      if (isProcessing || isAIProcessing) return;

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
        !e.ctrlKey &&
        !(e.target instanceof HTMLInputElement) &&
        !(e.target instanceof HTMLTextAreaElement)
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
    isProcessing,
    isAIProcessing,
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
