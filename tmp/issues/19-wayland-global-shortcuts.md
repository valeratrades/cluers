# 19 Global shortcuts don't work on Wayland (Hyprland/sway)

Found by the issue-11 planner: `tauri-plugin-global-shortcut` → `global-hotkey` 0.7 is X11-only (XGrabKey on the XWayland root). In a Wayland session, shortcuts fire only while an XWayland window has focus. This is likely the dominant cause of "shortcuts not working" for the owner (Linux Wayland).

Goal: a Linux Wayland backend using the XDG Desktop Portal `org.freedesktop.portal.GlobalShortcuts` (supported by xdg-desktop-portal-hyprland and KDE; check which portal the owner's session provides via busctl/dbus-send). Keep X11 on the existing plugin. Rust stays the single registration owner (`shortcuts.rs`), and it's the same action-id → handler dispatch either way. Note that the portal flow is: CreateSession, then BindShortcuts (the user may be prompted), then the Activated signal. Bindings are compositor-controlled, so the app proposes triggers and the compositor may override them. Surface the binding result in the UI.

Consider the `ashpd` crate (it has a GlobalShortcuts portal API) before hand-rolling D-Bus.
