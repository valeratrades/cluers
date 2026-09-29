//! Spec for the VAD segmenter. Fixtures: `tests/fixtures/vad/gen.sh`.
use pluely_lib::vad::{Segmenter, VadConfig, VadEvent};
use std::f32::consts::TAU;

mod common;
use common::{load, truth};

const RATES: [u32; 3] = [16000, 44100, 48000];
const TOLERANCE_MS: i64 = 50;

#[derive(Debug, Clone, Copy)]
enum Variant {
    Clean,
    Hum,
    Clicks,
}

/// Utterances closer than `silence_ms` belong to one segment.
fn expected(truth: &[(u64, u64)], silence_ms: u64) -> Vec<(u64, u64)> {
    let mut out: Vec<(u64, u64)> = Vec::new();
    for &(s, e) in truth {
        match out.last_mut() {
            Some(last) if s - last.1 < silence_ms => last.1 = e,
            _ => out.push((s, e)),
        }
    }
    out
}

fn apply(variant: Variant, samples: &mut [f32], rate: u32, truth: &[(u64, u64)]) {
    let at = |ms: u64| (ms * rate as u64 / 1000) as usize;
    match variant {
        Variant::Clean => {}
        Variant::Hum => {
            for (i, s) in samples.iter_mut().enumerate() {
                let t = i as f32 / rate as f32;
                *s += 0.03 * (TAU * 50.0 * t).sin() + 0.01 * (TAU * 150.0 * t).sin();
            }
        }
        Variant::Clicks => {
            let total_ms = samples.len() as u64 * 1000 / rate as u64;
            let mut edges = vec![0];
            edges.extend(truth.iter().flat_map(|&(s, e)| [s, e]));
            edges.push(total_ms);
            for gap in edges.chunks(2) {
                let (from, to) = (gap[0] + 100, gap[1].saturating_sub(100));
                if gap[1] - gap[0] < 1000 {
                    continue;
                }
                for (k, ms) in (from..to).step_by(700).enumerate() {
                    let sign = if k % 2 == 0 { 0.6 } else { -0.6 };
                    samples[at(ms)..at(ms + 1)]
                        .iter_mut()
                        .for_each(|s| *s += sign);
                }
            }
        }
    }
}

/// Segments as (start_ms, end_ms), plus the number of discarded bursts.
fn run(samples: &[f32], rate: u32, config: &VadConfig) -> (Vec<(u64, u64)>, usize) {
    let mut vad = Segmenter::new(config, rate).unwrap();
    let (mut segs, mut discarded) = (Vec::new(), 0);
    for chunk in samples.chunks(4096) {
        for ev in vad.push(chunk) {
            match ev {
                VadEvent::Segment {
                    start_ms,
                    end_ms,
                    samples,
                } => {
                    assert!(!samples.is_empty());
                    segs.push((start_ms, end_ms));
                }
                VadEvent::Discarded { .. } => discarded += 1,
                VadEvent::SpeechStart { .. } | VadEvent::Metrics { .. } => {}
            }
        }
    }
    (segs, discarded)
}

#[test]
fn segments_match_ground_truth() {
    let config = VadConfig::default();
    let mut failures = Vec::new();
    for name in ["pauses", "long"] {
        let truth = truth(name);
        let want = expected(&truth, config.silence_ms as u64);
        for variant in [Variant::Clean, Variant::Hum, Variant::Clicks] {
            let mut per_rate = Vec::new();
            for rate in RATES {
                let mut samples = load(name, rate);
                apply(variant, &mut samples, rate, &truth);
                let (got, discarded) = run(&samples, rate, &config);
                let ok = discarded == 0
                    && got.len() == want.len()
                    && got.iter().zip(&want).all(|(g, w)| {
                        (g.0 as i64 - w.0 as i64).abs() <= TOLERANCE_MS
                            && (g.1 as i64 - w.1 as i64).abs() <= TOLERANCE_MS
                    });
                if !ok {
                    failures.push(format!(
                        "{name} {variant:?} @{rate}: got {got:?} (+{discarded} discarded), want {want:?}"
                    ));
                }
                per_rate.push((rate, got));
            }
            let (r0, first) = &per_rate[0];
            for (rate, got) in &per_rate[1..] {
                let same = got.len() == first.len()
                    && got.iter().zip(first).all(|(a, b)| {
                        (a.0 as i64 - b.0 as i64).abs() <= config.hop_ms as i64
                            && (a.1 as i64 - b.1 as i64).abs() <= config.hop_ms as i64
                    });
                if !same {
                    failures.push(format!(
                        "{name} {variant:?}: @{rate} {got:?} differs from @{r0} {first:?}"
                    ));
                }
            }
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

#[test]
fn config_rejected() {
    type Mutate = fn(&mut VadConfig, &mut u32);
    let cases: &[(&str, Mutate, bool)] = &[
        ("default", |_, _| {}, true),
        ("hop 0", |c, _| c.hop_ms = 0, false),
        ("silence < hop", |c, _| c.silence_ms = c.hop_ms - 1, false),
        (
            "cap <= silence",
            |c, _| c.max_segment_ms = c.silence_ms,
            false,
        ),
        ("NaN threshold", |c, _| c.sensitivity_rms = f32::NAN, false),
        ("sr 4000", |_, sr| *sr = 4000, false),
    ];
    for (label, mutate, ok) in cases {
        let (mut config, mut sr) = (VadConfig::default(), 48000);
        mutate(&mut config, &mut sr);
        assert_eq!(Segmenter::new(&config, sr).is_ok(), *ok, "{label}");
    }
}

#[test]
fn max_segment_cap() {
    let config = VadConfig {
        max_segment_ms: 1500,
        ..VadConfig::default()
    };
    let (segs, _) = run(&load("long", 48000), 48000, &config);
    assert!(segs.len() > 1, "{segs:?}");
    for (s, e) in &segs {
        assert!(e - s <= 1500, "{segs:?}");
    }
    for w in segs.windows(2) {
        assert_eq!(w[0].1, w[1].0, "capped pieces not contiguous: {segs:?}");
    }
}
