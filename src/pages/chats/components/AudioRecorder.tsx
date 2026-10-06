import { useState, useEffect, useRef } from "react";
import { Channel, invoke } from "@tauri-apps/api/core";
import { Button } from "@/components";
import { resolveProviderInput } from "@/lib/llm";
import { useApp } from "@/contexts";
import { StopCircle, Send } from "lucide-react";

interface AudioRecorderProps {
  onTranscriptionComplete: (text: string) => void;
  onCancel: () => void;
}

type PttEvent = { kind: "started" } | { kind: "level"; rms: number };
type Finish = "send" | "discard";

const MAX_DURATION = 3 * 60 * 1000; // mirrors MAX_SECS in speaker/push_to_talk.rs

export const AudioRecorder = ({
  onTranscriptionComplete,
  onCancel,
}: AudioRecorderProps) => {
  const { selectedSttProvider, allSttProviders, selectedAudioDevices } =
    useApp();
  const [started, setStarted] = useState(false);
  const [level, setLevel] = useState(0);
  const [isTranscribing, setIsTranscribing] = useState(false);
  const [duration, setDuration] = useState(0);
  const [error, setError] = useState("");
  const finishedRef = useRef(false);

  const finish = (action: Finish) => {
    if (finishedRef.current) return;
    finishedRef.current = true;
    invoke("finish_push_to_talk", { action }).catch((e) =>
      setError(`Failed to stop recording: ${e}`)
    );
  };

  useEffect(() => {
    let mounted = true;
    let timer: ReturnType<typeof setInterval> | undefined;
    const events = new Channel<PttEvent>();
    events.onmessage = (e) => {
      if (e.kind === "level") return setLevel(e.rms);
      if (!mounted) return finish("discard"); // unmounted before Rust could accept it
      const startedAt = Date.now();
      timer = setInterval(() => setDuration(Date.now() - startedAt), 100);
      setStarted(true);
    };

    (async () => {
      const stt = await resolveProviderInput(selectedSttProvider, allSttProviders);
      const deviceId = selectedAudioDevices.input.id;
      const text = await invoke<string | null>("record_push_to_talk", {
        deviceId: deviceId === "default" ? null : deviceId,
        stt,
        events,
      });
      if (!mounted || text === null) return;
      if (!text.trim()) throw new Error("No speech recognized");
      onTranscriptionComplete(text);
    })()
      .catch((e) => mounted && setError(`${e}`))
      .finally(() => {
        finishedRef.current = true; // the recording is over either way
        clearInterval(timer);
        if (mounted) setIsTranscribing(false);
      });

    return () => {
      mounted = false;
      if (timer) finish("discard");
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const handleSend = () => {
    setIsTranscribing(true);
    finish("send");
  };

  const formatTime = (ms: number) => {
    const seconds = Math.floor(ms / 1000);
    const mins = Math.floor(seconds / 60);
    const secs = seconds % 60;
    return `${mins}:${secs.toString().padStart(2, "0")}`;
  };

  return (
    <div className="border bg-background rounded-lg overflow-hidden">
      <div className="h-12 relative bg-muted/20">
        {error ? (
          <div className="h-full flex items-center justify-center px-4 text-xs text-red-500 truncate" title={error}>
            {error}
          </div>
        ) : isTranscribing ? (
          <div className="h-full flex items-center justify-center text-sm text-muted-foreground">
            Transcribing...
          </div>
        ) : started ? (
          <div className="h-full flex items-center px-4">
            <div
              className="h-2 rounded-full bg-primary transition-[width] duration-75"
              style={{ width: `${Math.min(100, Math.sqrt(level) * 250)}%` }}
            />
          </div>
        ) : (
          <div className="h-full flex items-center justify-center text-sm text-muted-foreground">
            Initializing...
          </div>
        )}
      </div>
      <div className="flex items-center justify-between px-4 py-2.5 border-t bg-muted/5">
        <div className={`flex items-center gap-2 ${error ? "invisible" : ""}`}>
          <div className="h-2 w-2 bg-red-500 rounded-full animate-pulse" />
          <span className="text-sm font-mono tabular-nums font-medium">
            {formatTime(duration)}
          </span>
          <span className="text-xs text-muted-foreground">
            / {formatTime(MAX_DURATION)}
          </span>
        </div>
        <div className="flex items-center gap-2">
          <Button
            size="icon"
            variant="outline"
            onClick={onCancel}
            disabled={isTranscribing}
            className="h-8 w-8"
            title="Stop recording"
          >
            <StopCircle className="h-4 w-4" />
          </Button>
          <Button
            size="icon"
            onClick={handleSend}
            disabled={!started || isTranscribing || !!error}
            className="h-8 w-8"
            title={isTranscribing ? "Sending..." : "Send to AI"}
          >
            <Send className="h-4 w-4" />
          </Button>
        </div>
      </div>
    </div>
  );
};
