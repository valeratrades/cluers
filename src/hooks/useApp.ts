import { useEffect, useRef, useState } from "react";
import { useTitles, useSystemAudio, useGlobalShortcutListeners } from "@/hooks";
import { listen } from "@tauri-apps/api/event";
import { getShortcutsConfig } from "@/lib/storage";
import { invoke } from "@tauri-apps/api/core";
import { getPlatform } from "@/lib";

export const useApp = () => {
  const systemAudio = useSystemAudio();
  const [isHidden, setIsHidden] = useState(false);
  // Initialize title management
  useTitles();

  const [shortcutError, setShortcutError] = useState<string | null>(null);
  const capturing = useRef(false);
  capturing.current = systemAudio.capturing;
  useGlobalShortcutListeners(() => capturing.current);

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

  // the window is larger than what it paints; clicks on its transparent rest must reach the app below
  useEffect(() => {
    if (getPlatform() !== "linux") return;
    let frame = 0;
    let sent = "";
    const update = () => {
      cancelAnimationFrame(frame);
      frame = requestAnimationFrame(() => {
        const rects = [
          ...document.querySelectorAll(
            "[data-input-region], [data-radix-popper-content-wrapper]"
          ),
        ]
          .map((el) => el.getBoundingClientRect())
          .filter((r) => r.width > 0 && r.height > 0)
          .map(({ x, y, width, height }) => ({ x, y, width, height }));
        if (JSON.stringify(rects) === sent) return;
        sent = JSON.stringify(rects);
        invoke("set_input_region", { rects }).catch((e) =>
          invoke("js_log", { msg: `set_input_region failed: ${e}` })
        );
      });
    };
    const mutations = new MutationObserver(update);
    mutations.observe(document.body, {
      childList: true,
      subtree: true,
      attributes: true,
    });
    const resizes = new ResizeObserver(update);
    resizes.observe(document.body);
    update();
    return () => {
      cancelAnimationFrame(frame);
      mutations.disconnect();
      resizes.disconnect();
    };
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
