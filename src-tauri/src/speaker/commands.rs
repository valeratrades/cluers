// Pluely AI Speech Detection, and capture system audio (speaker output) as a stream of f32 samples.
use crate::speaker::{AudioDevice, SpeakerInput};
use anyhow::Result;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use futures_util::StreamExt;
use hound::{WavSpec, WavWriter};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::Cursor;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use tauri::{AppHandle, Emitter, Listener, Manager};
use tauri_plugin_shell::ShellExt;
use tracing::{error, warn};

// VAD Configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VadConfig {
    pub enabled: bool,
    pub hop_size: usize,
    pub sensitivity_rms: f32,
    pub peak_threshold: f32,
    pub silence_chunks: usize,
    pub min_speech_chunks: usize,
    pub pre_speech_chunks: usize,
    pub noise_gate_threshold: f32,
    pub max_recording_duration_secs: u64,
}

impl Default for VadConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            hop_size: 1024,
            sensitivity_rms: 0.012, // Much less sensitive - only real speech
            peak_threshold: 0.035,  // Higher threshold - filters clicks/noise
            silence_chunks: 45,     // ~1.0s of silence before stopping
            min_speech_chunks: 7,   // ~0.16s - captures short answers
            pre_speech_chunks: 12,  // ~0.27s - enough to catch word start
            noise_gate_threshold: 0.003, // Stronger noise filtering
            max_recording_duration_secs: 180, // 3 minutes default
        }
    }
}

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

