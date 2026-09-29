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
use std::io::Cursor;
use std::task::Poll;
use std::time::Duration;
use tauri::ipc::Channel;
use tauri::{AppHandle, Emitter, Manager, Runtime};
use tauri_plugin_shell::ShellExt;
use tokio::sync::{mpsc, watch};
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
    Record { action: Record },
}

/// Continuous-mode recording actions.
#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "camelCase")]
pub enum Record {
    Start,
    Send,
    Discard,
}

/// Payload of `continuous-recording-stopped`.
#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "camelCase")]
enum Stopped {
    Sent,
    Limit,
    Discarded,
}

pub(crate) struct Capture {
    task: JoinHandle<()>,
    control: mpsc::UnboundedSender<Control>,
    record: mpsc::UnboundedSender<Record>,
}

#[tauri::command]
pub async fn start_system_audio_capture(
    app: AppHandle,
    vad_config: VadConfig,
    device_id: Option<String>,
    mic_device_id: Option<String>,
    session: Session,
    events: Channel<TurnEvent>,
) -> Result<(), String> {
    let state = app.state::<crate::AudioState>();
    let app_clone = app.clone();
    state
        .start(|control, record| {
            vad_config.validate()?;
            state.vad.send_replace(vad_config.clone());
            let vad = state.vad.subscribe();
            let record = (!vad_config.enabled).then_some(record);

            // opened first: open skew then only delays the mic, so echo never precedes its source
            let mic = vad_config
                .enabled
                .then(|| SpeakerInput::microphone(mic_device_id))
                .transpose()
                .map_err(|e| {
                    error!("Failed to open microphone: {e:#}");
                    format!("Failed to access microphone: {e}")
                })?
                .map(SpeakerInput::stream);

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

            let vads = mic
                .map(|mic| -> Result<_, String> {
                    assert_eq!(mic.sample_rate(), sr, "both Pulse streams request 44.1kHz");
                    Ok((
                        mic,
                        Segmenter::new(&vad_config, sr)?,
                        Segmenter::new(&vad_config, sr)?,
                    ))
                })
                .transpose()?;

            Ok(async move {
                let app = app_clone;
                let mut stream = stream;
                let Some((mut mic, mut sys_seg, mut mic_seg)) = vads else {
                    let record = record.expect("kept in continuous mode");
                    let audio = recordings(&app, &mut stream, record, sr, vad.clone());
                    drive(&app, sr, vad, audio, session, control, events).await;
                    report_stream_error(&app, stream.error());
                    return;
                };
                // ponytail: one sample clock for both devices; under plain PulseAudio (no PipeWire rate matching) clock drift is ~0.2s/h worst case. Resync on timestamps if long sessions show it.
                let (mut seen, mut stalled, mut live) = (0u64, None, vad.clone());
                let audio = lockstep(&mut stream, &mut mic, 2 * sr as usize, &mut stalled)
                    .map(|(s, m)| {
                        if live.has_changed().expect("AudioState owns the sender") {
                            let c = live.borrow_and_update().clone();
                            sys_seg.reconfigure(&c).expect("validated by update_vad_config");
                            mic_seg.reconfigure(&c).expect("validated by update_vad_config");
                        }
                        seen += s.len() as u64;
                        let mut ev: Vec<_> = sys_seg
                            .push(&s)
                            .into_iter()
                            .map(|e| (Speaker::Interviewer, e))
                            .collect();
                        ev.extend(mic_seg.push(&m).into_iter().map(|e| (Speaker::User, e))); // interviewer first: echo never precedes its source
                        (ev, seen * 1000 / sr as u64)
                    });
                drive(&app, sr, vad, audio, session, control, events).await;
                report_stream_error(&app, stream.error());
                report_stream_error(&app, mic.error().map(|e| e.context("Microphone")));
                report_stream_error(&app, stalled.map(anyhow::Error::msg));
            })
        })
        .await
}

