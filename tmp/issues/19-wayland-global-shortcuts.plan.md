## Plan for issue 19: global shortcuts on Wayland

### What I checked in the owner's session (this changes the plan)

- The session is **sway 1.11** (`XDG_CURRENT_DESKTOP=sway`), not Hyprland.
- The portals running are xdg-desktop-portal 1.20.4, plus the -gtk, -wlr and -termfilechooser backends.
- `busctl --user introspect org.freedesktop.portal.Desktop /org/freedesktop/portal/desktop` does **not** list `org.freedesktop.portal.GlobalShortcuts`. None of the installed `*.portal` files declare `impl.portal.GlobalShortcuts`. xdg-desktop-portal-wlr does not implement it, and sway has no global-shortcuts protocol at all.
- So a portal backend would change nothing for the owner.
- On Hyprland, xdph does expose the portal, but it ignores `preferred_trigger`. The user still has to write `bind = MODS, KEY, global, <app>:<id>` in hyprland.conf.
- Only KDE and GNOME 48+ actually honour the proposed triggers.

On every wlroots compositor the binding lives in the compositor config anyway. The portable fix is to let the compositor run a command that fires an action in the running instance: `bindsym ctrl+backslash exec pluely toggle_window`. This works on sway, Hyprland, niri, river, GNOME custom keybinds and KDE, without portal sessions or a long-lived D-Bus task. It also keeps the "no detached task" rule.

**Recommendation:** ship the CLI trigger (below). Defer the portal (see the end) until someone on KDE or GNOME asks for it.

### Design

`tauri-plugin-single-instance` (Linux-only target dependency) does the forwarding. It already uses zbus 5, which is in Cargo.lock via tauri-plugin-opener, so it adds almost nothing to the dependency tree.
- A second `pluely <action_id>` process sends its argv to the running instance over D-Bus, then exits 0.
- The running instance dispatches the argv through the same `Action` → `run()` path the X11 hotkeys use.
- The X11 plugin stays registered: under XWayland focus it still works, and on X11 sessions it is the main path.
- Side effect: launching Pluely a second time no longer starts a second copy. Today two copies would fight over the X grabs, so this is wanted.

### Steps

**1. Failing test first: `src-tauri/tests/cli.rs` (new).**
- Test through the process interface, data-driven.
- For each row in
  ```rust
  [(&["bogus"][..], "bogus"), (&["move_window"], "move_window"), (&["toggle_window", "extra"], "extra")]
  ```
  run
  ```rust
  Command::new(env!("CARGO_BIN_EXE_pluely")).args(args).env_remove("DISPLAY").env_remove("WAYLAND_DISPLAY").output()
  ```
  and assert `status.code() == Some(2)` and that stderr contains the offender.
- Removing the display variables makes the pre-change binary panic quickly in GTK init (exit 101) instead of opening a window. The test therefore fails deterministically before the change.
- After the change, rejection happens before tracing, the DB or any GUI work.
- Valid ids cannot be tested headless: they would start the GUI or poke a real running instance. They are covered by manual verification.

**2. `src-tauri/Cargo.toml`**
- Under `[target.'cfg(target_os = "linux")'.dependencies]`, add `tauri-plugin-single-instance = "2"`.
- Run `nix develop -c cargo update -p tauri-plugin-single-instance --manifest-path src-tauri/Cargo.toml` only if the lock needs it. Check that the resolved version uses zbus 5 (`cargo tree -i zbus` shows one version).

**3. `src-tauri/src/shortcuts.rs`**
- Change `enum Action` to `pub(crate) enum Action`. It appears in a crate-visible signature, which avoids the `private_interfaces` lint.
- Add two `pub(crate)` functions (Linux-only via `#[cfg(target_os = "linux")]`, so there is no dead code on other platforms):
  ```rust
  /// `pluely <action_id>` fires the action in the running instance; compositor binds use this on Wayland.
  pub(crate) fn cli_action(argv: &[String]) -> Result<Option<Action>, String> {
      match argv {
          [_] => Ok(None),
          [_, id] => id.parse().map(Some),
          _ => Err(format!("usage: pluely [<action_id>], got {:?}", &argv[1..])),
      }
  }

  pub(crate) fn run_cli_action<R: Runtime>(app: &AppHandle<R>, argv: &[String]) {
      let result = cli_action(argv).and_then(|a| match a {
          Some(a) => run(app, a).map_err(|e| format!("{a:?}: {e:#}")),
          None => Ok(()), // bare relaunch: the instance is already up
      });
      if let Err(e) = result {
          tracing::error!("cli shortcut failed: {e}"); // the sending process has already exited: nobody to return to
      }
  }
  ```
