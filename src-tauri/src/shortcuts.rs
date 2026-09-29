use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;
use std::sync::Mutex;
use tauri::async_runtime::JoinHandle;
use tauri::{AppHandle, Emitter, Manager, Runtime};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};
use tokio::time::{sleep, Duration};

#[cfg(target_os = "macos")]
use tauri_nspanel::ManagerExt;

// State for window visibility
pub struct WindowVisibility {
    #[allow(dead_code)]
    pub is_hidden: Mutex<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Direction {
    Up,
    Down,
    Left,
    Right,
}

impl Direction {
    const ALL: [Direction; 4] = [
        Direction::Up,
        Direction::Down,
        Direction::Left,
        Direction::Right,
    ];
}

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Direction::Up => "up",
            Direction::Down => "down",
            Direction::Left => "left",
            Direction::Right => "right",
        })
    }
}

#[derive(Clone, Copy, Debug)]
enum Action {
    ToggleDashboard,
    ToggleWindow,
    FocusInput,
    AudioRecording,
    Screenshot,
    SystemAudio,
    Move(Direction),
}

/// Ids mirror `src/config/shortcuts.ts`; `move_window` is expanded in [`bindings`].
impl FromStr for Action {
    type Err = String;
    fn from_str(id: &str) -> Result<Self, String> {
        Ok(match id {
            "toggle_dashboard" => Action::ToggleDashboard,
            "toggle_window" => Action::ToggleWindow,
            "focus_input" => Action::FocusInput,
            "audio_recording" => Action::AudioRecording,
            "screenshot" => Action::Screenshot,
            "system_audio" => Action::SystemAudio,
            _ => return Err(format!("unknown shortcut action '{id}'")),
        })
    }
}

