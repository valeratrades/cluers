use reqwest::Client;
use std::collections::hash_map::Entry;
use std::{collections::HashMap, sync::Mutex};
use tokio::sync::oneshot;

use crate::llm::secrets::Secrets;
use crate::llm::LlmError;

/// Per-app LLM state. Owns the shared `reqwest::Client` (one connection
/// pool for the whole process), the custom-provider secret write-through
/// cache, and the registry of in-flight streams so `cancel_chat` can
/// interrupt them.
pub struct LlmState {
    pub http: Client,
    pub secrets: Secrets,
    pub(crate) cancels: Cancels,
}

impl LlmState {
    pub fn new() -> Self {
        Self {
            http: Client::new(),
            secrets: Secrets::new(),
            cancels: Cancels::default(),
        }
    }
}

#[derive(Default)]
pub(crate) struct Cancels(Mutex<HashMap<String, Option<oneshot::Sender<()>>>>); // None = cancelled, id still owned by a live stream

/// Owns its `request_id` in the registry until dropped.
pub(crate) struct Registration<'a> {
    cancels: &'a Cancels,
    id: String,
    pub rx: oneshot::Receiver<()>,
}

impl Cancels {
    pub fn register(&self, id: String) -> Result<Registration<'_>, LlmError> {
        let mut map = self.0.lock().expect("LlmState::cancels mutex poisoned");
        match map.entry(id) {
            Entry::Occupied(e) => Err(LlmError::DuplicateRequestId(e.key().clone())),
            Entry::Vacant(e) => {
                let (tx, rx) = oneshot::channel();
                let id = e.key().clone();
                e.insert(Some(tx));
                Ok(Registration { cancels: self, id, rx })
            }
        }
    }

    pub fn cancel(&self, id: &str) {
        let mut map = self.0.lock().expect("LlmState::cancels mutex poisoned");
        if let Some(tx) = map.get_mut(id).and_then(Option::take) {
            tx.send(()).expect("receiver outlives its registry entry");
        }
    }
}

impl Drop for Registration<'_> {
    fn drop(&mut self) {
        self.cancels
            .0
            .lock()
            .expect("LlmState::cancels mutex poisoned")
            .remove(&self.id)
            .expect("registration owns its entry");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(ops: &str, expect: &str) {
        let cancels = Cancels::default();
        let mut live: HashMap<&str, Registration> = HashMap::new();
        let trace: Vec<String> = ops
            .split(';')
            .map(|op| {
                let (cmd, id) = op.trim().split_once(' ').expect("op is `<cmd> <id>`");
                match cmd {
                    "reg" => match cancels.register(id.to_string()) {
                        Ok(r) => {
                            assert!(live.insert(id, r).is_none(), "script registers {id} twice");
                            "ok".into()
                        }
                        Err(LlmError::DuplicateRequestId(_)) => "err duplicate".into(),
                        Err(e) => panic!("unexpected {e}"),
                    },
                    "drop" => {
                        live.remove(id).expect("script drops a live registration");
                        "-".into()
                    }
                    "cancel" => {
                        cancels.cancel(id);
                        "-".into()
                    }
                    "poll" => {
                        let r = live.get_mut(id).expect("script polls a live registration");
                        match r.rx.try_recv() {
                            Ok(()) => "fired".into(),
                            Err(oneshot::error::TryRecvError::Empty) => "idle".into(),
                            Err(e) => panic!("unexpected {e}"),
                        }
                    }
                    _ => panic!("unknown op {cmd}"),
                }
            })
            .collect();
        assert_eq!(trace.join("; "), expect, "ops: {ops}");
    }

    #[test]
    fn duplicate_keeps_first_cancellable() {
        check("reg a; reg a; cancel a; poll a", "ok; err duplicate; -; fired");
    }

    #[test]
    fn cancel_after_complete_is_noop() {
        check("reg a; drop a; cancel a; reg a; poll a", "ok; -; -; ok; idle");
    }

    #[test]
    fn cancelled_id_reserved_until_stream_exits() {
        check(
            "reg a; cancel a; reg a; drop a; reg a; poll a",
            "ok; -; err duplicate; -; ok; idle",
        );
    }

    #[test]
    fn unknown_and_double_cancel() {
        check("cancel x; reg a; cancel a; cancel a", "-; ok; -; -");
    }
}
