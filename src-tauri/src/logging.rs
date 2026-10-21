//! Log sinks, panic hook, session banner and the renderer's log command. See ARCHITECTURE.md#logging.

use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _, EnvFilter, Layer as _};

/// RUST_LOG overrides every sink's default.
fn filter(default: &str) -> EnvFilter {
    let directives = match std::env::var("RUST_LOG") {
        Err(std::env::VarError::NotPresent) => default.to_owned(),
        r => r.expect("RUST_LOG must be unicode"),
    };
    EnvFilter::try_new(&directives).unwrap_or_else(|e| panic!("invalid RUST_LOG `{directives}`: {e}"))
}

/// `forwarder`: a `pluely --action` process that hands off to the running instance and exits.
#[cfg_attr(not(target_os = "linux"), expect(unused_variables))]
pub(crate) fn init(forwarder: bool) {
    use std::io::IsTerminal as _;
    let tty = std::io::stderr().is_terminal();
    let registry = tracing_subscriber::registry();
    #[cfg(target_os = "linux")]
    let registry = registry.with(file_layer(forwarder, tty));
    // off a tty on linux, fd 2 already is the log file
    let stderr = (tty || cfg!(not(target_os = "linux")))
        .then(|| tracing_subscriber::fmt::layer().with_writer(std::io::stderr).with_filter(filter("info")));
    registry.with(stderr).init();

    std::panic::set_hook(Box::new(|info| {
        tracing::error!(
            target: "panic",
            thread = ?std::thread::current().name(),
            "{info}\n{}",
            std::backtrace::Backtrace::force_capture()
        );
    }));
}

/// Launched from a launcher/autostart, stderr is gone: persist it so a failed run can be studied afterwards.
#[cfg(target_os = "linux")]
fn file_layer<S>(forwarder: bool, tty: bool) -> impl tracing_subscriber::Layer<S>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    const ROTATE_BYTES: u64 = 16 << 20;
    let dir = std::path::PathBuf::from(std::env::var_os("HOME").expect("HOME is set on linux sessions"))
        .join(".local/state/pluely");
    std::fs::create_dir_all(&dir).expect("state dir must be writable");
    let path = dir.join("pluely.log");
    // a forwarder runs next to the live instance, whose log it must not move
    if !forwarder && std::fs::metadata(&path).is_ok_and(|m| m.len() > ROTATE_BYTES) {
        std::fs::rename(&path, dir.join("pluely.log.1")).expect("state dir must be writable");
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .expect("log file must be writable");
    if !tty {
        use std::os::fd::AsRawFd as _;
        // catches what never reaches tracing: GLib/WebKit/GStreamer (web processes inherit fd 2) and glibc abort messages
        // SAFETY: both descriptors are open; dup2 atomically replaces fd 2.
        let r = unsafe { libc::dup2(file.as_raw_fd(), libc::STDERR_FILENO) };
        assert_ne!(r, -1, "dup2 log file onto stderr: {}", std::io::Error::last_os_error());
    }
    tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(std::sync::Arc::new(file))
        .with_filter(filter("info,pluely_lib=debug,js=debug"))
}

/// Primary instance only: forwarders exit inside the single-instance plugin, before `setup`.
pub(crate) fn banner(app: &tauri::AppHandle) {
    const ENV: &[&str] = &[
        "WAYLAND_DISPLAY",
        "DISPLAY",
        "XDG_SESSION_TYPE",
        "XDG_CURRENT_DESKTOP",
        "GDK_BACKEND",
        "WEBKIT_DISABLE_DMABUF_RENDERER",
        "WEBKIT_DISABLE_COMPOSITING_MODE",
        "LIBGL_ALWAYS_SOFTWARE",
        "PULSE_SERVER",
        "RUST_LOG",
    ];
    let env: Vec<_> = ENV.iter().filter_map(|k| Some((*k, std::env::var_os(k)?))).collect();
    tracing::info!(
        version = %app.package_info().version,
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        kernel = ?std::fs::read_to_string("/proc/sys/kernel/osrelease").map(|s| s.trim().to_owned()),
        pid = std::process::id(),
        ?env,
        "session start"
    );
    let names = |d: Vec<crate::speaker::AudioDevice>| -> Vec<String> {
        d.into_iter().map(|d| if d.is_default { format!("*{}", d.name) } else { d.name }).collect()
    };
    match crate::speaker::get_input_devices() {
        Ok(d) => tracing::info!(devices = ?names(d), "audio inputs"),
        Err(e) => tracing::error!(error = %e, "audio inputs unavailable"),
    }
    match crate::speaker::get_output_devices() {
        Ok(d) => tracing::info!(devices = ?names(d), "audio outputs"),
        Err(e) => tracing::error!(error = %e, "audio outputs unavailable"),
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JsLevel {
    Error,
    Warn,
    Info,
    Debug,
}

/// The webview console is invisible outside devtools.
#[tauri::command]
pub fn js_log(window: tauri::WebviewWindow, level: JsLevel, msg: String) {
    let window = window.label();
    match level {
        JsLevel::Error => tracing::error!(target: "js", window, "{msg}"),
        JsLevel::Warn => tracing::warn!(target: "js", window, "{msg}"),
        JsLevel::Info => tracing::info!(target: "js", window, "{msg}"),
        JsLevel::Debug => tracing::debug!(target: "js", window, "{msg}"),
    }
}
