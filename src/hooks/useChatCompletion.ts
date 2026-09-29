import {
  useState,
  useCallback,
  useRef,
  type Dispatch,
  type SetStateAction,
} from "react";
import { useApp } from "@/contexts";
import { MAX_FILES } from "@/config";
import {
  appendTurn,
  getResponseSettings,
  buildEnhancedSystemPrompt,
  resolveProviderInput,
} from "@/lib";
import type { AttachedFile, ChatConversation, ChatMessage } from "@/types";
import { useScreenshotCapture, useStream } from "./useCompletionCore";

interface ChatCompletionState {
  input: string;
  isLoading: boolean;
  error: string | null;
}

export const useChatCompletion = (
  conversationId: string,
  messages: ChatConversation | null,
  setMessages: Dispatch<SetStateAction<ChatConversation | null>>
) => {
  const {
    selectedAIProvider,
    allAiProviders,
    systemPrompt,
    screenshotConfiguration,
    attachedFiles,
    addAttachedScreenshot,
    removeAttachedFile,
    clearAttachedFiles,
    handleAttachedFileSelect,
    handleAttachedPaste,
    isFilesPopoverOpen,
    setIsFilesPopoverOpen,
  } = useApp();
  const stream = useStream();

  const [state, setState] = useState<ChatCompletionState>({
    input: "",
    isLoading: false,
    error: null,
  });
  const [micOpen, setMicOpen] = useState(false);
  const [isRecording, setIsRecording] = useState(false);

  const inputRef = useRef<HTMLTextAreaElement | null>(null);
  const messagesEndRef = useRef<HTMLDivElement>(null);

  const scrollToBottom = () => {
    if (getResponseSettings().autoScroll) {
      messagesEndRef.current?.scrollIntoView({ behavior: "smooth" });
    }
  };

  const setInput = useCallback((value: string) => {
    setState((prev) => ({ ...prev, input: value }));
  }, []);

  const setError = useCallback((error: string) => {
    setState((prev) => ({ ...prev, error }));
  }, []);

  const runTurn = useCallback(
    async (message: string, files: AttachedFile[]) => {
      if (!messages) throw new Error("turn submitted before the conversation loaded");
      const key = crypto.randomUUID();
      const uId = `pending_user_${key}`;
      const aId = `pending_assistant_${key}`;
      const patch = (f: (ms: ChatMessage[]) => ChatMessage[], updatedAt?: number) =>
        setMessages((prev) => {
          if (!prev) throw new Error("conversation unloaded mid-turn");
          return { ...prev, updatedAt: updatedAt ?? prev.updatedAt, messages: f(prev.messages) };
        });
      const dropPending = () => patch((ms) => ms.filter((m) => m.id !== uId && m.id !== aId));

      patch((ms) => [
        ...ms,
        {
          id: uId,
          role: "user",
          content: message,
          timestamp: Date.now(),
          attachedFiles: files.length > 0 ? files : undefined,
        },
      ]);
      setState((prev) => ({ ...prev, input: "", isLoading: true, error: null }));
      setTimeout(scrollToBottom, 100);

      let text = "";
      try {
        const full = await stream.run(
          {
            provider: await resolveProviderInput(selectedAIProvider, allAiProviders),
            message,
            systemPrompt: buildEnhancedSystemPrompt(systemPrompt || undefined),
            history: messages.messages.map(({ role, content }) => ({ role, content })),
            attachedFiles: files,
          },
          (d) => {
            text += d;
            patch((ms) =>
              ms.some((m) => m.id === aId)
                ? ms.map((m) => (m.id === aId ? { ...m, content: text } : m))
                : [...ms, { id: aId, role: "assistant", content: text, timestamp: Date.now() }]
            );
            scrollToBottom();
          }
        );
        if (full === null) {
          dropPending();
          return;
        }
        const turn = await appendTurn(conversationId, {
          user: message,
          attachedFiles: files,
          assistant: full,
        });
        patch(
          (ms) =>
            ms.map((m) =>
              m.id === uId
                ? { ...m, id: turn.user.id, timestamp: turn.user.timestamp }
                : m.id === aId
                  ? { ...m, id: turn.assistant.id, timestamp: turn.assistant.timestamp, content: full }
                  : m
            ),
          turn.assistant.timestamp
        );
        setState((prev) => ({ ...prev, isLoading: false }));
        setTimeout(() => inputRef.current?.focus(), 100);
      } catch (e) {
        dropPending();
        setState((prev) => ({
          ...prev,
          input: message,
          isLoading: false,
          error: e instanceof Error ? e.message : String(e),
        }));
      }
    },
    [messages, setMessages, stream.run, selectedAIProvider, allAiProviders, systemPrompt, conversationId]
  );

  const submit = useCallback(
    async (speechText?: string) => {
      const text = speechText || state.input;
      if (!text.trim()) return;
      const files = attachedFiles;
      clearAttachedFiles();
      await runTurn(text, files);
    },
    [state.input, attachedFiles, clearAttachedFiles, runTurn]
  );

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

  return {
    input: state.input,
    setInput,
    isLoading: state.isLoading,
    error: state.error,
    attachedFiles,
    removeFile: removeAttachedFile,
    submit,
    isRecording,
    setIsRecording,
    micOpen,
    setMicOpen,
    screenshotConfiguration,
    handleFileSelect: handleAttachedFileSelect,
    handleKeyPress,
    handlePaste: handleAttachedPaste,
    isFilesPopoverOpen,
    setIsFilesPopoverOpen,
    onRemoveAllFiles,
    inputRef,
    captureScreenshot,
    isScreenshotLoading,
    messagesEndRef,
  };
};
