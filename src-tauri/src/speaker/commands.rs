// Pluely AI Speech Detection, and capture system audio (speaker output) as a stream of f32 samples.
use crate::db::schema::AttachedFile;
use crate::llm::commands::{complete, Chat, ProviderInput};
use crate::llm::provider::HistoryMessage;
use crate::llm::{stt, LlmError, LlmState};
use crate::speaker::turn::{Input, Output, Speaker, TurnEvent, Turns, SKIP_INSTRUCTION, SKIP_WORDS};
use crate::speaker::vad::{
    apply_noise_gate, calculate_audio_metrics, Segmenter, VadConfig, VadEvent,
};
use crate::speaker::{AudioDevice, SpeakerInput};
use anyhow::Result;
use futures_util::future::{BoxFuture, OptionFuture};
use futures_util::stream::{self, FuturesUnordered};
use futures_util::{FutureExt, Stream, StreamExt};
use hound::{WavSpec, WavWriter};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::Cursor;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::ipc::Channel;
use tauri::{AppHandle, Emitter, Listener, Manager};
use tauri_plugin_shell::ShellExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{error, warn};

const STT_TIMEOUT: Duration = Duration::from_secs(30);

/// Live per-chunk metrics emitted to the UI so users can see why VAD does or
/// does not trigger on their setup.
#[derive(Debug, Clone, Serialize)]
struct VadMetrics {
    rms: f32,
    peak: f32,
    sensitivity_rms: f32,
    peak_threshold: f32,
    noise_gate_threshold: f32,
    in_speech: bool,
}

/// Result of an explicit calibration run. Returned to the caller, who is
/// responsible for writing these into `VadConfig` if they want them persisted.
#[derive(Debug, Clone, Serialize)]
pub struct VadCalibration {
    pub noise_floor_rms: f32,
    pub sensitivity_rms: f32,
    pub peak_threshold: f32,
    pub noise_gate_threshold: f32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfig {
    stt: ProviderInput,
    ai: ProviderInput,
    system_prompt: String,
    attached_files: Vec<AttachedFile>,
}

/// Renderer-held session memory, seeded into each capture.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    config: SessionConfig,
    history: Vec<HistoryMessage>,
    carry: String,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Control {
    Config { config: Box<SessionConfig> },
    Prompt { text: String },
}

pub(crate) struct Capture {
    task: JoinHandle<()>,
    control: mpsc::UnboundedSender<Control>,
}

#[tauri::command]
pub async fn start_system_audio_capture(
    app: AppHandle,
    vad_config: Option<VadConfig>,
    device_id: Option<String>,
    session: Session,
    events: Channel<TurnEvent>,
) -> Result<(), String> {
    let state = app.state::<crate::AudioState>();
    let app_clone = app.clone();
    state
        .start(|control| {
            if let Some(config) = vad_config {
                config.validate()?;
                let mut vad_cfg = state
                    .vad_config
                    .lock()
                    .map_err(|e| format!("Failed to acquire VAD config lock: {}", e))?;
                *vad_cfg = config;
            }

            let input = SpeakerInput::new_with_device(device_id).map_err(|e| {
                error!("Failed to create speaker input: {}", e);
                format!("Failed to access system audio: {}", e)
            })?;

            let stream = input.stream();
            let sr = stream.sample_rate();

            if !(8000..=96000).contains(&sr) {
                error!("Invalid sample rate: {}", sr);
                return Err(format!(
                    "Invalid sample rate: {}. Expected 8000-96000 Hz",
                    sr
                ));
            }

            let vad_config = state
                .vad_config
                .lock()
                .map_err(|e| format!("Failed to read VAD config: {}", e))?
                .clone();

            let segmenter = vad_config
                .enabled
                .then(|| Segmenter::new(&vad_config, sr))
                .transpose()?;

            Ok(async move {
                let app = app_clone;
                let mut stream = stream;
                let recording = match segmenter {
                    Some(mut vad) => {
                        let mut seen = 0u64;
                        let audio = (&mut stream).ready_chunks(4096).map(|c| {
                            seen += c.len() as u64;
                            (vad.push(&c).into_iter().map(|e| (Speaker::Interviewer, e)).collect(), seen * 1000 / sr as u64)
                        });
                        drive(&app, sr, &vad_config, audio, session, control, events).await;
                        report_stream_error(&app, stream.error());
                        return;
                    }
                    None => run_continuous_capture(&app, &mut stream, sr, &vad_config).await,
                };
                if report_stream_error(&app, stream.error()) {
                    return;
                }
                drop(stream); // release the device while answering
                if let Some(samples) = recording {
                    let end_ms = samples.len() as u64 * 1000 / sr as u64;
                    let segment = VadEvent::Segment { samples, start_ms: 0, end_ms };
                    let audio = stream::iter([(vec![(Speaker::Interviewer, segment)], end_ms)]);
                    drive(&app, sr, &vad_config, audio, session, control, events).await;
                }
            })
        })
        .await
}

/// True if the stream ended on an error.
fn report_stream_error(app: &AppHandle, e: Option<anyhow::Error>) -> bool {
    let Some(e) = e else { return false };
    error!("System audio capture ended: {e:#}");
    if let Err(emit_err) = app.emit("capture-error", format!("{e:#}")) {
        error!("Failed to emit capture-error: {}", emit_err);
    }
    true
}

#[tauri::command]
pub async fn system_audio_control(app: AppHandle, control: Control) -> Result<(), String> {
    let state = app.state::<crate::AudioState>();
    let slot = state.capture.lock().await;
    slot.as_ref()
        .ok_or(())
        .and_then(|c| c.control.send(control).map_err(drop))
        .map_err(|()| "System audio capture is not running".to_string())
}

impl crate::AudioState {
    /// Locks the capture slot; Err if a capture is live. A finished capture is reaped.
    async fn lock_idle(
        &self,
    ) -> Result<tokio::sync::MutexGuard<'_, Option<Capture>>, String> {
        let mut slot = self.capture.lock().await;
        if let Some(done) = slot.take_if(|c| c.task.is_finished()) {
            reap(done.task).await;
        }
        if slot.is_some() {
            warn!("Capture already running");
            return Err("Capture already running".to_string());
        }
        Ok(slot)
    }

