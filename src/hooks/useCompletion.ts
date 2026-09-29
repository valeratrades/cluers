import { useState, useCallback, useRef, useEffect } from "react";
import { useWindowResize } from "./useWindow";
import { useGlobalShortcuts } from "@/hooks";
import { MAX_FILES } from "@/config";
import { useApp } from "@/contexts";
import {
  appendTurn,
  loadConversation as loadConversationFromDb,
  getResponseSettings,
  buildEnhancedSystemPrompt,
  resolveProviderInput,
} from "@/lib";
import type { AttachedFile, ChatConversation, ChatMessage } from "@/types";
import { useScreenshotCapture, useStream } from "./useCompletionCore";

interface CompletionState {
  input: string;
  response: string;
  isLoading: boolean;
  error: string | null;
  currentConversationId: string | null;
  conversationHistory: ChatMessage[];
}

export const useCompletion = () => {
  const {
    selectedAIProvider,
    allAiProviders,
    systemPrompt,
    screenshotConfiguration,
    setScreenshotConfiguration,
    attachedFiles,
    addAttachedFile,
    addAttachedScreenshot,
    removeAttachedFile,
    clearAttachedFiles,
    handleAttachedFileSelect,
    handleAttachedPaste,
    isFilesPopoverOpen,
    setIsFilesPopoverOpen,
  } = useApp();
  const globalShortcuts = useGlobalShortcuts();
  const stream = useStream();

  const [state, setState] = useState<CompletionState>({
    input: "",
    response: "",
    isLoading: false,
    error: null,
    currentConversationId: null,
    conversationHistory: [],
  });
  const [micOpen, setMicOpen] = useState(false);
  const [enableVAD, setEnableVAD] = useState(false);
  const [messageHistoryOpen, setMessageHistoryOpen] = useState(false);
  const [keepEngaged, setKeepEngaged] = useState(false);
  const inputRef = useRef<HTMLInputElement | null>(null);
  const scrollAreaRef = useRef<HTMLDivElement>(null);

  const { resizeWindow } = useWindowResize();

  const setInput = useCallback((value: string) => {
    setState((prev) => ({ ...prev, input: value }));
  }, []);

  const setResponse = useCallback((value: string) => {
    setState((prev) => ({ ...prev, response: value }));
  }, []);

  const setError = useCallback((error: string) => {
    setState((prev) => ({ ...prev, error }));
  }, []);

  const runTurn = useCallback(
    async (message: string, files: AttachedFile[]): Promise<boolean> => {
      setState((prev) => ({
        ...prev,
        input: message,
        isLoading: true,
        error: null,
        response: "",
      }));
      try {
        const full = await stream.run(
          {
            provider: await resolveProviderInput(selectedAIProvider, allAiProviders),
            message,
            systemPrompt: buildEnhancedSystemPrompt(systemPrompt || undefined),
            history: state.conversationHistory.map(({ role, content }) => ({ role, content })),
            attachedFiles: files,
          },
          (d) => setState((prev) => ({ ...prev, response: prev.response + d }))
        );
        if (full === null) return false;
        const turn = await appendTurn(state.currentConversationId, {
          user: message,
          attachedFiles: files,
          assistant: full,
        });
        setState((prev) => ({
          ...prev,
          currentConversationId: turn.conversationId,
          conversationHistory: [
            ...prev.conversationHistory,
            {
              id: turn.user.id,
              role: "user",
              content: message,
              timestamp: turn.user.timestamp,
              attachedFiles: files.length > 0 ? files : undefined,
            },
            {
              id: turn.assistant.id,
              role: "assistant",
              content: full,
              timestamp: turn.assistant.timestamp,
            },
          ],
          input: "",
          isLoading: false,
        }));
        setTimeout(() => inputRef.current?.focus(), 100);
        return true;
      } catch (e) {
        setState((prev) => ({
          ...prev,
          error: e instanceof Error ? e.message : String(e),
          isLoading: false,
        }));
        return false;
      }
    },
    [
      stream.run,
      selectedAIProvider,
      allAiProviders,
      systemPrompt,
      state.conversationHistory,
      state.currentConversationId,
    ]
  );

  const submit = useCallback(
    async (speechText?: string) => {
      const text = speechText || state.input;
      if (!text.trim()) return;
      if (await runTurn(text, attachedFiles)) clearAttachedFiles();
    },
    [state.input, attachedFiles, runTurn, clearAttachedFiles]
  );

  const cancel = useCallback(() => {
    stream.cancel();
    setState((prev) => ({ ...prev, isLoading: false }));
  }, [stream.cancel]);

  const reset = useCallback(() => {
    if (keepEngaged) {
      return;
    }
    cancel();
    setState((prev) => ({
      ...prev,
      input: "",
      response: "",
      error: null,
    }));
    clearAttachedFiles();
  }, [cancel, keepEngaged, clearAttachedFiles]);

  const applyConversation = useCallback(
    (conversation: ChatConversation) => {
      stream.cancel();
      setState((prev) => ({
        ...prev,
        currentConversationId: conversation.id,
        conversationHistory: conversation.messages,
        input: "",
        response: "",
        error: null,
        isLoading: false,
      }));
    },
    [stream.cancel]
  );

  const startNewConversation = useCallback(() => {
    stream.cancel();
    setState((prev) => ({
      ...prev,
      currentConversationId: null,
      conversationHistory: [],
      input: "",
      response: "",
      error: null,
      isLoading: false,
    }));
    clearAttachedFiles();
  }, [stream.cancel, clearAttachedFiles]);

  // Listen for conversation events from the main ChatHistory component
  useEffect(() => {
    const handleConversationSelected = async (event: any) => {
      const { id } = event.detail;
      if (!id || typeof id !== "string") {
        console.error("No conversation ID provided");
        setState((prev) => ({
          ...prev,
          error: "Invalid conversation selected",
        }));
        return;
      }
      try {
        const conversation = await loadConversationFromDb(id);
        applyConversation(conversation);
      } catch (error) {
        console.error("Failed to load conversation:", error);
        setState((prev) => ({
          ...prev,
          error: "Failed to load conversation. Please try again.",
        }));
      }
    };

    const handleNewConversation = () => {
      startNewConversation();
    };

    const handleConversationDeleted = (event: any) => {
      const deletedId = event.detail;
      if (state.currentConversationId === deletedId) {
        startNewConversation();
      }
    };

    const handleStorageChange = async (e: StorageEvent) => {
      if (e.key === "pluely-conversation-selected" && e.newValue) {
        try {
          const data = JSON.parse(e.newValue);
          const { id } = data;
          if (id && typeof id === "string") {
            const conversation = await loadConversationFromDb(id);
            applyConversation(conversation);
          }
        } catch (error) {
          console.error("Failed to parse conversation selection:", error);
        }
      }
    };

    window.addEventListener("conversationSelected", handleConversationSelected);
    window.addEventListener("newConversation", handleNewConversation);
    window.addEventListener("conversationDeleted", handleConversationDeleted);
    window.addEventListener("storage", handleStorageChange);

    return () => {
      window.removeEventListener(
        "conversationSelected",
        handleConversationSelected
      );
      window.removeEventListener("newConversation", handleNewConversation);
      window.removeEventListener(
        "conversationDeleted",
        handleConversationDeleted
      );
      window.removeEventListener("storage", handleStorageChange);
    };
  }, [applyConversation, startNewConversation, state.currentConversationId]);

  const onCapture = useCallback(
    async (shot: AttachedFile, autoPrompt: string | null) => {
      if (autoPrompt) {
        await runTurn(autoPrompt, [shot]);
      } else if (attachedFiles.length >= MAX_FILES) {
        setError(`You can only upload ${MAX_FILES} files`);
      } else {
        addAttachedScreenshot(shot.base64);
      }
    },
    [runTurn, attachedFiles.length, setError, addAttachedScreenshot]
  );

  const { captureScreenshot, isScreenshotLoading } = useScreenshotCapture(
    screenshotConfiguration,
    onCapture,
    setError
  );

  const onRemoveAllFiles = () => {
    clearAttachedFiles();
    setIsFilesPopoverOpen(false);
  };

  const handleKeyPress = (e: React.KeyboardEvent) => {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      if (!state.isLoading && state.input.trim()) {
        submit();
      }
    }
  };

  const isPopoverOpen =
    state.isLoading ||
    state.response !== "" ||
    state.error !== null ||
    keepEngaged;

  useEffect(() => {
    resizeWindow(
      isPopoverOpen || micOpen || messageHistoryOpen || isFilesPopoverOpen
    );
  }, [
    isPopoverOpen,
    micOpen,
    messageHistoryOpen,
    resizeWindow,
    isFilesPopoverOpen,
  ]);

  // Auto scroll to bottom when response updates
  useEffect(() => {
    const responseSettings = getResponseSettings();
    if (
      !keepEngaged &&
      state.response &&
      scrollAreaRef.current &&
      responseSettings.autoScroll
    ) {
      const scrollElement = scrollAreaRef.current.querySelector(
        "[data-radix-scroll-area-viewport]"
      );
      if (scrollElement) {
        scrollElement.scrollTo({
          top: scrollElement.scrollHeight,
          behavior: "smooth",
        });
      }
    }
  }, [state.response, keepEngaged]);

  // Keyboard arrow key support for scrolling
  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      if (!isPopoverOpen) return;

      const scrollElement = scrollAreaRef.current?.querySelector(
        "[data-radix-scroll-area-viewport]"
      ) as HTMLElement | null;

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

  // Keyboard shortcut for toggling keep engaged mode (Cmd+K / Ctrl+K)
  useEffect(() => {
    const handleToggleShortcut = (e: KeyboardEvent) => {
      if (!isPopoverOpen) return;

      if ((e.metaKey || e.ctrlKey) && e.key === "k") {
        e.preventDefault();
        setKeepEngaged((prev) => !prev);
        setTimeout(() => {
          inputRef.current?.focus();
        }, 100);
      }
    };

    window.addEventListener("keydown", handleToggleShortcut);
    return () => window.removeEventListener("keydown", handleToggleShortcut);
  }, [isPopoverOpen]);

  const toggleRecording = useCallback(() => {
    setEnableVAD(!enableVAD);
    setMicOpen(!micOpen);
  }, [enableVAD, micOpen]);

  // register callbacks for global shortcuts
  useEffect(() => {
    globalShortcuts.registerAudioCallback(toggleRecording);
    globalShortcuts.registerInputRef(inputRef.current);
    globalShortcuts.registerScreenshotCallback(captureScreenshot);
  }, [
    globalShortcuts.registerAudioCallback,
    globalShortcuts.registerInputRef,
    globalShortcuts.registerScreenshotCallback,
    toggleRecording,
    captureScreenshot,
    inputRef,
  ]);

  return {
    input: state.input,
    setInput,
    response: state.response,
    setResponse,
    isLoading: state.isLoading,
    error: state.error,
    attachedFiles,
    addFile: addAttachedFile,
    removeFile: removeAttachedFile,
    clearFiles: clearAttachedFiles,
    submit,
    cancel,
    reset,
    setState,
    enableVAD,
    setEnableVAD,
    micOpen,
    setMicOpen,
    currentConversationId: state.currentConversationId,
    conversationHistory: state.conversationHistory,
    startNewConversation,
    messageHistoryOpen,
    setMessageHistoryOpen,
    screenshotConfiguration,
    setScreenshotConfiguration,
    handleFileSelect: handleAttachedFileSelect,
    handleKeyPress,
    handlePaste: handleAttachedPaste,
    isPopoverOpen,
    scrollAreaRef,
    resizeWindow,
    isFilesPopoverOpen,
    setIsFilesPopoverOpen,
    onRemoveAllFiles,
    inputRef,
    captureScreenshot,
    isScreenshotLoading,
    keepEngaged,
    setKeepEngaged,
  };
};
