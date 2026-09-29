import { useEffect, useState } from "react";
import { useTitles, useSystemAudio, useGlobalShortcutListeners } from "@/hooks";
import { listen } from "@tauri-apps/api/event";
import { getShortcutsConfig } from "@/lib/storage";
import { invoke } from "@tauri-apps/api/core";

export const useApp = () => {
  const systemAudio = useSystemAudio();
  const [isHidden, setIsHidden] = useState(false);
  // Initialize title management
  useTitles();

  const [shortcutError, setShortcutError] = useState<string | null>(null);
  useGlobalShortcutListeners();

  useEffect(() => {
    (async () => {
      try {
        await invoke("update_shortcuts", { config: getShortcutsConfig() });
      } catch (error) {
        setShortcutError(`${error}`);
        await invoke("js_log", { msg: `shortcut init failed: ${error}` });
      }
    })();
  }, []);

  const handleSelectConversation = (conversation: any) => {
    // useCompletion will fetch the full conversation from SQLite by id
    window.dispatchEvent(
      new CustomEvent("conversationSelected", {
        detail: { id: conversation.id },
      })
    );
  };

  const handleNewConversation = () => {
    // Trigger new conversation event
    window.dispatchEvent(new CustomEvent("newConversation"));
  };

  // WINDOWS HIDE/SHOW TOGGLE WINDOW WORKAROUND FOR SHORTCUTS
  useEffect(() => {
    const unlistenPromise = listen<boolean>(
      "toggle-window-visibility",
      (event) => {
        const platform = navigator.platform.toLowerCase();
        if (typeof event.payload === "boolean" && platform.includes("win")) {
          setIsHidden(!event.payload);
          // find popover open and close it
          const popover = document.getElementById("popover-content");
          // set display to none, change data-state to closed
          if (popover) {
            popover.style.setProperty("display", "none", "important");
            // update the data-state to closed
            popover.setAttribute("data-state", "closed");

            // Also find and update the popover trigger's data-state
            const popoverTriggers = document.querySelectorAll(
              '[data-slot="popover-trigger"]'
            );
            popoverTriggers.forEach((trigger) => {
              trigger.setAttribute("data-state", "closed");
            });
          }
        }
      }
    );

    return () => {
      unlistenPromise.then((unlisten) => unlisten());
    };
  }, []);

  return {
    isHidden,
    setIsHidden,
    shortcutError,
    handleSelectConversation,
    handleNewConversation,
    systemAudio,
  };
};
