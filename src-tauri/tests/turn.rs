//! Spec for the turn machine. Audio cases run the `pauses` VAD fixture through the real segmenter.
use pluely_lib::turn::{Input, Output, TurnEvent, Turns};
use pluely_lib::vad::{Segmenter, VadConfig, VadEvent};
use std::collections::VecDeque;

mod common;
use common::{load, truth};

const RATES: [u32; 3] = [16000, 44100, 48000];

fn trace(outs: Vec<Output>, into: &mut Vec<String>, mut on_ask: impl FnMut()) {
    for o in outs {
        match o {
            Output::Ask { message, history } => {
                into.push(format!("ask {message:?} h={}", history.len()));
                on_ask();
            }
            Output::Event(e) => match e {
                TurnEvent::Answered { .. } => into.push("answered".into()),
                TurnEvent::Skipped { carried, .. } => into.push(format!(
                    "skipped {}",
                    if carried { "carried" } else { "copy" }
                )),
                TurnEvent::NoSpeech => into.push("nospeech".into()),
                TurnEvent::Failed { error } => into.push(format!("failed {error}")),
                TurnEvent::Heard { .. } | TurnEvent::Asked { .. } | TurnEvent::Delta { .. } => {}
            },
        }
    }
}

struct Case {
    name: &'static str,
    stt_delay_ms: &'static [u64], // per segment index; missing = 0
    reply_delay_ms: u64,
    replies: &'static [&'static str],
    want: &'static [&'static str],
}

/// Delivers fake STT/LLM completions on the audio clock.
struct Sim<'a> {
    turns: Turns,
    replies: VecDeque<&'a str>,
    reply_delay_ms: u64,
    due: Vec<(u64, Input)>,
    out: Vec<String>,
}

impl Sim<'_> {
    fn feed(&mut self, input: Input, now: u64) {
        let mut asked = 0;
        trace(self.turns.push(input), &mut self.out, || asked += 1);
        for _ in 0..asked {
            let r = self
                .replies
                .pop_front()
                .expect("more asks than scripted replies");
            self.due
                .push((now + self.reply_delay_ms, Input::Reply(Ok(r.to_string()))));
        }
    }

    fn deliver(&mut self, until: u64) {
        loop {
            self.due.sort_by_key(|d| d.0); // stable: same-time completions keep issue order
            if self.due.first().is_none_or(|d| d.0 > until) {
                return;
            }
            let (at, input) = self.due.remove(0);
            self.feed(input, at);
        }
    }
}

/// Fake STT names the truth utterances a segment overlaps; fake LLM replies from `case.replies`.
fn run(case: &Case, rate: u32) -> Vec<String> {
    let truth = truth("pauses");
    let samples = load("pauses", rate);
    let mut vad = Segmenter::new(&VadConfig::default(), rate).unwrap();
    let mut sim = Sim {
        turns: Turns::new(Vec::new(), String::new()),
        replies: case.replies.iter().copied().collect(),
        reply_delay_ms: case.reply_delay_ms,
        due: Vec::new(),
        out: Vec::new(),
    };
    let (mut seen, mut seg_idx) = (0u64, 0usize);

    for chunk in samples.chunks(4096) {
        seen += chunk.len() as u64;
        let now = seen * 1000 / rate as u64;
        let events = vad.push(chunk);
        sim.deliver(now);
        for ev in events {
            let input = match ev {
                VadEvent::SpeechStart { start_ms } => Input::SpeechStart { at_ms: start_ms },
                VadEvent::Segment {
                    start_ms, end_ms, ..
                } => {
                    let names: Vec<String> = truth
                        .iter()
                        .enumerate()
                        .filter(|(_, (s, e))| *s < end_ms && *e > start_ms)
                        .map(|(i, _)| format!("u{}", i + 1))
                        .collect();
                    let delay = case.stt_delay_ms.get(seg_idx).copied().unwrap_or(0);
                    seg_idx += 1;
                    sim.due.push((
                        now + delay,
                        Input::Transcript {
                            start_ms,
                            text: Ok(names.join(" ")),
                        },
                    ));
                    Input::Segment { start_ms, end_ms }
                }
                VadEvent::Discarded { .. } => Input::Discarded,
                VadEvent::Metrics { .. } => continue,
            };
            sim.feed(input, now);
        }
        sim.feed(Input::Tick { now_ms: now }, now);
    }
    sim.feed(Input::Flush, seen * 1000 / rate as u64);
    sim.deliver(u64::MAX);
    sim.out
}

// Truth: u1 +0.5s u2 (one VAD segment), +1.5s u3, +3s u4.
const SPLIT: &[&str] = &[
    r#"ask "u1 u2 u3" h=0"#,
    "answered",
    r#"ask "u4" h=2"#,
    "answered",
];

