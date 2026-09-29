# 20 Keyboard input stays in the pluely window after clicking another app

Reported upstream: after clicking another app's text field, typing still goes into pluely's input.

Needs a live repro on the owner's session (sway, Wayland) before any code change. None of the fixes so far targeted it.

What changed since the report:
- `useWindowFocus` (an unused focus hook) was deleted in v0.1.11. Nothing suggested it was a fix for this.
- Issue 05 (v0.1.10) made the screenshot overlay always tear down or emit `capture-closed`. So a leaked overlay holding focus is unlikely now, but not ruled out.

Hypotheses to check, in order:
1. The main window resizes to a 600px-wide transparent area when expanded (`src-tauri/src/window.rs:44`). Clicks on "empty" screen inside it may still land in pluely.
2. The `focus-text-input` shortcut handler (`shortcuts.rs`, `handle_focus_input`) or always-on-top keeps or retakes keyboard focus.
3. A capture overlay window is still open (check `swaymsg -t get_tree` for extra pluely windows while it happens).

First step: reproduce, then capture `swaymsg -t get_tree` output showing which pluely surface holds focus.
