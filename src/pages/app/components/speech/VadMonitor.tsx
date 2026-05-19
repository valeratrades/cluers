import { VadMetrics } from "@/hooks/useSystemAudio";
import { ActivityIcon, AlertTriangleIcon } from "lucide-react";
import { cn } from "@/lib/utils";

interface VadMonitorProps {
  metrics: VadMetrics | null;
  discardedNotice: string;
}

// Map a linear amplitude (0..~0.3) onto a 0..1 bar position. Audio is
// perceptually log-scale; using a soft curve keeps quiet ambient noise
// visible without making loud speech peg the meter.
function levelToBar(value: number): number {
  if (value <= 0) return 0;
  // Compresses 0..0.3 into 0..1 with emphasis on the lower range.
  return Math.min(1, Math.sqrt(value / 0.3));
}

export const VadMonitor = ({
  metrics,
  discardedNotice,
}: VadMonitorProps) => {
  // Render even before the first metric arrives so the user sees the slot.
  const rms = metrics?.rms ?? 0;
  const peak = metrics?.peak ?? 0;
  const sensitivity = metrics?.sensitivity_rms ?? 0;
  const peakThreshold = metrics?.peak_threshold ?? 0;
  const inSpeech = metrics?.in_speech ?? false;

  const rmsBar = levelToBar(rms);
  const peakBar = levelToBar(peak);
  const sensitivityMark = levelToBar(sensitivity);
  const peakMark = levelToBar(peakThreshold);

  return (
    <div className="rounded-lg border border-border/50 bg-muted/30 p-3 space-y-2">
      <div className="flex items-center justify-between">
        <div className="flex items-center gap-1.5">
          <ActivityIcon
            className={cn(
              "w-3 h-3",
              inSpeech ? "text-green-500" : "text-muted-foreground"
            )}
          />
          <span className="text-[10px] font-medium uppercase tracking-wide text-muted-foreground">
            Audio Level
          </span>
        </div>
        {inSpeech && (
          <span className="text-[9px] font-medium text-green-600">
            speech detected
          </span>
        )}
      </div>

      {/* RMS bar with sensitivity threshold marker */}
      <div className="space-y-1">
        <div className="flex justify-between text-[9px] text-muted-foreground">
          <span>RMS {(rms * 1000).toFixed(1)}</span>
          <span>threshold {(sensitivity * 1000).toFixed(1)}</span>
        </div>
        <div className="relative w-full h-2 bg-muted rounded overflow-hidden">
          <div
            className={cn(
              "h-full transition-[width] duration-75",
              rms > sensitivity ? "bg-green-500" : "bg-blue-400"
            )}
            style={{ width: `${rmsBar * 100}%` }}
          />
          {sensitivity > 0 && (
            <div
              className="absolute top-0 bottom-0 w-px bg-red-500"
              style={{ left: `${sensitivityMark * 100}%` }}
              title="Sensitivity threshold"
            />
          )}
        </div>
      </div>

      {/* Peak bar with peak threshold marker */}
      <div className="space-y-1">
        <div className="flex justify-between text-[9px] text-muted-foreground">
          <span>Peak {(peak * 1000).toFixed(1)}</span>
          <span>threshold {(peakThreshold * 1000).toFixed(1)}</span>
        </div>
        <div className="relative w-full h-1.5 bg-muted rounded overflow-hidden">
          <div
            className={cn(
              "h-full transition-[width] duration-75",
              peak > peakThreshold ? "bg-green-500" : "bg-blue-400"
            )}
            style={{ width: `${peakBar * 100}%` }}
          />
          {peakThreshold > 0 && (
            <div
              className="absolute top-0 bottom-0 w-px bg-red-500"
              style={{ left: `${peakMark * 100}%` }}
              title="Peak threshold"
            />
          )}
        </div>
      </div>

      {/* Discarded notice (transient) */}
      {discardedNotice && (
        <div className="flex items-start gap-1.5 pt-1 border-t border-border/50">
          <AlertTriangleIcon className="w-3 h-3 text-amber-500 mt-0.5 flex-shrink-0" />
          <p className="text-[9px] text-amber-700 leading-snug">
            Speech segment discarded: {discardedNotice}
          </p>
        </div>
      )}
    </div>
  );
};