#[test]
fn fixture_turns() {
    let cases = [
        Case {
            name: "split question",
            stt_delay_ms: &[],
            reply_delay_ms: 0,
            replies: &["A", "B"],
            want: SPLIT,
        },
        Case {
            name: "SKIP carries",
            stt_delay_ms: &[],
            reply_delay_ms: 0,
            replies: &["SKIP", "B"],
            want: &[
                r#"ask "u1 u2 u3" h=0"#,
                "skipped carried",
                r#"ask "u1 u2 u3 u4" h=0"#,
                "answered",
            ],
        },
        Case {
            name: "out-of-order STT",
            stt_delay_ms: &[5000],
            reply_delay_ms: 0,
            replies: &["A", "B"],
            want: SPLIT,
        },
        Case {
            name: "answer in flight",
            stt_delay_ms: &[],
            reply_delay_ms: 10000,
            replies: &["A", "B"],
            want: SPLIT,
        },
        Case {
            name: "COPY",
            stt_delay_ms: &[],
            reply_delay_ms: 0,
            replies: &[" COPY\n", "B"],
            want: &[
                r#"ask "u1 u2 u3" h=0"#,
                "skipped copy",
                r#"ask "u4" h=0"#,
                "answered",
            ],
        },
    ];
    let mut failures = Vec::new();
    for case in &cases {
        for rate in RATES {
            let got = run(case, rate);
            if got != case.want {
                failures.push(format!(
                    "{} @{rate}:\n  got  {got:?}\n  want {:?}",
                    case.name, case.want
                ));
            }
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

fn check(inputs: Vec<Input>, want: &[&str]) {
    let mut turns = Turns::new(Vec::new(), String::new());
    let mut got = Vec::new();
    for input in inputs {
        trace(turns.push(input), &mut got, || {});
    }
    assert_eq!(got, want);
}

fn seg(start_ms: u64, end_ms: u64) -> Input {
    Input::Segment { start_ms, end_ms }
}

fn heard(start_ms: u64, text: &str) -> Input {
    Input::Transcript {
        start_ms,
        text: Ok(text.into()),
    }
}

fn reply(text: &str) -> Input {
    Input::Reply(Ok(text.into()))
}

#[test]
fn scripted() {
    check(
        vec![
            seg(0, 500),
            seg(900, 1500),
            heard(0, ""),
            heard(900, " "),
            Input::Flush,
        ],
        &["nospeech"],
    );
    check(
        vec![
            seg(0, 500),
            seg(900, 1500),
            heard(0, "how do"),
            Input::Transcript {
                start_ms: 900,
                text: Err("timeout".into()),
            },
            Input::Flush,
        ],
        &["failed timeout"],
    );
    check(
        vec![
            seg(0, 500),
            heard(0, "q"),
            Input::Flush,
            Input::Prompt("summarize".into()),
            reply("SKIP"),
        ],
        &[
            r#"ask "q" h=0"#,
            "skipped carried",
            r#"ask "q\n\nsummarize" h=0"#,
        ],
    );
    check(
        vec![seg(0, 500), heard(0, "q"), Input::Flush, reply("  ")],
        &[r#"ask "q" h=0"#, "failed model returned an empty answer"],
    );
}

/// Next speech at `gap_ms` after the previous segment's end, with ticks every `tick_ms`.
fn gap(gap_ms: u64, tick_ms: u64) -> Vec<String> {
    let next = 1000 + gap_ms;
    let mut inputs = vec![
        Input::SpeechStart { at_ms: 100 },
        seg(100, 1000),
        heard(100, "a"),
    ];
    inputs.extend((1..=next / tick_ms).map(|k| Input::Tick {
        now_ms: k * tick_ms,
    }));
    inputs.extend([
        Input::SpeechStart { at_ms: next },
        seg(next, next + 500),
        heard(next, "b"),
        Input::Flush,
    ]);
    let mut turns = Turns::new(Vec::new(), String::new());
    let mut got = Vec::new();
    for input in inputs {
        trace(turns.push(input), &mut got, || {});
    }
    loop {
        let open = got.iter().filter(|l| l.starts_with("ask")).count()
            - got.iter().filter(|l| *l == "answered").count();
        if open == 0 {
            return got;
        }
        trace(turns.push(reply("A")), &mut got, || {});
    }
}

#[test]
fn turn_gap_boundary() {
    let gap_ms = 2000; // TURN_GAP_MS
    for tick_ms in [20, 256, 1000] {
        let merged = gap(gap_ms - 100, tick_ms);
        assert_eq!(
            merged,
            [r#"ask "a b" h=0"#, "answered"],
            "gap-100 tick {tick_ms}"
        );
        let split = gap(gap_ms + 100, tick_ms);
        assert_eq!(
            split,
            [r#"ask "a" h=0"#, "answered", r#"ask "b" h=2"#, "answered"],
            "gap+100 tick {tick_ms}"
        );
    }
}