#[tauri::command]
pub async fn start_system_audio_capture(
    app: AppHandle,
    vad_config: Option<VadConfig>,
    device_id: Option<String>,
) -> Result<(), String> {
    let state = app.state::<crate::AudioState>();
    let app_clone = app.clone();
    state
        .start(|| {
            if let Some(config) = vad_config {
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

            Ok(async move {
                let mut stream = stream;
                if vad_config.enabled {
                    run_vad_capture(app_clone.clone(), &mut stream, sr, vad_config).await;
                } else {
                    run_continuous_capture(app_clone.clone(), &mut stream, sr, vad_config).await;
                }
                if let Some(e) = stream.error() {
                    error!("System audio capture ended: {e:#}");
                    if let Err(emit_err) = app_clone.emit("capture-error", format!("{e:#}")) {
                        error!("Failed to emit capture-error: {}", emit_err);
                    }
                }
            })
        })
        .await
}

impl crate::AudioState {
    /// Locks the capture slot; Err if a capture is live. A finished capture is reaped.
    async fn lock_idle(
        &self,
    ) -> Result<tokio::sync::MutexGuard<'_, Option<JoinHandle<()>>>, String> {
        let mut slot = self.capture.lock().await;
        if let Some(done) = slot.take_if(|t| t.is_finished()) {
            reap(done).await;
        }
        if slot.is_some() {
            warn!("Capture already running");
            return Err("Capture already running".to_string());
        }
        Ok(slot)
    }

    async fn start<F>(&self, open: impl FnOnce() -> Result<F, String>) -> Result<(), String>
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let mut slot = self.lock_idle().await?;
        *slot = Some(tokio::spawn(open()?));
        Ok(())
    }

    async fn stop(&self) {
        let mut slot = self.capture.lock().await;
        if let Some(task) = slot.take() {
            task.abort();
            reap(task).await;
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

// VAD-enabled capture - OPTIMIZED for real-time speech detection
async fn run_vad_capture(
    app: AppHandle,
    stream: impl StreamExt<Item = f32> + Unpin,
    sr: u32,
    config: VadConfig,
) {
    let mut stream = stream;
    let mut buffer: VecDeque<f32> = VecDeque::new();
    let mut pre_speech: VecDeque<f32> =
        VecDeque::with_capacity(config.pre_speech_chunks * config.hop_size);
    let mut speech_buffer = Vec::new();
    let mut in_speech = false;
    let mut silence_chunks = 0;
    let mut speech_chunks = 0;
    let max_samples = sr as usize * 30; // 30s safety cap per utterance

    // Throttle metrics emission to ~10 Hz regardless of sample rate / hop size.
    let metrics_interval = Duration::from_millis(100);
    let mut last_metrics_emit = Instant::now()
        .checked_sub(metrics_interval)
        .unwrap_or_else(Instant::now);

    while let Some(sample) = stream.next().await {
        buffer.push_back(sample);

        // Process in fixed chunks for VAD analysis
        while buffer.len() >= config.hop_size {
            let mut mono = Vec::with_capacity(config.hop_size);
            for _ in 0..config.hop_size {
                if let Some(v) = buffer.pop_front() {
                    mono.push(v);
                }
            }

            // Apply noise gate BEFORE VAD (critical for accuracy)
            let mono = apply_noise_gate(&mono, config.noise_gate_threshold);

            let (rms, peak) = calculate_audio_metrics(&mono);
            let is_speech =
                rms > config.sensitivity_rms || peak > config.peak_threshold;

            // Throttled metrics emission so the UI can render a live meter.
            if last_metrics_emit.elapsed() >= metrics_interval {
                last_metrics_emit = Instant::now();
                if let Err(e) = app.emit(
                    "vad-metrics",
                    VadMetrics {
                        rms,
                        peak,
                        sensitivity_rms: config.sensitivity_rms,
                        peak_threshold: config.peak_threshold,
                        noise_gate_threshold: config.noise_gate_threshold,
                        in_speech,
                    },
                ) {
                    error!("Failed to emit vad-metrics: {}", e);
                }
            }

            if is_speech {
                if !in_speech {
                    // Speech START detected
                    in_speech = true;
                    speech_chunks = 0;

                    // Include pre-speech buffer for natural sound
                    speech_buffer.extend(pre_speech.drain(..));

                    if let Err(e) = app.emit("speech-start", ()) {
                        error!("Failed to emit speech-start: {}", e);
                    }
                }

                speech_chunks += 1;
                speech_buffer.extend_from_slice(&mono);
                silence_chunks = 0; // Reset silence counter on any speech

                // Safety cap: force emit if exceeds 30s
                if speech_buffer.len() > max_samples {
                    let normalized_buffer = normalize_audio_level(&speech_buffer, 0.1);
                    match samples_to_wav_b64(sr, &normalized_buffer) {
                        Ok(b64) => {
                            if let Err(e) = app.emit("speech-detected", b64) {
                                error!("Failed to emit speech-detected (max-samples cap): {}", e);
                            }
                        }
                        Err(e) => error!("Failed to encode speech (max-samples cap): {}", e),
                    }
                    speech_buffer.clear();
                    in_speech = false;
                    speech_chunks = 0;
                }
            } else {
                // Silence detected
                if in_speech {
                    silence_chunks += 1;

                    // Continue collecting during silence (important for natural speech)
                    speech_buffer.extend_from_slice(&mono);

                    // Check if silence duration exceeds threshold
                    if silence_chunks >= config.silence_chunks {
                        // Verify minimum speech duration
                        if speech_chunks >= config.min_speech_chunks && !speech_buffer.is_empty() {
                            // Trim trailing silence (keep ~0.15s for natural ending)
                            let silence_duration_samples = silence_chunks * config.hop_size;
                            let keep_silence_samples = (sr as usize) * 15 / 100; // 0.15s
                            let trim_amount =
                                silence_duration_samples.saturating_sub(keep_silence_samples);

                            if speech_buffer.len() > trim_amount {
                                speech_buffer.truncate(speech_buffer.len() - trim_amount);
                            }

                            // Emit complete speech segment
                            let normalized_buffer = normalize_audio_level(&speech_buffer, 0.1);
                            match samples_to_wav_b64(sr, &normalized_buffer) {
                                Ok(b64) => {
                                    if let Err(e) = app.emit("speech-detected", b64) {
                                        error!("Failed to emit speech-detected: {}", e);
                                    }
                                }
                                Err(e) => {
                                    error!("Failed to encode speech to WAV: {}", e);
                                    if let Err(emit_err) =
                                        app.emit("audio-encoding-error", "Failed to encode speech")
                                    {
                                        error!(
                                            "Failed to emit audio-encoding-error: {}",
                                            emit_err
                                        );
                                    }
                                }
                            }
                        } else if let Err(e) = app.emit(
                            "speech-discarded",
                            "Audio too short (likely background noise)",
                        ) {
                            error!("Failed to emit speech-discarded: {}", e);
                        }

                        // Reset for next speech detection
                        speech_buffer.clear();
                        in_speech = false;
                        silence_chunks = 0;
                        speech_chunks = 0;
                    }
                } else {
                    // Not in speech yet - maintain rolling pre-speech buffer
                    pre_speech.extend(mono);

                    // Trim excess (maintain fixed size)
                    while pre_speech.len() > config.pre_speech_chunks * config.hop_size {
                        pre_speech.pop_front();
                    }

                    // Periodically shrink capacity to prevent memory bloat
                    if pre_speech.len() == config.pre_speech_chunks * config.hop_size {
                        pre_speech.shrink_to_fit();
                    }
                }
            }
        }
    }
}

// Continuous capture (VAD disabled)
async fn run_continuous_capture(
    app: AppHandle,
    stream: impl StreamExt<Item = f32> + Unpin,
    sr: u32,
    config: VadConfig,
) {
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

    // Process and emit audio
    if audio_buffer.is_empty() {
        warn!("No audio captured in continuous mode");
        if let Err(e) = app.emit("audio-encoding-error", "No audio recorded") {
            error!("Failed to emit audio-encoding-error: {}", e);
        }
    } else {
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
            // Nothing in the recording rises above the noise floor (e.g. the
            // monitored sink received no signal at all). Sending it to STT
            // would only produce an empty transcript; tell the user what
            // happened instead.
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
        } else {
            // Apply noise gate
            let cleaned_audio = apply_noise_gate(&audio_buffer, config.noise_gate_threshold);
            let cleaned_audio = normalize_audio_level(&cleaned_audio, 0.1);

            match samples_to_wav_b64(sr, &cleaned_audio) {
                Ok(b64) => {
                    if let Err(e) = app.emit("speech-detected", b64) {
                        error!("Failed to emit speech-detected (continuous): {}", e);
                    }
                }
                Err(e) => {
                    error!("Failed to encode continuous audio: {}", e);
                    if let Err(emit_err) = app.emit("audio-encoding-error", e) {
                        error!("Failed to emit audio-encoding-error: {}", emit_err);
                    }
                }
            }
        }
    }

    if let Err(e) = app.emit("continuous-recording-stopped", ()) {
        error!("Failed to emit continuous-recording-stopped: {}", e);
    }
}

// Apply noise gate
fn apply_noise_gate(samples: &[f32], threshold: f32) -> Vec<f32> {
    const KNEE_RATIO: f32 = 3.0; // Compression ratio for soft knee

    samples
        .iter()
        .map(|&s| {
            let abs = s.abs();
            if abs < threshold {
                s * (abs / threshold).powf(1.0 / KNEE_RATIO)
            } else {
                s
            }
        })
        .collect()
}

// Calculate RMS and peak (optimized)
fn calculate_audio_metrics(chunk: &[f32]) -> (f32, f32) {
    let mut sumsq = 0.0f32;
    let mut peak = 0.0f32;

    for &v in chunk {
        let a = v.abs();
        peak = peak.max(a);
        sumsq += v * v;
    }

    let rms = (sumsq / chunk.len() as f32).sqrt();
    (rms, peak)
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

// Convert samples to WAV base64 (with proper error handling)
fn samples_to_wav_b64(sample_rate: u32, mono_f32: &[f32]) -> Result<String, String> {
    // Validate sample rate
    if !(8000..=96000).contains(&sample_rate) {
        error!("Invalid sample rate: {}", sample_rate);
        return Err(format!(
            "Invalid sample rate: {}. Expected 8000-96000 Hz",
            sample_rate
        ));
    }

    // Validate buffer
    if mono_f32.is_empty() {
        return Err("Empty audio buffer".to_string());
    }

    let mut cursor = Cursor::new(Vec::new());
    let spec = WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    let mut writer = WavWriter::new(&mut cursor, spec).map_err(|e| {
        error!("Failed to create WAV writer: {}", e);
        e.to_string()
    })?;

    for &s in mono_f32 {
        let clamped = s.clamp(-1.0, 1.0);
        let sample_i16 = (clamped * i16::MAX as f32) as i16;
        writer.write_sample(sample_i16).map_err(|e| e.to_string())?;
    }

    writer.finalize().map_err(|e| e.to_string())?;

    Ok(B64.encode(cursor.into_inner()))
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
    // Validate config
    if config.sensitivity_rms < 0.0 || config.sensitivity_rms > 1.0 {
        return Err("Invalid sensitivity_rms: must be 0.0-1.0".to_string());
    }
    if config.max_recording_duration_secs > 3600 {
        return Err("Invalid max_recording_duration_secs: must be <= 3600 (1 hour)".to_string());
    }

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
            .start(|| {
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
            (&[StartFinite(10), WaitFinished, Start], &[true, true, true], 1, 1),
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
            assert_eq!(live.now.load(Ordering::SeqCst), *now, "{ops:?} live streams");
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
        assert_eq!(live.now.load(Ordering::SeqCst), 0, "orphaned capture still live");
        assert!(live.max.load(Ordering::SeqCst) <= 1, "two captures ran at once");
    }
}