- `move_window` is rejected by the existing `FromStr` because it needs a key release. That is intended, and the error message already names it.
- Reuse `run()` unchanged. Do not route through `on_event`, because there is no Pressed/Released pair here.

**4. `src-tauri/src/lib.rs`**
- First statement of `run()`:
  ```rust
  #[cfg(target_os = "linux")]
  if let Err(e) = shortcuts::cli_action(&std::env::args().collect::<Vec<_>>()) {
      eprintln!("{e}");
      std::process::exit(2);
  }
  ```
- The plugin must be the first plugin registered, per the plugin docs. `builder` must be `let mut` or rebound. Follow the existing macOS `builder = builder.plugin(...)` pattern: rebind with `#[cfg(target_os = "linux")] let builder = builder.plugin(tauri_plugin_single_instance::init(|app, argv, _| shortcuts::run_cli_action(app, &argv)));` placed right after `tauri::Builder::default()`, before the `.manage` calls. The simplest way is to split the chain there.
- Replace the Wayland warning text:
  `"global shortcuts use X11 key grabs and fire under Wayland only while an XWayland window has focus; bind `pluely <action_id>` in your compositor config instead"`.
- A valid action id given to the *first* instance only starts the app (`[_, id]` passes validation, and nothing fires). This is acceptable because starting the app is what the user sees. See the trade-offs.

**5. UI surfacing: `src/pages/shortcuts/components/shortcuts/ShortcutManager.tsx`**
- Replace the footer note with this: when `getPlatform() === "linux"` (from `@/lib`; confirm the literal it returns), show "On Wayland, bind `pluely <action>` in your compositor (sway: `bindsym ctrl+backslash exec pluely toggle_window`)".
- Also show the list of ids: `DEFAULT_SHORTCUT_ACTIONS.filter(a => a.id !== "move_window").map(a => a.id).join(", ")`.
- Do not add a new command or Wayland detection.

**6. `ARCHITECTURE.md`, "Global shortcuts" section.** Replace the last bullet with:
- Linux: X11 key grabs (global-hotkey 0.7) plus a CLI trigger. `pluely <action_id>` is forwarded to the running instance by tauri-plugin-single-instance (D-Bus) and runs the same `run()`.
- This is the Wayland path, because the GlobalShortcuts portal is missing on wlroots (sway) and does not apply triggers on Hyprland.
- Invalid argv exits with code 2 before startup. `move_window` is X11-only.

### Verification
- `nix develop -c cargo test --manifest-path src-tauri/Cargo.toml` fails at step 1 and passes after steps 2–4.
- `nix develop -c cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings`
- `nix develop -c npx tsc --noEmit` (or the repo's lint script from package.json).
- Manual check on sway:
  1. Start the app (`nix develop -c npm run tauri dev`).
  2. `swaymsg 'bindsym ctrl+backslash exec <path-to>/pluely toggle_window'`, then press it: the main window toggles.
  3. Repeat for `toggle_dashboard`, `screenshot`, `focus_input` and `system_audio`.
  4. `pluely bogus` prints usage and exits 2.
  5. A bare `pluely` while it is running starts no second window.

### Deferred: portal backend (only if the owner wants KDE/GNOME auto-binding or press/release `move_window` on Wayland)
- Use `ashpd` (0.11, zbus 5) `desktop::global_shortcuts`: `GlobalShortcuts::new()`, `create_session()`, `bind_shortcuts(&session, &[NewShortcut::new(id, desc).preferred_trigger(..)], None)` (response = actual triggers, send them to the UI), then `receive_activated()` / `receive_deactivated()` streams mapped to `on_event(Pressed/Released)`.
- Unsandboxed apps need `register_host_app` with a desktop-file id first.
- It needs a long-lived stream task. That would be a second sanctioned spawn, with its `JoinHandle` owned in state like `MoveWindowState` and aborted on `update_shortcuts`.
- It also needs a converter from global-hotkey key strings to the XDG shortcut syntax.
- Adds roughly 150 LOC plus a dependency for users who are not the owner. Skipped for now.
## Review amendments (orchestrator)
- Approved: a CLI trigger instead of the portal, since the owner runs sway and no wlroots portal implements GlobalShortcuts. Add the exact sway `bindsym ... exec pluely --action <id>` lines for every action to ARCHITECTURE.md (or README), so the owner can paste them.
