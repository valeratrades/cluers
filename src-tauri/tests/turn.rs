//! Spec for the turn machine. Audio cases run the `pauses` VAD fixture through the real segmenter.
use pluely_lib::turn::{Input, Output, Speaker, TurnEvent, Turns};
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
    last_interviewer_end: u64,
    ask_lag: Vec<u64>,
}

impl<'a> Sim<'a> {
    fn new(replies: &[&'a str], reply_delay_ms: u64) -> Self {
        Self {
            turns: Turns::new(Vec::new(), String::new(), &VadConfig::default()),
            replies: replies.iter().copied().collect(),
            reply_delay_ms,
            due: Vec::new(),
            out: Vec::new(),
            last_interviewer_end: 0,
            ask_lag: Vec::new(),
        }
    }

    fn feed(&mut self, input: Input, now: u64) {
        let mut asked = 0;
        trace(self.turns.push(input), &mut self.out, || asked += 1);
        for _ in 0..asked {
            self.ask_lag.push(now - self.last_interviewer_end);
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
    let mut sim = Sim::new(case.replies, case.reply_delay_ms);
    let (mut seen, mut seg_idx) = (0u64, 0usize);

    for chunk in samples.chunks(4096) {
        seen += chunk.len() as u64;
        let now = seen * 1000 / rate as u64;
        let events = vad.push(chunk);
        sim.deliver(now);
        for ev in events {
            let input = match ev {
                VadEvent::SpeechStart { start_ms } => start(start_ms),
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
                    seg(start_ms, end_ms)
                }
                VadEvent::Discarded { .. } => Input::Discarded {
                    speaker: Speaker::Interviewer,
                },
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

/// Two channels assembled from `pauses` clips: `"u1"`..`"u4"` are its utterances, `"mm"` the first 500ms of u4.
struct Scene {
    name: &'static str,
    interviewer: &'static [(&'static str, u64)], // (clip, at_ms)
    user: &'static [(&'static str, u64)],
    bleed: Option<(u64, f32)>, // (delay_ms, gain): mic += gain * system delayed
    want: &'static [&'static str],
}

/// Segments both channels in lockstep like the capture driver; ask lines carry `+{lag}s` since the last interviewer speech end.
fn run_scene(scene: &Scene, rate: u32) -> Vec<String> {
    let truth = truth("pauses");
    let src = load("pauses", rate);
    let span = |clip: &str| match clip {
        "mm" => (truth[3].0, truth[3].0 + 500),
        u => truth[u[1..].parse::<usize>().unwrap() - 1],
    };
    let at = |ms: u64| (ms * rate as u64 / 1000) as usize;
    let placed = |clips: &[(&'static str, u64)]| -> Vec<(&'static str, u64, u64)> {
        clips
            .iter()
            .map(|&(c, t)| {
                let (s, e) = span(c);
                (c, t, t + e - s)
            })
            .collect()
    };
    let (interviewer, user) = (placed(scene.interviewer), placed(scene.user));
    let len_ms = interviewer.iter().chain(&user).map(|p| p.2).max().unwrap() + 3000;
    let mut sys = vec![0.0f32; at(len_ms)];
    let mut mic = sys.clone();
    for (chan, clips) in [(&mut sys, scene.interviewer), (&mut mic, scene.user)] {
        for &(c, t) in clips {
            let (s, e) = span(c);
            for (d, x) in chan[at(t)..].iter_mut().zip(&src[at(s)..at(e)]) {
                *d += x;
            }
        }
    }
    if let Some((delay, gain)) = scene.bleed {
        let d = at(delay);
        for i in 0..sys.len() - d {
            mic[i + d] += gain * sys[i];
        }
    }

    let cfg = VadConfig::default();
    let (mut sys_vad, mut mic_vad) = (
        Segmenter::new(&cfg, rate).unwrap(),
        Segmenter::new(&cfg, rate).unwrap(),
    );
    let mut sim = Sim::new(&["A", "A"], 0); // a second ask shows up in the trace instead of panicking
    let (mut seen, mut mic_heard) = (0u64, false);
    for (s, m) in sys.chunks(4096).zip(mic.chunks(4096)) {
        seen += s.len() as u64;
        let now = seen * 1000 / rate as u64;
        let events: Vec<_> = sys_vad
            .push(s)
            .into_iter()
            .map(|e| (Speaker::Interviewer, e))
            .chain(mic_vad.push(m).into_iter().map(|e| (Speaker::User, e)))
            .collect();
        sim.deliver(now);
        for (speaker, ev) in events {
            let input = match ev {
                VadEvent::SpeechStart { start_ms } => {
                    mic_heard |= speaker == Speaker::User;
                    Input::SpeechStart {
                        speaker,
                        at_ms: start_ms,
                    }
                }
                VadEvent::Segment {
                    start_ms, end_ms, ..
                } => {
                    if speaker == Speaker::Interviewer {
                        sim.last_interviewer_end = end_ms;
                        let names: Vec<&str> = interviewer
                            .iter()
                            .filter(|(_, s, e)| *s < end_ms && *e > start_ms)
                            .map(|p| p.0)
                            .collect();
                        sim.due.push((
                            now,
                            Input::Transcript {
                                start_ms,
                                text: Ok(names.join(" ")),
                            },
                        ));
                    }
                    Input::Segment {
                        speaker,
                        start_ms,
                        end_ms,
                    }
                }
                VadEvent::Discarded { .. } => Input::Discarded { speaker },
                VadEvent::Metrics { .. } => continue,
            };
            sim.feed(input, now);
        }
        sim.feed(Input::Tick { now_ms: now }, now);
    }
    sim.feed(Input::Flush, seen * 1000 / rate as u64);
    sim.deliver(u64::MAX);

    let mut lags = sim.ask_lag.iter();
    let mut out: Vec<String> = sim
        .out
        .into_iter()
        .map(|l| match l.starts_with("ask") {
            true => format!("{l} +{}s", lags.next().expect("one lag per ask") / 1000),
            false => l,
        })
        .collect();
    if (!scene.user.is_empty() || scene.bleed.is_some()) && !mic_heard {
        out.push("mic segmenter never heard speech".into());
    }
    out
}

#[test]
fn scenes() {
    const Q: &[(&str, u64)] = &[("u1", 1000), ("u2", 4124)]; // one segment ending ~6980
    const Q3: &[(&str, u64)] = &[("u1", 1000), ("u2", 4124), ("u3", 8480)];
    const INTERVIEWER_ONLY: &[&str] = &[r#"ask "u1 u2 u3" h=0 +2s"#, "answered"];
    const USER_TOOK: &[&str] = &[r#"ask "u1 u2" h=0 +1s"#, "answered"];
    let scenes = [
        Scene {
            name: "interviewer only",
            interviewer: Q3,
            user: &[],
            bleed: None,
            want: INTERVIEWER_ONLY,
        },
        Scene {
            name: "user answers",
            interviewer: Q,
            user: &[("u4", 7580)],
            bleed: None,
            want: USER_TOOK,
        },
        Scene {
            name: "user only",
            interviewer: &[],
            user: &[("u4", 1000)],
            bleed: None,
            want: &[],
        },
        Scene {
            name: "user backchannel",
            interviewer: Q3,
            user: &[("mm", 2000)],
            bleed: None,
            want: INTERVIEWER_ONLY,
        },
        Scene {
            name: "overlap",
            interviewer: Q,
            user: &[("u4", 6000)],
            bleed: None,
            want: USER_TOOK,
        },
        Scene {
            name: "echo 50ms",
            interviewer: Q3,
            user: &[],
            bleed: Some((50, 0.3)),
            want: INTERVIEWER_ONLY,
        },
        Scene {
            name: "echo 300ms (Bluetooth)",
            interviewer: Q3,
            user: &[],
            bleed: Some((300, 0.3)),
            want: INTERVIEWER_ONLY,
        },
        Scene {
            name: "echo + reply",
            interviewer: Q,
            user: &[("u4", 7580)],
            bleed: Some((150, 0.3)),
            want: USER_TOOK,
        },
        Scene {
            name: "interviewer mm-hm while user answers",
            interviewer: &[("u1", 1000), ("u2", 4124), ("mm", 9000)],
            user: &[("u4", 7580)],
            bleed: None,
            want: USER_TOOK,
        },
    ];
    let mut failures = Vec::new();
    for scene in &scenes {
        for rate in RATES {
            let got = run_scene(scene, rate);
            if got != scene.want {
                failures.push(format!(
                    "{} @{rate}:\n  got  {got:?}\n  want {:?}",
                    scene.name, scene.want
                ));
            }
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

fn check(inputs: Vec<Input>, want: &[&str]) {
    let mut turns = Turns::new(Vec::new(), String::new(), &VadConfig::default());
    let mut got = Vec::new();
    for input in inputs {
        trace(turns.push(input), &mut got, || {});
    }
    assert_eq!(got, want);
}

fn start(at_ms: u64) -> Input {
    Input::SpeechStart {
        speaker: Speaker::Interviewer,
        at_ms,
    }
}

fn seg(start_ms: u64, end_ms: u64) -> Input {
    Input::Segment {
        speaker: Speaker::Interviewer,
        start_ms,
        end_ms,
    }
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

/// Replies "A" to every ask.
fn play(inputs: Vec<Input>) -> Vec<String> {
    let mut turns = Turns::new(Vec::new(), String::new(), &VadConfig::default());
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

/// Next speech at `gap_ms` after the previous segment's end, with ticks every `tick_ms`.
fn gap(gap_ms: u64, tick_ms: u64) -> Vec<String> {
    let next = 1000 + gap_ms;
    let mut inputs = vec![start(100), seg(100, 1000), heard(100, "a")];
    inputs.extend((1..=next / tick_ms).map(|k| Input::Tick {
        now_ms: k * tick_ms,
    }));
    inputs.extend([
        start(next),
        seg(next, next + 500),
        heard(next, "b"),
        Input::Flush,
    ]);
    play(inputs)
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

#[test]
fn attribution_boundaries() {
    const BLEED_TAIL_MS: u64 = 400;
    const BACKCHANNEL_MS: u64 = 1000;
    let user_start = |at_ms| Input::SpeechStart {
        speaker: Speaker::User,
        at_ms,
    };
    let merged = [r#"ask "a b" h=0"#, "answered"];
    let split = [r#"ask "a" h=0"#, "answered", r#"ask "b" h=2"#, "answered"];
    let asked_a = [r#"ask "a" h=0"#, "answered"];

    let t = 1000 + BLEED_TAIL_MS;
    let echo_tail = |run_start: u64| {
        play(vec![
            start(100),
            seg(100, 1000),
            heard(100, "a"),
            user_start(run_start),
            Input::Segment {
                speaker: Speaker::User,
                start_ms: run_start,
                end_ms: run_start + 300,
            },
            start(2500),
            seg(2500, 3000),
            heard(2500, "b"),
            Input::Flush,
        ])
    };
    assert_eq!(
        echo_tail(t - 400),
        merged,
        "run ending before the echo tail"
    );
    assert_eq!(echo_tail(t + 100), split, "run after the echo tail");

    let backchannel = |len: u64| {
        play(vec![
            start(100),
            seg(100, 1000),
            heard(100, "a"),
            user_start(1500),
            start(2000),
            seg(2000, 2000 + len),
            heard(2000, "b"),
            Input::Flush,
        ])
    };
    assert_eq!(
        backchannel(BACKCHANNEL_MS - 100),
        asked_a,
        "short interviewer segment"
    );
    assert_eq!(
        backchannel(BACKCHANNEL_MS + 100),
        split,
        "long interviewer segment"
    );
}
