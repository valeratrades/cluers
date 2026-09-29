//! Pure turn machine: segments, transcripts and replies in; LLM asks and UI events out. No IO, no clocks.
//! Spec: `tests/turn.rs`.
use super::vad::{VadConfig, HOP_MS};
use crate::db::schema::Role;
use crate::llm::provider::HistoryMessage;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

pub(crate) const SKIP_WORDS: [&str; 2] = ["SKIP", "COPY"];
pub(crate) const SKIP_INSTRUCTION: &str = "You are receiving live transcribed speech from system audio, which may \
arrive in incomplete chunks. If the current input is clearly only a partial fragment (cut off mid-sentence, \
missing the actual question, or otherwise insufficient to give a meaningful answer), reply with exactly the \
single word SKIP (uppercase, no punctuation, no other text); the fragment will be prepended to the next chunk. \
If the input is complete and understood but calls for no reply (e.g. a statement not addressed to you, or one \
that needs no answer), reply with exactly the single word COPY. Otherwise respond normally.";
const TURN_GAP_MS: u64 = 2000; // > natural mid-question pauses (~1.5s); effectively max(this, silence_ms). Closes the turn when the user stays silent
const BLEED_TAIL_MS: u64 = 400; // playback latency (Bluetooth ~300ms) can put echo after short interviewer speech
const BACKCHANNEL_MS: u64 = 1000; // "mm-hm", "right"

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speaker {
    Interviewer,
    User,
}

pub enum Input {
    SpeechStart {
        speaker: Speaker,
        at_ms: u64,
    },
    Segment {
        speaker: Speaker,
        start_ms: u64, // segment id for Interviewer
        end_ms: u64,
    },
    Discarded {
        speaker: Speaker,
    },
    Transcript {
        start_ms: u64,
        text: Result<String, String>,
    },
    Tick {
        now_ms: u64, // audio clock
    },
    Flush, // close the open turn now
    Prompt(String),
    Reply(Result<String, String>), // outcome of the in-flight Ask
}

pub enum Output {
    Ask {
        message: String,
        history: Vec<HistoryMessage>,
    },
    Event(TurnEvent),
}

/// IPC contract with `useSystemAudio.ts`; `Delta` is produced by the driver.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum TurnEvent {
    Heard { text: String }, // carry + every unanswered transcript, chronological
    Asked { message: String },
    Delta { delta: String },
    Answered { message: String, answer: String },
    Skipped { message: String, carried: bool },
    NoSpeech,
    Failed { error: String },
}

type Segments = BTreeMap<u64, Option<Result<String, String>>>;

enum Pending {
    Speech(Segments),
    Prompt(String),
}

/// Latest mic run; runs are sequential and the newest one decides.
struct UserRun {
    start_ms: u64,
    end_ms: Option<u64>,
    took_turn: Option<bool>,
}

pub struct Turns {
    history: Vec<HistoryMessage>,
    carry: String,
    speaking: bool,
    last_end_ms: u64,
    open: Segments,
    closed: VecDeque<Pending>,
    asking: Option<String>,
    user: Option<UserRun>,
    backchannels: BTreeSet<u64>, // dropped interviewer segment ids
    now_ms: u64,
    mic_hold_ms: u64,
}

impl Turns {
    pub fn new(history: Vec<HistoryMessage>, carry: String, vad: &VadConfig) -> Self {
        Self {
            history,
            carry,
            speaking: false,
            last_end_ms: 0,
            open: Segments::new(),
            closed: VecDeque::new(),
            asking: None,
            user: None,
            backchannels: BTreeSet::new(),
            now_ms: 0,
            mic_hold_ms: mic_hold_ms(vad),
        }
    }

    pub fn reconfigure(&mut self, vad: &VadConfig) {
        self.mic_hold_ms = mic_hold_ms(vad);
    }