    async fn start<F>(
        &self,
        open: impl FnOnce(mpsc::UnboundedReceiver<Control>) -> Result<F, String>,
    ) -> Result<(), String>
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let mut slot = self.lock_idle().await?;
        let (control, rx) = mpsc::unbounded_channel();
        *slot = Some(Capture {
            task: tokio::spawn(open(rx)?),
            control,
        });
        Ok(())
    }

    async fn stop(&self) {
        let mut slot = self.capture.lock().await;
        if let Some(c) = slot.take() {
            c.task.abort();
            reap(c.task).await;
        }
    }
}

async fn reap(task: JoinHandle<()>) {
    if let Err(e) = task.await {
        if e.is_panic() {
            std::panic::resume_unwind(e.into_panic());
        } // Cancelled is our own abort
    }
}

/// Segment -> STT -> turn -> LLM loop. Owns every STT and LLM future, so aborting the task cancels them.
async fn drive(
    app: &AppHandle,
    sr: u32,
    vad_cfg: &VadConfig,
    mut audio: impl Stream<Item = (Vec<(Speaker, VadEvent)>, u64)> + Unpin,
    session: Session,
    mut control: mpsc::UnboundedReceiver<Control>,
    events: Channel<TurnEvent>,
) {
    let llm = app.state::<LlmState>();
    let llm: &LlmState = &llm;
    let Session {
        mut config,
        history,
        carry,
    } = session;
    let mut turns = Turns::new(history, carry, vad_cfg);
    let mut transcripts = FuturesUnordered::<BoxFuture<'_, (u64, Result<String, String>)>>::new();
    let mut answer: Option<BoxFuture<'_, Result<String, LlmError>>> = None;
    let mut audio_done = false;

    loop {
        if audio_done && transcripts.is_empty() && answer.is_none() {
            return;
        }
        let inputs = tokio::select! {
            chunk = audio.next(), if !audio_done => match chunk {
                None => {
                    audio_done = true;
                    vec![Input::Flush]
                }
                Some((vad_events, now_ms)) => {
                    let mut inputs = Vec::new();
                    for (speaker, ev) in vad_events {
                        match ev {
                            VadEvent::SpeechStart { start_ms } => {
                                inputs.push(Input::SpeechStart { speaker, at_ms: start_ms })
                            }
                            VadEvent::Segment { samples, start_ms, end_ms } => {
                                if speaker == Speaker::Interviewer {
                                    let wav = samples_to_wav(sr, &normalize_audio_level(&samples, 0.1));
                                    let provider = config.stt.clone();
                                    transcripts.push(
                                        async move {
                                            let text = match tokio::time::timeout(
                                                STT_TIMEOUT,
                                                stt::transcribe(app, llm, &provider, &wav, "audio/wav"),
                                            )
                                            .await
                                            {
                                                Ok(r) => r.map_err(|e| format!("Transcription failed: {e}")),
                                                Err(_) => Err("Speech transcription timed out (30s)".to_string()),
                                            };
                                            (start_ms, text)
                                        }
                                        .boxed(),
                                    );
                                }
                                inputs.push(Input::Segment { speaker, start_ms, end_ms });
                            }
                            VadEvent::Discarded { .. } => {
                                if speaker == Speaker::Interviewer {
                                    if let Err(e) = app.emit(
                                        "speech-discarded",
                                        "Audio too short (likely background noise)",
                                    ) {
                                        error!("Failed to emit speech-discarded: {}", e);
                                    }
                                }
                                inputs.push(Input::Discarded { speaker });
                            }
                            VadEvent::Metrics { .. } if speaker == Speaker::User => {} // meters and calibration are about system audio
                            VadEvent::Metrics { rms, peak, in_speech } => {
                                if let Err(e) = app.emit(
                                    "vad-metrics",
                                    VadMetrics {
                                        rms,
                                        peak,
                                        sensitivity_rms: vad_cfg.sensitivity_rms,
                                        peak_threshold: vad_cfg.peak_threshold,
                                        noise_gate_threshold: vad_cfg.noise_gate_threshold,
                                        in_speech,
                                    },
                                ) {
                                    error!("Failed to emit vad-metrics: {}", e);
                                }
                            }
                        }
                    }
                    inputs.push(Input::Tick { now_ms });
                    inputs
                }
            },
            Some((start_ms, text)) = transcripts.next() => vec![Input::Transcript { start_ms, text }],
            Some(reply) = OptionFuture::from(answer.as_mut()) => {
                answer = None;
                vec![Input::Reply(reply.map_err(|e| e.to_string()))]
            }
            Some(c) = control.recv() => match c {
                Control::Config { config: c } => {
                    config = *c;
                    Vec::new()
                }
                Control::Prompt { text } => vec![Input::Prompt(text)],
            },
        };

        for input in inputs {
            for out in turns.push(input) {
                match out {
                    Output::Ask { message, history } => {
                        assert!(answer.is_none(), "Turns asks one at a time");
                        let chat = Chat {
                            provider: config.ai.clone(),
                            message,
                            system_prompt: Some(format!("{}\n\n{SKIP_INSTRUCTION}", config.system_prompt)),
                            history,
                            attached_files: config.attached_files.clone(),
                        };
                        let events = events.clone();
                        answer = Some(
                            async move {
                                let (mut full, mut sent) = (String::new(), 0);
                                let mut on_delta = |d: String| {
                                    full.push_str(&d);
                                    if SKIP_WORDS.iter().any(|w| w.starts_with(full.trim())) {
                                        return Ok(()); // may still become a skip word, which never reaches the screen
                                    }
                                    let delta = full[sent..].to_string();
                                    sent = full.len();
                                    events
                                        .send(TurnEvent::Delta { delta })
                                        .map_err(|e| LlmError::Channel(e.to_string()))
                                };
                                complete(app, llm, chat, &mut on_delta).await
                            }
                            .boxed(),
                        );
                    }
                    Output::Event(e) => {
                        if let Err(err) = events.send(e) {
                            error!("Turn event channel closed, ending capture: {err}"); // its renderer is gone
                            return;
                        }
                    }
                }
            }
        }
    }
}

