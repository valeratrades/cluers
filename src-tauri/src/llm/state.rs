use reqwest::Client;
use std::{collections::HashMap, sync::Mutex};
use tokio::sync::oneshot;

/// Per-app LLM state. Owns the shared `reqwest::Client` (one connection
/// pool for the whole process) and a registry of in-flight stream cancel
/// senders keyed by `request_id` so `cancel_chat` can interrupt them.
pub struct LlmState {
    pub http: Client,
    pub cancels: Mutex<HashMap<String, oneshot::Sender<()>>>,
}

impl LlmState {
    pub fn new() -> Self {
        Self {
            http: Client::new(),
            cancels: Mutex::new(HashMap::new()),
        }
    }
}