/// Pairs system and mic samples in equal-length chunks. Ends when either ends, or with `stalled` set once one side runs `max_lag` samples ahead.
fn lockstep<'a, S: Stream<Item = f32> + Unpin + 'a>(
    sys: S,
    mic: S,
    max_lag: usize,
    stalled: &'a mut Option<String>,
) -> impl Stream<Item = (Vec<f32>, Vec<f32>)> + 'a {
    let (mut sys, mut mic) = (sys.ready_chunks(4096), mic.ready_chunks(4096));
    let (mut sys_buf, mut mic_buf) = (Vec::new(), Vec::new());
    stream::poll_fn(move |cx| loop {
        let mut progressed = false;
        for (side, buf) in [(&mut sys, &mut sys_buf), (&mut mic, &mut mic_buf)] {
            match side.poll_next_unpin(cx) {
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Ready(Some(c)) => {
                    buf.extend(c);
                    progressed = true;
                }
                Poll::Pending => {}
            }
        }
        let n = sys_buf.len().min(mic_buf.len());
        for (lagging, ahead) in [("Microphone", &sys_buf), ("System audio", &mic_buf)] {
            if ahead.len() - n > max_lag {
                *stalled = Some(format!(
                    "{lagging} stopped delivering audio while the other device kept going"
                ));
                return Poll::Ready(None);
            }
        }
        if n > 0 {
            return Poll::Ready(Some((
                sys_buf.drain(..n).collect(),
                mic_buf.drain(..n).collect(),
            )));
        }
        if !progressed {
            return Poll::Pending;
        }
    })
}

fn report_stream_error(app: &AppHandle, e: Option<anyhow::Error>) {
    let Some(e) = e else { return };
    error!("System audio capture ended: {e:#}");
    emit(app, "capture-error", format!("{e:#}"));
}

fn emit<R: Runtime>(app: &AppHandle<R>, event: &str, payload: impl Serialize + Clone) {
    if let Err(e) = app.emit(event, payload) {
        error!("Failed to emit {event}: {e}");
    }
}

#[tauri::command]
pub async fn system_audio_control(app: AppHandle, control: Control) -> Result<(), String> {
    app.state::<crate::AudioState>().control(control).await
}

/// Continuous mode: one segment per recording, started and ended by `actions`. Ends when either source ends.
fn recordings<'a, R: Runtime>(
    app: &'a AppHandle<R>,
    samples: impl Stream<Item = f32> + Unpin + 'a,
    mut actions: mpsc::UnboundedReceiver<Record>,
    sr: u32,
    vad: watch::Receiver<VadConfig>,
) -> impl Stream<Item = (Vec<(Speaker, VadEvent)>, u64)> + 'a {
    let mut samples = samples.ready_chunks(4096);
    let (mut buf, mut start, mut seen): (Option<Vec<f32>>, u64, u64) = (None, 0, 0);
    let finish = move |samples: Vec<f32>,
                       start: u64,
                       how: Stopped,
                       cfg: &VadConfig|
          -> Option<(Vec<(Speaker, VadEvent)>, u64)> {
        let ms = |s: u64| s * 1000 / sr as u64;
        let (start_ms, end_ms) = (ms(start), ms(start + samples.len() as u64));
        let discard = |why: &str| {
            emit(app, "speech-discarded", why);
            emit(app, "continuous-recording-stopped", Stopped::Discarded);
            None
        };
        if end_ms - start_ms < (cfg.min_speech_ms as u64).max(1) {
            return discard("recording too short"); // max(1): keeps segment ids unique for Turns
        }
        let (_, peak) = calculate_audio_metrics(&samples);
        if peak < cfg.noise_gate_threshold {
            warn!(
                "Continuous recording contained no audible signal (peak {peak:.6}, gate {:.6})",
                cfg.noise_gate_threshold
            );
            return discard(
                "recording was silent - check that audio is routed to the captured device",
            );
        }
        emit(app, "continuous-recording-stopped", how);
        let samples = apply_noise_gate(&samples, cfg.noise_gate_threshold);
        let segment = VadEvent::Segment {
            samples,
            start_ms,
            end_ms,
        };
        Some((vec![(Speaker::Interviewer, segment)], end_ms))
    };
    stream::poll_fn(move |cx| loop {
        match actions.poll_recv(cx) {
            Poll::Ready(None) => return Poll::Ready(None),
            Poll::Ready(Some(Record::Start)) => {
                if buf.is_none() {
                    buf = Some(Vec::new());
                    start = seen;
                    emit(
                        app,
                        "continuous-recording-start",
                        vad.borrow().max_recording_duration_secs,
                    );
                } // else Enter and a click raced
                continue;
            }
            Poll::Ready(Some(action)) => {
                let Some(b) = buf.take() else { continue }; // raced the limit auto-send
                if let Record::Send = action {
                    if let Some(item) = finish(b, start, Stopped::Sent, &vad.borrow()) {
                        return Poll::Ready(Some(item));
                    }
                } else {
                    emit(app, "continuous-recording-stopped", Stopped::Discarded);
                }
                continue;
            }
            Poll::Pending => {}
        }
        let chunk = match samples.poll_next_unpin(cx) {
            Poll::Ready(Some(chunk)) => chunk,
            Poll::Ready(None) => return Poll::Ready(None),
            Poll::Pending => return Poll::Pending,
        };
        seen += chunk.len() as u64;
        let Some(b) = &mut buf else { continue };
        let limit = sr as usize * vad.borrow().max_recording_duration_secs as usize;
        let before = b.len() / sr as usize;
        b.extend(chunk);
        b.truncate(limit);
        for secs in before + 1..=b.len() / sr as usize {
            emit(app, "recording-progress", secs);
        }
        if b.len() == limit {
            let b = buf.take().expect("matched Some above");
            if let Some(item) = finish(b, start, Stopped::Limit, &vad.borrow()) {
                return Poll::Ready(Some(item));
            }
        }
    })
}

