//! Pure speech segmenter: samples in, [`VadEvent`]s out. No IPC, no clocks.
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VadConfig {
    pub enabled: bool,
    pub hop_ms: u32,
    pub sensitivity_rms: f32,
    pub peak_threshold: f32,
    pub silence_ms: u32,
    pub min_speech_ms: u32,
    pub pre_speech_ms: u32,
    pub max_segment_ms: u32,
    pub noise_gate_threshold: f32,
    pub max_recording_duration_secs: u64, // continuous mode only
}

impl Default for VadConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            hop_ms: 20,
            sensitivity_rms: 0.012,
            peak_threshold: 0.035,
            silence_ms: 1000,
            min_speech_ms: 160,
            pre_speech_ms: 300,
            max_segment_ms: 30_000,
            noise_gate_threshold: 0.003,
            max_recording_duration_secs: 180,
        }
    }
}

impl VadConfig {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if !(1..=100).contains(&self.hop_ms) {
            return Err(format!("Invalid hop_ms {}: must be 1-100", self.hop_ms));
        }
        if self.silence_ms < self.hop_ms {
            return Err(format!(
                "Invalid silence_ms {}: must be >= hop_ms ({})",
                self.silence_ms, self.hop_ms
            ));
        }
        if self.max_segment_ms <= self.silence_ms {
            return Err(format!(
                "Invalid max_segment_ms {}: must be > silence_ms ({})",
                self.max_segment_ms, self.silence_ms
            ));
        }
        if self.max_recording_duration_secs > 3600 {
            return Err("Invalid max_recording_duration_secs: must be <= 3600 (1 hour)".into());
        }
        for (name, v) in [
            ("sensitivity_rms", self.sensitivity_rms),
            ("peak_threshold", self.peak_threshold),
            ("noise_gate_threshold", self.noise_gate_threshold),
        ] {
            if !(0.0..=1.0).contains(&v) {
                return Err(format!("Invalid {name} {v}: must be 0.0-1.0"));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum VadEvent {
    SpeechStart { start_ms: u64 },
    /// `start_ms`/`end_ms` are the detected speech on/offset; `samples` also carry pre-roll and a short tail.
    Segment { samples: Vec<f32>, start_ms: u64, end_ms: u64 },
    Discarded { start_ms: u64, end_ms: u64 },
    Metrics { rms: f32, peak: f32, in_speech: bool },
}

const TAIL_MS: u32 = 150;
const METRICS_MS: u32 = 100;

struct Active {
    buf: Vec<f32>,
    buf_origin: u64, // absolute sample index of buf[0]
    start_sample: u64,
    last_speech_end: u64,
    speech_hops: u32,
    silence_hops: u32,
}

pub struct Segmenter {
    sr: u32,
    hop_len: usize,
    sensitivity_rms: f32,
    peak_threshold: f32,
    noise_gate_threshold: f32,
    silence_hops: u32,
    min_speech_hops: u32,
    max_segment_hops: u32,
    metrics_hops: u32,
    pre_len: usize,
    tail_len: u64,
    pending: Vec<f32>,
    samples_seen: u64,
    hops_seen: u64,
    pre_roll: VecDeque<f32>,
    active: Option<Active>,
}

impl Segmenter {
    pub fn new(config: &VadConfig, sample_rate: u32) -> Result<Self, String> {
        config.validate()?;
        if !(8000..=96000).contains(&sample_rate) {
            return Err(format!(
                "Invalid sample rate: {sample_rate}. Expected 8000-96000 Hz"
            ));
        }
        let hops = |ms: u32| ms.div_ceil(config.hop_ms);
        let hop_len = (sample_rate as u64 * config.hop_ms as u64 / 1000) as usize;
        assert!(hop_len > 0, "sr >= 8000 and hop_ms >= 1 give >= 8 samples");
        Ok(Self {
            sr: sample_rate,
            hop_len,
            sensitivity_rms: config.sensitivity_rms,
            peak_threshold: config.peak_threshold,
            noise_gate_threshold: config.noise_gate_threshold,
            silence_hops: hops(config.silence_ms),
            min_speech_hops: hops(config.min_speech_ms),
            max_segment_hops: hops(config.max_segment_ms),
            metrics_hops: hops(METRICS_MS),
            pre_len: hops(config.pre_speech_ms) as usize * hop_len,
            tail_len: sample_rate as u64 * TAIL_MS as u64 / 1000,
            pending: Vec::new(),
            samples_seen: 0,
            hops_seen: 0,
            pre_roll: VecDeque::new(),
            active: None,
        })
    }

    pub fn push(&mut self, samples: &[f32]) -> Vec<VadEvent> {
        self.pending.extend_from_slice(samples);
        let mut events = Vec::new();
        let n_hops = self.pending.len() / self.hop_len;
        let pending = std::mem::take(&mut self.pending);
        for hop in pending.chunks_exact(self.hop_len).take(n_hops) {
            self.hop(hop, &mut events);
        }
        self.pending = pending[n_hops * self.hop_len..].to_vec();
        events
    }

    fn ms(&self, sample: u64) -> u64 {
        sample * 1000 / self.sr as u64
    }

    fn hop(&mut self, raw: &[f32], events: &mut Vec<VadEvent>) {
        let hop_start = self.samples_seen;
        let hop_end = hop_start + raw.len() as u64;
        self.samples_seen = hop_end;

        let mono = apply_noise_gate(raw, self.noise_gate_threshold);
        let (rms, peak) = calculate_audio_metrics(&mono);
        let is_speech = rms > self.sensitivity_rms || peak > self.peak_threshold;

        if self.hops_seen % self.metrics_hops as u64 == 0 {
            events.push(VadEvent::Metrics { rms, peak, in_speech: self.active.is_some() });
        }
        self.hops_seen += 1;

        let Some(a) = &mut self.active else {
            if is_speech {
                let pre: Vec<f32> = self.pre_roll.drain(..).collect();
                let mut buf = pre;
                let buf_origin = hop_start - buf.len() as u64;
                buf.extend_from_slice(&mono);
                self.active = Some(Active {
                    buf,
                    buf_origin,
                    start_sample: hop_start,
                    last_speech_end: hop_end,
                    speech_hops: 1,
                    silence_hops: 0,
                });
                events.push(VadEvent::SpeechStart { start_ms: self.ms(hop_start) });
            } else {
                self.pre_roll.extend(mono);
                let excess = self.pre_roll.len().saturating_sub(self.pre_len);
                self.pre_roll.drain(..excess);
            }
            return;
        };

        a.buf.extend_from_slice(&mono);
        if is_speech {
            a.speech_hops += 1;
            a.silence_hops = 0;
            a.last_speech_end = hop_end;
        } else {
            a.silence_hops += 1;
        }

        if (hop_end - a.start_sample) >= self.max_segment_hops as u64 * self.hop_len as u64 {
            let a = self.active.take().expect("matched Some above");
            events.push(VadEvent::Segment {
                samples: a.buf,
                start_ms: self.ms(a.start_sample),
                end_ms: self.ms(hop_end),
            });
            if is_speech {
                self.active = Some(Active {
                    buf: Vec::new(),
                    buf_origin: hop_end,
                    start_sample: hop_end,
                    last_speech_end: hop_end,
                    speech_hops: 0,
                    silence_hops: 0,
                });
                events.push(VadEvent::SpeechStart { start_ms: self.ms(hop_end) });
            }
            return;
        }

        if a.silence_hops >= self.silence_hops {
            let mut a = self.active.take().expect("matched Some above");
            let (start_ms, end_ms) = (self.ms(a.start_sample), self.ms(a.last_speech_end));
            if a.speech_hops >= self.min_speech_hops {
                let keep = (a.last_speech_end + self.tail_len - a.buf_origin) as usize;
                a.buf.truncate(keep);
                events.push(VadEvent::Segment { samples: a.buf, start_ms, end_ms });
            } else {
                events.push(VadEvent::Discarded { start_ms, end_ms });
            }
        }
    }
}

/// Soft-knee gate: attenuates samples below `threshold` instead of zeroing them.
pub(super) fn apply_noise_gate(samples: &[f32], threshold: f32) -> Vec<f32> {
    const KNEE_RATIO: f32 = 3.0;
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

pub(super) fn calculate_audio_metrics(chunk: &[f32]) -> (f32, f32) {
    let mut sumsq = 0.0f32;
    let mut peak = 0.0f32;
    for &v in chunk {
        peak = peak.max(v.abs());
        sumsq += v * v;
    }
    ((sumsq / chunk.len() as f32).sqrt(), peak)
}