    pub fn push(&mut self, input: Input) -> Vec<Output> {
        let mut out = Vec::new();
        match input {
            Input::SpeechStart {
                speaker: Speaker::User,
                at_ms,
            } => {
                self.user = Some(UserRun {
                    start_ms: at_ms,
                    end_ms: None,
                    took_turn: None,
                })
            }
            Input::Segment {
                speaker: Speaker::User,
                end_ms,
                ..
            } => {
                self.user
                    .as_mut()
                    .expect("mic segment without speech start")
                    .end_ms = Some(end_ms)
            }
            Input::Discarded {
                speaker: Speaker::User,
            } => self.user = None, // shorter than min_speech_ms: noise
            Input::SpeechStart {
                speaker: Speaker::Interviewer,
                at_ms,
            } => {
                self.close_if_due(at_ms);
                self.speaking = true;
            }
            Input::Segment {
                speaker: Speaker::Interviewer,
                start_ms,
                end_ms,
            } => {
                self.speaking = false;
                self.last_end_ms = end_ms;
                let fresh = match end_ms - start_ms < BACKCHANNEL_MS
                    && self.user_holds_floor_at(start_ms)
                {
                    true => self.backchannels.insert(start_ms),
                    false => self.open.insert(start_ms, None).is_none(),
                };
                assert!(fresh, "segment {start_ms} pushed twice");
            }
            Input::Discarded {
                speaker: Speaker::Interviewer,
            } => self.speaking = false,
            Input::Transcript { start_ms, .. } if self.backchannels.contains(&start_ms) => {
                self.backchannels.remove(&start_ms);
            }
            Input::Transcript { start_ms, text } => {
                let heard = matches!(&text, Ok(t) if !t.trim().is_empty());
                let slot = std::iter::once(&mut self.open)
                    .chain(self.closed.iter_mut().filter_map(|p| match p {
                        Pending::Speech(s) => Some(s),
                        Pending::Prompt(_) => None,
                    }))
                    .find_map(|s| s.get_mut(&start_ms))
                    .filter(|slot| slot.is_none())
                    .expect("transcript for unknown segment");
                *slot = Some(text);
                if heard {
                    out.push(Output::Event(TurnEvent::Heard {
                        text: self.unanswered(),
                    }));
                }
            }
            Input::Tick { now_ms } => {
                self.now_ms = now_ms;
                self.close_if_due(now_ms)
            }
            Input::Flush => self.close(),
            Input::Prompt(text) => self.closed.push_back(Pending::Prompt(text)),
            Input::Reply(reply) => {
                let message = self.asking.take().expect("reply without an ask in flight");
                assert!(self.carry.is_empty(), "carry is consumed by every ask");
                out.push(Output::Event(match reply {
                    Err(error) => TurnEvent::Failed { error },
                    Ok(answer) => match answer.trim() {
                        "SKIP" => {
                            self.carry = message.clone();
                            TurnEvent::Skipped {
                                message,
                                carried: true,
                            }
                        }
                        "COPY" => TurnEvent::Skipped {
                            message,
                            carried: false,
                        },
                        "" => TurnEvent::Failed {
                            error: "model returned an empty answer".into(),
                        },
                        _ => {
                            self.history.push(HistoryMessage {
                                role: Role::User,
                                content: message.clone(),
                            });
                            self.history.push(HistoryMessage {
                                role: Role::Assistant,
                                content: answer.clone(),
                            });
                            TurnEvent::Answered { message, answer }
                        }
                    },
                }));
            }
        }
        self.resolve_user();
        self.pump(&mut out);
        out
    }

    // ponytail: echo judged by timing against the interviewer's VAD; the user barging in mid-question falls back to TURN_GAP_MS. Needs real AEC to detect.
    fn resolve_user(&mut self) {
        if self.speaking {
            return;
        }
        let Some(run) = self.user.as_mut().filter(|r| r.took_turn.is_none()) else {
            return;
        };
        let t = self.last_end_ms + BLEED_TAIL_MS;
        let took = match run.end_ms {
            _ if run.start_ms > t => true,
            Some(end) => end > t,
            None if self.now_ms >= t + self.mic_hold_ms => true,
            None => return,
        };
        run.took_turn = Some(took);
        if took {
            self.close();
        }
    }

    fn user_holds_floor_at(&self, t: u64) -> bool {
        self.user.as_ref().is_some_and(|r| {
            r.took_turn == Some(true) && r.start_ms <= t && r.end_ms.is_none_or(|e| e >= t)
        })
    }

    fn close_if_due(&mut self, t: u64) {
        if !self.speaking && t >= self.last_end_ms + TURN_GAP_MS {
            self.close();
        }
    }

    fn close(&mut self) {
        if !self.open.is_empty() {
            self.closed
                .push_back(Pending::Speech(std::mem::take(&mut self.open)));
        }
    }

    fn unanswered(&self) -> String {
        let speech = self
            .closed
            .iter()
            .filter_map(|p| match p {
                Pending::Speech(s) => Some(s),
                Pending::Prompt(_) => None,
            })
            .chain(std::iter::once(&self.open))
            .flat_map(|s| s.values())
            .filter_map(|t| match t {
                Some(Ok(t)) if !t.trim().is_empty() => Some(t.trim()),
                _ => None,
            });
        std::iter::once(self.carry.as_str())
            .filter(|c| !c.is_empty())
            .chain(speech)
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn pump(&mut self, out: &mut Vec<Output>) {
        while self.asking.is_none() {
            let (text, sep) = match self.closed.front() {
                None => return,
                Some(Pending::Speech(s)) if s.values().any(Option::is_none) => return,
                Some(_) => match self.closed.pop_front().expect("front checked above") {
                    Pending::Prompt(p) => (p, "\n\n"),
                    Pending::Speech(s) => {
                        let (ok, errs): (Vec<_>, Vec<_>) = s
                            .into_values()
                            .map(|t| t.expect("readiness checked above"))
                            .partition(Result::is_ok);
                        if !errs.is_empty() {
                            let error = errs
                                .into_iter()
                                .map(|e| e.unwrap_err())
                                .collect::<Vec<_>>()
                                .join("; ");
                            out.push(Output::Event(TurnEvent::Failed { error }));
                            continue;
                        }
                        let text = ok
                            .into_iter()
                            .map(Result::unwrap)
                            .filter(|t| !t.trim().is_empty())
                            .map(|t| t.trim().to_owned())
                            .collect::<Vec<_>>()
                            .join(" ");
                        if text.is_empty() {
                            out.push(Output::Event(TurnEvent::NoSpeech));
                            continue;
                        }
                        (text, " ")
                    }
                },
            };
            let message = match std::mem::take(&mut self.carry) {
                c if c.is_empty() => text,
                c => format!("{c}{sep}{text}"),
            };
            self.asking = Some(message.clone());
            out.push(Output::Ask {
                message: message.clone(),
                history: self.history.clone(),
            });
            out.push(Output::Event(TurnEvent::Asked { message }));
        }
    }
}

/// A mic run with no speech after t has ended by t + this.
fn mic_hold_ms(vad: &VadConfig) -> u64 {
    let hop = HOP_MS as u64;
    (vad.silence_ms as u64).div_ceil(hop) * hop + hop
}
