import { STORAGE_KEYS, DEFAULT_SHORTCUT_ACTIONS } from "@/config";
import {
  ShortcutsConfig,
  ShortcutBinding,
  ShortcutConflict,
  ShortcutAction,
} from "@/types";
import { getPlatform } from "@/lib";

/**
 * Get platform-specific default key for a shortcut action
 */
export const getPlatformDefaultKey = (action: ShortcutAction): string => {
  const platform = getPlatform();

  switch (platform) {
    case "macos":
      return action.defaultKey.macos;
    case "windows":
      return action.defaultKey.windows;
    case "linux":
      return action.defaultKey.linux;
  }
};

/**
 * Get default shortcuts configuration
 */
export const getDefaultShortcutsConfig = (): ShortcutsConfig => {
  const bindings: Record<string, ShortcutBinding> = {};

  DEFAULT_SHORTCUT_ACTIONS.forEach((action) => {
    bindings[action.id] = {
      action: action.id,
      key: getPlatformDefaultKey(action),
      enabled: true,
    };
  });

  return { bindings };
};

/**
 * Get shortcuts configuration from localStorage; throws on corrupt JSON
 */
export const getShortcutsConfig = (): ShortcutsConfig => {
  const defaults = getDefaultShortcutsConfig();
  const stored = localStorage.getItem(STORAGE_KEYS.SHORTCUTS);
  if (!stored) return defaults;
  const parsed: ShortcutsConfig = JSON.parse(stored);
  return { bindings: { ...defaults.bindings, ...parsed.bindings } };
};

export const setShortcutsConfig = (config: ShortcutsConfig): void =>
  localStorage.setItem(STORAGE_KEYS.SHORTCUTS, JSON.stringify(config));

/**
 * Update a single shortcut binding
 */
export const updateShortcutBinding = (
  actionId: string,
  key: string,
  enabled: boolean = true
): ShortcutsConfig => {
  const config = getShortcutsConfig();
  config.bindings[actionId] = {
    action: actionId,
    key,
    enabled,
  };
  setShortcutsConfig(config);
  return config;
};

/**
 * Reset shortcuts to platform defaults
 */
export const resetShortcutsToDefaults = (): ShortcutsConfig => {
  const defaults = getDefaultShortcutsConfig();
  setShortcutsConfig(defaults);
  return defaults;
};

/**
 * Check for shortcut conflicts
 */
export const checkShortcutConflicts = (
  key: string,
  excludeActionId?: string
): ShortcutConflict | null => {
  const config = getShortcutsConfig();
  const conflicts: string[] = [];

  Object.entries(config.bindings).forEach(([actionId, binding]) => {
    if (
      binding.key.toLowerCase() === key.toLowerCase() &&
      binding.enabled &&
      actionId !== excludeActionId
    ) {
      conflicts.push(actionId);
    }
  });

  if (conflicts.length > 0) {
    return { key, actions: conflicts };
  }

  return null;
};

/**
 * Validate shortcut key format
 */
export const validateShortcutKey = (key: string): boolean => {
  const parts = key
    .toLowerCase()
    .split("+")
    .map((p) => p.trim());

  if (parts.length < 2) return false;

  const modifiers = ["cmd", "ctrl", "alt", "shift"];
  const hasModifier = parts.some((p) => modifiers.includes(p));
  if (!hasModifier) return false;

  // Validate all parts are known modifiers or valid keys
  const validKeys = [
    ...modifiers,
    // Letters
    "a",
    "b",
    "c",
    "d",
    "e",
    "f",
    "g",
    "h",
    "i",
    "j",
    "k",
    "l",
    "m",
    "n",
    "o",
    "p",
    "q",
    "r",
    "s",
    "t",
    "u",
    "v",
    "w",
    "x",
    "y",
    "z",
    // Numbers
    "0",
    "1",
    "2",
    "3",
    "4",
    "5",
    "6",
    "7",
    "8",
    "9",
    // Function keys
    "f1",
    "f2",
    "f3",
    "f4",
    "f5",
    "f6",
    "f7",
    "f8",
    "f9",
    "f10",
    "f11",
    "f12",
    // Special keys
    "space",
    "return",
    "enter",
    "tab",
    "backspace",
    "delete",
    "esc",
    "escape",
    "up",
    "down",
    "left",
    "right",
    "backslash",
    "slash",
    "comma",
    "period",
    "minus",
    "equal",
    "plus",
    "bracketleft",
    "bracketright",
    "semicolon",
    "quote",
    "grave",
  ];

  return parts.every((p) => validKeys.includes(p));
};

/**
 * Format shortcut key for display
 */
export const formatShortcutKeyForDisplay = (key: string): string => {
  return key
    .split("+")
    .map((part) => {
      const trimmed = part.trim();
      // Capitalize first letter
      return trimmed.charAt(0).toUpperCase() + trimmed.slice(1);
    })
    .join(" + ");
};