#[derive(Default)]
pub struct MoveWindowState {
    tasks: Mutex<HashMap<Direction, JoinHandle<()>>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShortcutBinding {
    pub action: String,
    pub key: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShortcutsConfig {
    pub bindings: HashMap<String, ShortcutBinding>,
}

fn bindings(config: &ShortcutsConfig) -> Result<Vec<(Shortcut, Action)>, String> {
    let parse = |key: &str, id: &str| {
        key.parse::<Shortcut>()
            .map_err(|e| format!("invalid shortcut '{key}' for '{id}': {e}"))
    };
    let mut out = Vec::new();
    for (id, binding) in &config.bindings {
        let action = match id.as_str() {
            "move_window" => None,
            id => Some(id.parse::<Action>()?),
        };
        if !binding.enabled || binding.key.is_empty() {
            continue;
        }
        match action {
            Some(action) => out.push((parse(&binding.key, id)?, action)),
            None => {
                for dir in Direction::ALL {
                    let key = format!("{}+{dir}", binding.key.trim());
                    out.push((parse(&key, id)?, Action::Move(dir)));
                }
            }
        }
    }
    Ok(out)
}

/// Tauri command to update shortcuts dynamically
#[tauri::command]
pub fn update_shortcuts<R: Runtime>(
    app: AppHandle<R>,
    config: ShortcutsConfig,
) -> Result<(), String> {
    let bindings =
        bindings(&config).inspect_err(|e| tracing::error!("rejected shortcuts config: {e}"))?;

    stop_all_move_windows(&app);
    let gs = app.global_shortcut();
    gs.unregister_all().map_err(|e| e.to_string())?;

    let mut failures = Vec::new();
    for (shortcut, action) in bindings {
        if let Err(e) = gs.on_shortcut(shortcut, move |app, _, ev| on_event(app, action, ev.state))
        {
            failures.push(format!("{action:?} ({shortcut}) - {e}"));
        }
    }
    if !failures.is_empty() {
        let msg = format!(
            "Some shortcuts could not be registered: {}",
            failures.join("; ")
        );
        tracing::error!("{msg}");
        return Err(msg);
    }
    Ok(())
}

fn on_event<R: Runtime>(app: &AppHandle<R>, action: Action, state: ShortcutState) {
    let result = match (action, state) {
        (Action::Move(dir), ShortcutState::Pressed) => {
            start_move_window(app, dir);
            Ok(())
        }
        (Action::Move(dir), ShortcutState::Released) => {
            stop_move_window(app, dir);
            Ok(())
        }
        (action, ShortcutState::Pressed) => run(app, action),
        (_, ShortcutState::Released) => Ok(()),
    };
    if let Err(e) = result {
        tracing::error!("shortcut {action:?} failed: {e:#}"); // runs on the hotkey thread: nothing to return to, a panic kills all shortcuts
    }
}

fn run<R: Runtime>(app: &AppHandle<R>, action: Action) -> anyhow::Result<()> {
    if let Action::ToggleDashboard = action {
        return crate::window::toggle_dashboard(app).map_err(anyhow::Error::msg);
    }
    let window = app
        .get_webview_window("main")
        .ok_or(tauri::Error::WindowNotFound)?;
    match action {
        Action::FocusInput => {
            if !window.is_visible()? {
                window.show()?;
            }
            window.set_focus()?;
            window.emit("focus-text-input", ())?;
        }
        Action::AudioRecording | Action::SystemAudio => {
            if !window.is_visible()? {
                window.show()?;
                window.set_focus()?;
            }
            let event = match action {
                Action::AudioRecording => "start-audio-recording",
                _ => "toggle-system-audio",
            };
            window.emit(event, ())?;
        }
        Action::Screenshot => window.emit("trigger-screenshot", ())?,
        Action::ToggleWindow => {
            #[cfg(target_os = "windows")]
            {
                let state = app.state::<WindowVisibility>();
                let mut is_hidden = state.is_hidden.lock().unwrap();
                *is_hidden = !*is_hidden;

                window.emit("toggle-window-visibility", *is_hidden)?;

                if !*is_hidden {
                    window.show()?;
                    window.set_focus()?;
                    window.emit("focus-text-input", ())?;
                }
                return Ok(());
            }

            #[cfg(not(target_os = "windows"))]
            if window.is_visible()? {
                #[cfg(target_os = "macos")]
                {
                    let panel = app.get_webview_window("main").unwrap();
                    let _ = panel.hide();
                }
                window.hide()?;
            } else {
                window.show()?;
                window.set_focus()?;

                #[cfg(target_os = "macos")]
                {
                    let panel = app.get_webview_panel("main").unwrap();
                    panel.show();
                }
                window.emit("focus-text-input", ())?;
            }
        }
        Action::ToggleDashboard | Action::Move(_) => {
            unreachable!("dashboard handled above, Move routed in on_event")
        }
    }
    Ok(())
}

fn start_move_window<R: Runtime>(app: &AppHandle<R>, dir: Direction) {
    let state = app.state::<MoveWindowState>();
    let mut tasks = state.tasks.lock().unwrap();
    if tasks.contains_key(&dir) {
        return;
    }
    let app = app.clone();
    let task = tauri::async_runtime::spawn(async move {
        loop {
            if let Err(e) = move_once(&app, dir) {
                tracing::error!("move window {dir} failed: {e}");
                break;
            }
            sleep(Duration::from_millis(16)).await;
        }
    });
    tasks.insert(dir, task);
}

fn stop_move_window<R: Runtime>(app: &AppHandle<R>, dir: Direction) {
    if let Some(task) = app
        .state::<MoveWindowState>()
        .tasks
        .lock()
        .unwrap()
        .remove(&dir)
    {
        task.abort();
    }
}

fn stop_all_move_windows<R: Runtime>(app: &AppHandle<R>) {
    for (_, task) in app.state::<MoveWindowState>().tasks.lock().unwrap().drain() {
        task.abort();
    }
}

fn move_once<R: Runtime>(app: &AppHandle<R>, dir: Direction) -> tauri::Result<()> {
    let window = app
        .get_webview_window("main")
        .ok_or(tauri::Error::WindowNotFound)?;
    let pos = window.outer_position()?;
    let step = 12;
    let (x, y) = match dir {
        Direction::Up => (pos.x, pos.y - step),
        Direction::Down => (pos.x, pos.y + step),
        Direction::Left => (pos.x - step, pos.y),
        Direction::Right => (pos.x + step, pos.y),
    };
    window.set_position(tauri::Position::Physical(tauri::PhysicalPosition { x, y }))
}

/// Tauri command to validate shortcut key
#[tauri::command]
pub fn validate_shortcut_key(key: String) -> Result<bool, String> {
    match key.parse::<Shortcut>() {
        Ok(_) => Ok(true),
        Err(e) => {
            eprintln!("Invalid shortcut '{}': {}", key, e);
            Ok(false)
        }
    }
}

/// Tauri command to set app icon visibility in dock/taskbar
#[tauri::command]
pub fn set_app_icon_visibility<R: Runtime>(app: AppHandle<R>, visible: bool) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        // On macOS, use activation policy to control dock icon
        let policy = if visible {
            tauri::ActivationPolicy::Regular
        } else {
            tauri::ActivationPolicy::Accessory
        };

        app.set_activation_policy(policy).map_err(|e| {
            eprintln!("Failed to set activation policy: {}", e);
            format!("Failed to set activation policy: {}", e)
        })?;
    }