impl crate::AudioState {
    /// `Record` goes to the continuous recorder, everything else to `drive`.
    async fn control(&self, control: Control) -> Result<(), String> {
        let slot = self.capture.lock().await;
        let c = slot.as_ref().ok_or("System audio capture is not running")?;
        // SendError only hands our own message back; a closed receiver is the whole error
        match control {
            Control::Record { action } => c.record.send(action).map_err(|_| {
                "Continuous recording is not available (auto-detect mode or capture ended)"
                    .to_string()
            }),
            other => c
                .control
                .send(other)
                .map_err(|_| "System audio capture is not running".to_string()),
        }
    }

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
        open: impl FnOnce(
            mpsc::UnboundedReceiver<Control>,
            mpsc::UnboundedReceiver<Record>,
        ) -> Result<F, String>,
    ) -> Result<(), String>
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let mut slot = self.lock_idle().await?;
        let (control, control_rx) = mpsc::unbounded_channel();
        let (record, record_rx) = mpsc::unbounded_channel();
        *slot = Some(Capture {
            task: tokio::spawn(open(control_rx, record_rx)?),
            control,
            record,
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
    mut vad: watch::Receiver<VadConfig>,
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
    let mut vad_cfg = vad.borrow_and_update().clone();
    let continuous = !vad_cfg.enabled;
    let mut turns = Turns::new(history, carry, &vad_cfg);
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
                                if continuous {
                                    inputs.push(Input::Flush); // each recording is one turn
                                }
                            }
                            VadEvent::Discarded { .. } => {
                                if speaker == Speaker::Interviewer {
                                    emit(app, "speech-discarded", "Audio too short (likely background noise)");
                                }
                                inputs.push(Input::Discarded { speaker });
                            }
                            VadEvent::Metrics { .. } if speaker == Speaker::User => {} // meters and calibration are about system audio
                            VadEvent::Metrics { rms, peak, in_speech } => emit(
                                app,
                                "vad-metrics",
                                VadMetrics {
                                    rms,
                                    peak,
                                    sensitivity_rms: vad_cfg.sensitivity_rms,
                                    peak_threshold: vad_cfg.peak_threshold,
                                    noise_gate_threshold: vad_cfg.noise_gate_threshold,
                                    in_speech,
                                },
                            ),
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
                Control::Record { .. } => unreachable!("AudioState::control routes it to the recorder"),
            },
            Ok(()) = vad.changed() => {
                vad_cfg = vad.borrow_and_update().clone();
                turns.reconfigure(&vad_cfg);
                Vec::new()
            }
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

    const HOP: usize = 1024;
    let target_chunks = ((sr as usize * duration_secs as usize) / HOP).max(8);
    let mut floor_samples: Vec<f32> = Vec::with_capacity(target_chunks);
    let sampling = async {
        let mut chunks = (&mut stream).ready_chunks(4096);
        let mut pending = Vec::new();
        while floor_samples.len() < target_chunks {
            let chunk = chunks.next().await.ok_or("Audio source ended during calibration")?;
            pending.extend(chunk);
            let n = pending.len() / HOP * HOP;
            floor_samples.extend(pending[..n].chunks_exact(HOP).map(|h| calculate_audio_metrics(h).0));
            pending.drain(..n);
        }
        floor_samples.truncate(target_chunks);
        Ok::<_, &str>(())
    };
    let sampled = tokio::time::timeout(Duration::from_secs(duration_secs + 2), sampling).await;
    let ended = stream.error();
    drop(stream);
    drop(idle); // release device before admitting a start
    match sampled {
        Err(_) => return Err(format!("Calibration timed out: the source delivered no audio for {}s", duration_secs + 2)),
        Ok(Err(e)) => return Err(match ended {
            Some(cause) => format!("{e}: {cause:#}"),
            None => e.to_string(),
        }),
        Ok(Ok(())) => {}
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
    app.state::<crate::AudioState>().vad.send_replace(config);
    Ok(())
}