// Continuous capture (VAD disabled)
/// Noise-gated recording, or None when nothing usable was captured (already reported to the UI).
async fn run_continuous_capture(
    app: &AppHandle,
    stream: impl StreamExt<Item = f32> + Unpin,
    sr: u32,
    config: &VadConfig,
) -> Option<Vec<f32>> {
    let mut stream = stream;
    let max_samples = (sr as u64 * config.max_recording_duration_secs) as usize;

    // Pre-allocate buffer to prevent reallocations
    let mut audio_buffer = Vec::with_capacity(max_samples);
    let start_time = Instant::now();
    let max_duration = Duration::from_secs(config.max_recording_duration_secs);

    // Atomic flag for manual stop
    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_flag_for_listener = stop_flag.clone();

    // Listen for manual stop event
    let stop_listener = app.listen("manual-stop-continuous", move |_| {
        stop_flag_for_listener.store(true, Ordering::Release);
    });

    // Emit recording started
    if let Err(e) = app.emit(
        "continuous-recording-start",
        config.max_recording_duration_secs,
    ) {
        error!("Failed to emit continuous-recording-start: {}", e);
    }

    // Accumulate audio - check stop flag on EVERY sample for immediate response
    loop {
        // Check stop flag FIRST on every iteration for immediate stopping
        if stop_flag.load(Ordering::Acquire) {
            break;
        }

        tokio::select! {
            sample_opt = stream.next() => {
                match sample_opt {
                    Some(sample) => {
                        if stop_flag.load(Ordering::Acquire) {
                            break;
                        }

                        audio_buffer.push(sample);

                        let elapsed = start_time.elapsed();

                        // Emit progress every second
                        if audio_buffer.len() % (sr as usize) == 0 {
                            if let Err(e) = app.emit("recording-progress", elapsed.as_secs()) {
                                error!("Failed to emit recording-progress: {}", e);
                            }
                        }

                        // Check size limit (safety)
                        if audio_buffer.len() >= max_samples {
                            break;
                        }

                        // Check time limit
                        if elapsed >= max_duration {
                            break;
                        }
                    },
                    None => {
                        warn!("Audio stream ended unexpectedly");
                        break;
                    }
                }
            }
            _ = tokio::time::sleep(tokio::time::Duration::from_millis(10)) => {
            }
        }
    }

    // Clean up event listener (CRITICAL)
    app.unlisten(stop_listener);

    if let Err(e) = app.emit("continuous-recording-stopped", ()) {
        error!("Failed to emit continuous-recording-stopped: {}", e);
    }

    if audio_buffer.is_empty() {
        warn!("No audio captured in continuous mode");
        if let Err(e) = app.emit("audio-encoding-error", "No audio recorded") {
            error!("Failed to emit audio-encoding-error: {}", e);
        }
        return None;
    }
    let (raw_rms, raw_peak) = calculate_audio_metrics(&audio_buffer);
    tracing::info!(
        "continuous capture: sr={} samples={} dur={:.2}s rms={:.5} peak={:.5}",
        sr,
        audio_buffer.len(),
        audio_buffer.len() as f32 / sr as f32,
        raw_rms,
        raw_peak
    );
    if raw_peak < config.noise_gate_threshold {
        // Nothing rises above the noise floor (e.g. the monitored sink received no signal);
        // STT would only return an empty transcript.
        warn!(
            "Continuous recording contained no audible signal (peak {:.6}, gate {:.6})",
            raw_peak, config.noise_gate_threshold
        );
        if let Err(e) = app.emit(
            "speech-discarded",
            "recording was silent - check that audio is routed to the captured device",
        ) {
            error!("Failed to emit speech-discarded (silent): {}", e);
        }
        return None;
    }
    Some(apply_noise_gate(&audio_buffer, config.noise_gate_threshold))
}

