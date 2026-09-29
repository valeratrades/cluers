import { InfoIcon, MicIcon } from "lucide-react";
import { Popover, PopoverContent, PopoverTrigger, Button } from "@/components";
import { AudioRecorder } from "@/pages/chats/components";
import { UseCompletionReturn } from "@/types";
import { useApp } from "@/contexts";

export const Audio = ({
  micOpen,
  setMicOpen,
  isRecording,
  setIsRecording,
  submit,
}: UseCompletionReturn) => {
  const { selectedSttProvider, pluelyApiEnabled } = useApp();

  const configured = pluelyApiEnabled || !!selectedSttProvider.provider;

  return (
    <Popover open={micOpen} onOpenChange={setMicOpen}>
      <PopoverTrigger asChild>
        <Button
          size="icon"
          onClick={() => setIsRecording(!isRecording)}
          className="cursor-pointer"
          title="Toggle voice input"
        >
          <MicIcon className="h-4 w-4" />
        </Button>
      </PopoverTrigger>

      <PopoverContent
        align="end"
        side="bottom"
        className={`w-80 p-3 ${configured && !isRecording ? "hidden" : ""}`}
        sideOffset={8}
      >
        {!configured ? (
          <div className="text-sm select-none">
            <div className="font-semibold text-orange-600 mb-1">
              Speech Provider Configuration Required
            </div>
            <div className="mt-2 flex flex-row gap-1 items-center text-orange-600">
              <InfoIcon size={16} />
              <p>PROVIDER IS MISSING</p>
            </div>
            <span className="block mt-2 text-muted-foreground">
              Please go to settings and configure your speech provider to
              enable voice input.
            </span>
          </div>
        ) : isRecording ? (
          <AudioRecorder
            onTranscriptionComplete={(t) => {
              setIsRecording(false);
              submit(t);
            }}
            onCancel={() => setIsRecording(false)}
          />
        ) : null}
      </PopoverContent>
    </Popover>
  );
};