    #[cfg(target_os = "windows")]
    {
        // On Windows, control taskbar icon visibility
        if let Some(window) = app.get_webview_window("main") {
            window
                .set_skip_taskbar(!visible)
                .map_err(|e| format!("Failed to set taskbar visibility: {}", e))?;
        } else {
            eprintln!("Main window not found on Windows");
        }
    }

    #[cfg(target_os = "linux")]
    {
        // On Linux, control panel icon visibility
        if let Some(window) = app.get_webview_window("main") {
            window
                .set_skip_taskbar(!visible)
                .map_err(|e| format!("Failed to set panel visibility: {}", e))?;
        } else {
            eprintln!("Main window not found on Linux");
        }
    }

    Ok(())
}

/// Tauri command to set always on top state
#[tauri::command]
pub fn set_always_on_top<R: Runtime>(app: AppHandle<R>, enabled: bool) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("main") {
        window
            .set_always_on_top(enabled)
            .map_err(|e| format!("Failed to set always on top: {}", e))?;
    } else {
        return Err("Main window not found".to_string());
    }

    Ok(())
}

/// Tauri command to exit the application
#[tauri::command]
pub fn exit_app(app_handle: tauri::AppHandle) {
    app_handle.exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauri::test::{mock_builder, mock_context, noop_assets};

    fn config(entries: &[(&str, &str, bool)]) -> ShortcutsConfig {
        ShortcutsConfig {
            bindings: entries
                .iter()
                .map(|&(action, key, enabled)| {
                    let binding = ShortcutBinding {
                        action: action.into(),
                        key: key.into(),
                        enabled,
                    };
                    (action.to_string(), binding)
                })
                .collect(),
        }
    }

    #[test]
    fn invalid_config_is_rejected_naming_the_offender() {
        let cases = [
            (
                "custom_x",
                config(&[
                    ("screenshot", "ctrl+shift+s", true),
                    ("custom_x", "ctrl+k", true),
                ]),
            ),
            ("custom_x", config(&[("custom_x", "ctrl+k", false)])),
            ("screenshot", config(&[("screenshot", "ctrl+nope", true)])),
            ("move_window", config(&[("move_window", "bogus", true)])),
        ];
        for (offender, config) in cases {
            let app = mock_builder()
                .manage(MoveWindowState::default())
                .build(mock_context(noop_assets()))
                .unwrap();
            let err = update_shortcuts(app.handle().clone(), config).unwrap_err();
            assert!(err.contains(offender), "{offender}: {err}");
        }
    }
}