fn normalize_audio_level(samples: &[f32], target_rms: f32) -> Vec<f32> {
    if samples.is_empty() {
        return Vec::new();
    }

    let sum_squares: f32 = samples.iter().map(|&s| s * s).sum();
    let current_rms = (sum_squares / samples.len() as f32).sqrt();

    if current_rms < 0.001 {
        return samples.to_vec();
    }

    let gain = (target_rms / current_rms).min(10.0);

    samples
        .iter()
        .map(|&s| {
            let amplified = s * gain;
            if amplified.abs() > 1.0 {
                amplified.signum() * (1.0 - (-amplified.abs()).exp())
            } else {
                amplified
            }
        })
        .collect()
}

fn samples_to_wav(sample_rate: u32, mono_f32: &[f32]) -> Vec<u8> {
    assert!(!mono_f32.is_empty(), "segments are never empty");
    let mut cursor = Cursor::new(Vec::new());
    let spec = WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = WavWriter::new(&mut cursor, spec).expect("writing to a Vec cannot fail");
    for &s in mono_f32 {
        let sample_i16 = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        writer.write_sample(sample_i16).expect("writing to a Vec cannot fail");
    }
    writer.finalize().expect("writing to a Vec cannot fail");
    cursor.into_inner()
}

#[tauri::command]
pub async fn stop_system_audio_capture(app: AppHandle) -> Result<(), String> {
    app.state::<crate::AudioState>().stop().await;
    Ok(())
}

