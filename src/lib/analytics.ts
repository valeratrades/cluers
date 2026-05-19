import { PostHog } from "tauri-plugin-posthog-api";

/**
 * Event names for tracking
 */
export const ANALYTICS_EVENTS = {
  // App Lifecycle
  APP_STARTED: "app_started",
  // License Events
  GET_LICENSE: "get_license",
} as const;

/**
 * Capture an analytics event
 */
export const captureEvent = async (
  eventName: string,
  properties?: Record<string, any>
) => {
  try {
    await PostHog.capture(eventName, properties || {});
  } catch (error) {
    // Silently fail - we don't want analytics to break the app
    console.debug("Analytics event failed:", eventName, error);
  }
};

/**
 * Track app initialization.
 *
 * Pluely's per-install instance_id used to be attached here, but it lives
 * in the OS keychain now and is intentionally not readable from JS (see
 * `llm/secrets.rs`). If we want it back on this event, the right move is
 * a small Rust-side capture using the credentials already in scope there.
 */
export const trackAppStart = async (appVersion: string) => {
  await captureEvent(ANALYTICS_EVENTS.APP_STARTED, {
    app_version: appVersion,
    platform: navigator.platform,
  });
};
