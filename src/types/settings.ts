export type ScreenshotMode = "auto" | "manual";

export interface ScreenshotConfig {
  mode: ScreenshotMode;
  autoPrompt: string;
  enabled: boolean;
}