/// Manual stop for continuous recording
#[tauri::command]
pub async fn manual_stop_continuous(app: AppHandle) -> Result<(), String> {
    if let Err(e) = app.emit("manual-stop-continuous", ()) {
        error!("Failed to emit manual-stop-continuous: {}", e);
    }

    tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;

    Ok(())
}

#[tauri::command]
pub fn check_system_audio_access(_app: AppHandle) -> Result<bool, String> {
    match SpeakerInput::new() {
        Ok(_) => Ok(true),
        Err(e) => {
            error!("System audio access check failed: {}", e);
            Ok(false) // IPC contract is a bool; the cause is only logged
        }
    }
}

#[tauri::command]
pub async fn request_system_audio_access(app: AppHandle) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        app.shell()
            .command("open")
            .args(["x-apple.systempreferences:com.apple.preference.security?Privacy_AudioCapture"])
            .spawn()
            .map_err(|e| {
                error!("Failed to open system preferences: {}", e);
                e.to_string()
            })?;
    }
    #[cfg(target_os = "windows")]
    {
        app.shell()
            .command("ms-settings:sound")
            .spawn()
            .map_err(|e| {
                error!("Failed to open sound settings: {}", e);
                e.to_string()
            })?;
    }
    #[cfg(target_os = "linux")]
    {
        let commands: [(&str, &[&str]); 2] =
            [("pavucontrol", &[]), ("gnome-control-center", &["sound"])];
        // A failed spawn only means that app isn't installed; try the next one.
        if !commands
            .iter()
            .any(|(cmd, args)| app.shell().command(cmd).args(*args).spawn().is_ok())
        {
            return Err(
                "No audio settings app found (tried pavucontrol, gnome-control-center)".into(),
            );
        }
    }

    Ok(())
}

// VAD Configuration Management
#[tauri::command]
pub async fn get_vad_config(app: AppHandle) -> Result<VadConfig, String> {
    let state = app.state::<crate::AudioState>();
    let config = state
        .vad_config
        .lock()
        .map_err(|e| format!("Failed to get VAD config: {}", e))?
        .clone();
    Ok(config)
}