/// The single source of VAD defaults.
#[tauri::command]
pub fn default_vad_config() -> VadConfig {
    VadConfig::default()
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
            .start(|control, record| {
                drop(record); // like the VAD path
                let mut s = FakeStream::new(live, remaining);
                Ok(async move {
                    while s.next().await.is_some() {}
                    drop(control);
                })
            })
            .await
    }

    #[derive(Debug)]
    enum Op {
        Start,
        StartFinite(usize),
        WaitFinished,
        Stop,
        Prompt,
        Record,
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
            (&[Start, Prompt], &[true, true], 1, 1),
            (&[Start, Record], &[true, false], 1, 1),
            (&[Prompt, Record], &[false, false], 0, 0),
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
                    Prompt => state
                        .control(super::Control::Prompt { text: "q".into() })
                        .await
                        .is_ok(),
                    Record => state
                        .control(super::Control::Record {
                            action: super::Record::Start,
                        })
                        .await
                        .is_ok(),
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

    #[tokio::test]
    async fn lockstep_errs_naming_the_stalled_device() {
        let feed = |n: usize| futures_util::stream::iter(vec![0.0f32; n]).chain(futures_util::stream::pending());
        for (sys, mic, want) in [
            (20_000, 100, "Microphone stopped"),
            (100, 20_000, "System audio stopped"),
        ] {
            let (mut paired, mut stalled) = (0, None);
            let mut pairs = super::lockstep(feed(sys), feed(mic), 5000, &mut stalled);
            while let Some((s, m)) =
                tokio::time::timeout(std::time::Duration::from_secs(1), pairs.next())
                    .await
                    .expect("a stall ends the stream instead of waiting")
            {
                assert_eq!(s.len(), m.len());
                paired += s.len();
            }
            drop(pairs);
            assert_eq!(paired, 100);
            let err = stalled.expect("stall reported");
            assert!(err.starts_with(want), "{err}");
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

    #[derive(Debug, Clone, Copy)]
    enum Step {
        Loud(u64),
        Quiet(u64),
        Act(super::Record),
        Max(u64),
    }

    #[test]
    fn recordings() {
        use super::Record::{Discard, Send, Start};
        use futures_util::FutureExt;
        use std::sync::Mutex;
        use tauri::test::{mock_builder, mock_context, noop_assets};
        use tauri::Listener;
        use Step::*;
        const SR: u32 = 44100;
        type Log = Arc<Mutex<Vec<String>>>;
        type Segment = (u64, u64, usize); // (start_ms, end_ms, n_samples)
        // (script, segments, stopped reasons, progress seconds)
        #[rustfmt::skip]
        let cases: &[(&str, &[Step], &[Segment], &[&str], &[u64])] = &[
            ("between start and send", &[Loud(1000), Act(Start), Loud(2000), Act(Send)], &[(1000, 3000, 88200)], &["sent"], &[1, 2]),
            ("discard", &[Act(Start), Loud(1000), Act(Discard)], &[], &["discarded"], &[1]),
            ("limit auto-sends", &[Max(1), Act(Start), Loud(1500), Act(Send)], &[(0, 1000, 44100)], &["limit"], &[1]),
            ("send while idle", &[Act(Send)], &[], &[], &[]),
            ("silent recording", &[Act(Start), Quiet(1000), Act(Send)], &[], &["discarded"], &[1]),
            ("shorter than min_speech_ms", &[Act(Start), Loud(50), Act(Send)], &[], &["discarded"], &[]),
            ("start while recording", &[Act(Start), Loud(1000), Act(Start), Loud(1000), Act(Send)], &[(0, 2000, 88200)], &["sent"], &[1, 2]),
            ("progress", &[Act(Start), Loud(2500)], &[], &[], &[1, 2]),
            ("live max change", &[Act(Start), Loud(500), Max(1), Loud(600)], &[(0, 1000, 44100)], &["limit"], &[1]),
        ];
        for (name, script, want_segs, want_stopped, want_progress) in cases {
            let app = mock_builder().build(mock_context(noop_assets())).unwrap();
            let log = |event: &'static str| -> Log {
                let got = Log::default();
                let sink = got.clone();
                app.listen_any(event, move |e| {
                    sink.lock()
                        .unwrap()
                        .push(e.payload().trim_matches('"').to_string())
                });
                got
            };
            let (stopped, progress, discarded) = (
                log("continuous-recording-stopped"),
                log("recording-progress"),
                log("speech-discarded"),
            );
            let (samples_tx, mut samples_rx) = tokio::sync::mpsc::unbounded_channel::<f32>();
            let (actions_tx, actions_rx) = tokio::sync::mpsc::unbounded_channel();
            let (vad_tx, vad_rx) = tokio::sync::watch::channel(crate::vad::VadConfig {
                enabled: false,
                ..Default::default()
            });
            let samples = futures_util::stream::poll_fn(move |cx| samples_rx.poll_recv(cx));
            let mut out = Box::pin(super::recordings(
                app.handle(),
                samples,
                actions_rx,
                SR,
                vad_rx,
            ));
            let mut segs = Vec::new();
            for step in *script {
                let n = |ms: u64| ms * SR as u64 / 1000;
                match *step {
                    Loud(ms) => (0..n(ms))
                        .for_each(|i| samples_tx.send(if i % 2 == 0 { 0.5 } else { -0.5 }).unwrap()),
                    Quiet(ms) => (0..n(ms)).for_each(|_| samples_tx.send(0.0).unwrap()),
                    Act(a) => actions_tx.send(a).unwrap(),
                    Max(secs) => vad_tx.send_modify(|c| c.max_recording_duration_secs = secs),
                }
                while let Some(item) = out.next().now_or_never() {
                    let (events, _) = item.expect("sources are still open");
                    for (_, ev) in events {
                        let crate::vad::VadEvent::Segment {
                            samples,
                            start_ms,
                            end_ms,
                        } = ev
                        else {
                            panic!("{name}: recordings only yield segments");
                        };
                        segs.push((start_ms, end_ms, samples.len()));
                    }
                }
            }
            assert_eq!(&segs, want_segs, "{name}: segments");
            assert_eq!(*stopped.lock().unwrap(), *want_stopped, "{name}: stopped");
            let progress: Vec<u64> = progress
                .lock()
                .unwrap()
                .iter()
                .map(|s| s.parse().unwrap())
                .collect();
            assert_eq!(&progress, want_progress, "{name}: progress");
            assert_eq!(
                discarded.lock().unwrap().len(),
                usize::from(matches!(*name, "silent recording" | "shorter than min_speech_ms")),
                "{name}: speech-discarded"
            );
        }
    }
}
