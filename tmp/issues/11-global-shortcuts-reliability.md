# 11 Global shortcuts / focus reliability

- `src/hooks/useGlobalShortcuts.ts:112-256`: effect without cleanup; module-level `globalEventListeners` guard races — two mounts both see the slot empty before `await listen()` resolves → both register, one leaks forever → duplicate handling of `focus-text-input`, `custom-shortcut-triggered`, etc.
- `src-tauri/src/shortcuts.rs:59-75`: `RegisteredShortcuts` empty until the frontend calls `update_shortcuts` from `useApp.ts:14-24` → presses before that are dropped. Rust should load persisted shortcuts itself (or the registration must be gated/ordered so no window exists before it).
- `src/contexts/app.context.tsx:497-520` storage-event handler omits `STORAGE_KEYS.SHORTCUTS` → other windows show stale bindings.
- `shortcuts.rs:552-561` `handle_focus_input`: `let _ = window.show(); let _ = window.set_focus(); let _ = window.emit(...)` — the "shortcut fired, nothing focused" path with zero diagnostics.
- `src/lib/storage/shortcuts.storage.ts:49-66` bypasses `safeLocalStorage`.
- Known user-facing bug class: "stuck typing in pluely window after clicking another app's text field". `src/hooks/useWindow.ts:90-130` `useWindowFocus` exists but is unused — likely an abandoned fix. Investigate on Linux (Hyprland/Wayland + X11) whether focus is being grabbed/kept (focus-text-input handler, always-on-top, panel settings); fix or delete `useWindowFocus`.