/// Sample ambient audio for `duration_secs` and derive proposed VAD thresholds
/// from the measured noise floor. The caller is responsible for persisting the
/// result into VadConfig if they want it kept.
///
/// Refuses to run while a capture session is active (would fight over the
/// audio device).
#[tauri::command]
pub async fn calibrate_vad_thresholds(
    app: AppHandle,
    duration_secs: u64,
    device_id: Option<String>,
) -> Result<VadCalibration, String> {
    if !(1..=10).contains(&duration_secs) {
        return Err("duration_secs must be between 1 and 10".to_string());
    }

    let state = app.state::<crate::AudioState>();
    let idle = state.lock_idle().await?;

    let input = SpeakerInput::new_with_device(device_id).map_err(|e| {
        error!("Calibration: failed to open audio source: {}", e);
        format!("Failed to access system audio: {}", e)
    })?;

    let mut stream = input.stream();
    let sr = stream.sample_rate();
    if !(8000..=96000).contains(&sr) {
        return Err(format!("Invalid sample rate: {}", sr));
    }

    let hop_size: usize = 1024;
    let target_chunks = ((sr as usize * duration_secs as usize) / hop_size).max(8);
    let mut buffer: VecDeque<f32> = VecDeque::new();
    let mut floor_samples: Vec<f32> = Vec::with_capacity(target_chunks);

    // Hard timeout so a misbehaving source can't hang the UI.
    let deadline = Instant::now() + Duration::from_secs(duration_secs + 2);

    while floor_samples.len() < target_chunks {
        if Instant::now() >= deadline {
            break;
        }
        match stream.next().await {
            Some(sample) => {
                buffer.push_back(sample);
                while buffer.len() >= hop_size && floor_samples.len() < target_chunks {
                    let mut mono = Vec::with_capacity(hop_size);
                    for _ in 0..hop_size {
                        if let Some(v) = buffer.pop_front() {
                            mono.push(v);
                        }
                    }
                    let (rms, _peak) = calculate_audio_metrics(&mono);
                    floor_samples.push(rms);
                }
            }
            None => break,
        }
    }
    drop(stream);
    drop(idle); // release device before admitting a start

    if floor_samples.is_empty() {
        return Err(
            "No audio captured during calibration — is the source actually producing sound?"
                .to_string(),
        );
    }

    if floor_samples.iter().any(|r| r.is_nan()) {
        return Err("Audio source produced NaN samples".to_string());
    }

    // Use the 90th-percentile RMS as the noise-floor "ceiling" so a single
    // transient (keystroke, mouse click) doesn't blow out the result.
    floor_samples.sort_by(f32::total_cmp);
    let idx = ((floor_samples.len() as f32) * 0.9) as usize;
    let noise_floor = floor_samples[idx.min(floor_samples.len() - 1)];

    // Refuse to calibrate if user is clearly talking — would set thresholds
    // above their normal speech volume.
    if noise_floor > 0.05 {
        return Err(format!(
            "Detected loud audio ({:.3}) during calibration. Stay quiet and try again.",
            noise_floor
        ));
    }

    // Clamp to sane minimums so a perfectly digital-silent source doesn't
    // result in zero thresholds that would treat every sample as speech.
    let noise_gate_threshold = (noise_floor * 1.5).max(0.0005);
    let sensitivity_rms = (noise_floor * 4.0).max(0.003);
    let peak_threshold = (noise_floor * 10.0).max(0.01);

    Ok(VadCalibration {
        noise_floor_rms: noise_floor,
        sensitivity_rms,
        peak_threshold,
        noise_gate_threshold,
    })
}

#[tauri::command]
pub async fn update_vad_config(app: AppHandle, config: VadConfig) -> Result<(), String> {
    config.validate()?;

    let state = app.state::<crate::AudioState>();
    *state
        .vad_config
        .lock()
        .map_err(|e| format!("Failed to update VAD config: {}", e))? = config;

    Ok(())
}

#[tauri::command]
pub fn get_input_devices() -> Result<Vec<AudioDevice>, String> {
    crate::speaker::list_input_devices().map_err(|e| {
        error!("Failed to get input devices: {}", e);
        format!("Failed to get input devices: {}", e)
    })
}

#[tauri::command]
pub fn get_output_devices() -> Result<Vec<AudioDevice>, String> {
    crate::speaker::list_output_devices().map_err(|e| {
        error!("Failed to get output devices: {}", e);
        format!("Failed to get output devices: {}", e)
    })
}

