//! Pure turn machine: VAD segments + transcripts + replies in, LLM asks and UI events out. No IO, no clocks.
use crate::llm::provider::HistoryMessage;
use serde::Serialize;

pub(crate) const SKIP_WORDS: [&str; 2] = ["SKIP", "COPY"];

pub enum Input {
    SpeechStart { at_ms: u64 },
    Segment { start_ms: u64, end_ms: u64 },
    Discarded,
    Transcript { start_ms: u64, text: Result<String, String> },
    Tick { now_ms: u64 },
    Flush,
    Prompt(String),
    Reply(Result<String, String>),
}

pub enum Output {
    Ask { message: String, history: Vec<HistoryMessage> },
    Event(TurnEvent),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum TurnEvent {
    Heard { text: String },
    Asked { message: String },
    Delta { delta: String },
    Answered { message: String, answer: String },
    Skipped { message: String, carried: bool },
    NoSpeech,
    Failed { error: String },
}

pub struct Turns {
    history: Vec<HistoryMessage>,
    asking: Option<String>,
}

impl Turns {
    pub fn new(history: Vec<HistoryMessage>, _carry: String) -> Self {
        Self { history, asking: None }
    }

    pub fn push(&mut self, input: Input) -> Vec<Output> {
        match input {
            Input::Transcript { text: Ok(t), .. } if !t.is_empty() => {
                self.asking = Some(t.clone());
                vec![Output::Ask { message: t, history: self.history.clone() }]
            }
            Input::Reply(Ok(r)) => {
                let message = self.asking.take().expect("reply without ask");
                if SKIP_WORDS.contains(&r.trim()) {
                    return vec![Output::Event(TurnEvent::Skipped { message, carried: false })];
                }
                vec![Output::Event(TurnEvent::Answered { message, answer: r })]
            }
            _ => Vec::new(),
        }
    }
}
