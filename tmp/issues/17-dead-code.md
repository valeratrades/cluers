# 17 Dead code sweep

Confirmed unused at audit time (re-verify; earlier phases may have changed it):
- `src/components/GetLicense.tsx`, `src/components/Promote.tsx`
- `src-tauri/src/activate.rs` (not declared as a module)
- ~~`src/hooks/useWindow.ts:90-130` `useWindowFocus`~~ (done in 11)
- `src-tauri/src/window.rs:54-71` `center_window_completely` + its `#[allow(dead_code)]`
- `src-tauri/src/shortcuts.rs:15-18` `WindowVisibility.is_hidden` `#[allow(dead_code)]`
- `cpal` dependency (unless 13 uses it)
- `useHistory.selectedConversationId` (unless 09 wired it)
- package.json: `moment`, `@bany/curl-to-json`, `react-error-boundary`, `recharts` — remove any with no remaining importer.
Then a fresh sweep for unused exports / Tauri commands / deps. Reduce `pub` surface where possible.

Added after phase 1 (issue 06 findings): `src/components/Overlay.tsx` Cancel button fires `handleCancel` twice; `ref={selectionRef}` duplicated; `devicePixelRatio || 1` fallback.
- clippy type_complexity warnings: speaker/linux.rs:48, capture.rs:304 (block `clippy -D warnings`).