#[cfg(test)]
mod tests {
    use crate::AudioState;
    use futures_util::{Stream, StreamExt};
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::task::{Context, Poll};

    #[derive(Default)]
    struct Counters {
        now: AtomicUsize,
        max: AtomicUsize,
    }

    struct FakeStream {
        live: Arc<Counters>,
        remaining: Option<usize>,
    }

    impl FakeStream {
        fn new(live: Arc<Counters>, remaining: Option<usize>) -> Self {
            let now = live.now.fetch_add(1, Ordering::SeqCst) + 1;
            live.max.fetch_max(now, Ordering::SeqCst);
            Self { live, remaining }
        }
    }

    impl Drop for FakeStream {
        fn drop(&mut self) {
            self.live.now.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl Stream for FakeStream {
        type Item = f32;
        fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<f32>> {
            match &mut self.remaining {
                None => Poll::Pending,
                Some(0) => Poll::Ready(None),
                Some(n) => {
                    *n -= 1;
                    Poll::Ready(Some(0.0))
                }
            }
        }
    }

    async fn start(
        state: &AudioState,
        live: &Arc<Counters>,
        remaining: Option<usize>,
    ) -> Result<(), String> {
        let live = live.clone();
        state
            .start(|_| {
                let mut s = FakeStream::new(live, remaining);
                Ok(async move { while s.next().await.is_some() {} })
            })
            .await
    }

    #[derive(Debug)]
    enum Op {
        Start,
        StartFinite(usize),
        WaitFinished,
        Stop,
    }

    #[tokio::test]
    async fn lifecycle() {
        use Op::*;
        // (ops, per-op success, live streams at end, max concurrent streams)
        let cases: &[(&[Op], &[bool], usize, usize)] = &[
            (&[Start, Stop, Start], &[true, true, true], 1, 1),
            (&[Start, Start], &[true, false], 1, 1),
            (&[Stop, Stop], &[true, true], 0, 0),
            (
                &[StartFinite(10), WaitFinished, Start],
                &[true, true, true],
                1,
                1,
            ),
            (&[Start, Stop], &[true, true], 0, 1),
        ];
        for (ops, expected, now, max) in cases {
            let state = AudioState::default();
            let live = Arc::new(Counters::default());
            let mut got = Vec::new();
            for op in *ops {
                got.push(match op {
                    Start => start(&state, &live, None).await.is_ok(),
                    StartFinite(n) => start(&state, &live, Some(*n)).await.is_ok(),
                    WaitFinished => {
                        while live.now.load(Ordering::SeqCst) != 0 {
                            tokio::task::yield_now().await;
                        }
                        true
                    }
                    Stop => {
                        state.stop().await;
                        true
                    }
                });
            }
            assert_eq!(&got[..], *expected, "{ops:?}");
            assert_eq!(
                live.now.load(Ordering::SeqCst),
                *now,
                "{ops:?} live streams"
            );
            assert_eq!(live.max.load(Ordering::SeqCst), *max, "{ops:?} max streams");
            state.stop().await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_stop_start_never_orphans() {
        let state = Arc::new(AudioState::default());
        let live = Arc::new(Counters::default());
        for _ in 0..20 {
            let (s1, s2, s3) = (state.clone(), state.clone(), state.clone());
            let l = live.clone();
            let stop1 = tokio::spawn(async move { s1.stop().await });
            let stop2 = tokio::spawn(async move { s2.stop().await });
            let start = tokio::spawn(async move {
                if let Err(e) = start(&s3, &l, None).await {
                    assert_eq!(e, "Capture already running"); // lost the race to a live capture, which is allowed
                }
            });
            stop1.await.unwrap();
            stop2.await.unwrap();
            start.await.unwrap();
        }
        state.stop().await;
        assert_eq!(
            live.now.load(Ordering::SeqCst),
            0,
            "orphaned capture still live"
        );
        assert!(
            live.max.load(Ordering::SeqCst) <= 1,
            "two captures ran at once"
        );
    }
}
