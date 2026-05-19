//! LLM streaming + secret storage subsystem.
//!
//! - `stream_chat` (in `commands`) is the single command surface the
//!   frontend uses to drive both the Pluely-hosted path and arbitrary
//!   custom provider templates. Streaming is over a Tauri `Channel<T>`.
//! - Cancellation is `tokio::select!` against an `oneshot::Receiver`
//!   whose `Sender` lives in `LlmState` keyed by `request_id`.
//! - All provider secrets and Pluely credentials live in the OS keychain
//!   via `secrets.rs`. The plaintext `secure_storage.json` and the
//!   `localStorage.variables` plaintext dicts are migrated once at
//!   startup, then deleted.

pub mod commands;
pub mod pluely;
pub mod provider;
pub mod secrets;
pub mod state;
pub mod stream;

pub use state::LlmState;

use serde::Serialize;

/// Event payload sent over the per-request `Channel<StreamEvent>`.
/// One stream lifetime: zero or more `Chunk`s, terminated by exactly
/// one of `Done` or `Error`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum StreamEvent {
    Chunk {
        delta: String,
    },
    Done {
        full_response: String,
        request_id: String,
    },
    Error {
        message: String,
        request_id: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error(transparent)]
    Reqwest(#[from] reqwest::Error),
    #[error("keychain: {0}")]
    Keychain(String),
    #[error("missing variable: {0}")]
    MissingVariable(String),
    #[error("invalid curl: {0}")]
    InvalidCurl(&'static str),
    #[error("pluely unlicensed")]
    PluelyUnlicensed,
    #[error("pluely config: {0}")]
    PluelyConfig(String),
    #[error("provider api {status}: {body}")]
    ProviderApi { status: u16, body: String },
    #[error("curl parse: {0}")]
    CurlParse(String),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("channel: {0}")]
    Channel(String),
    #[error("cancelled")]
    Cancelled,
}

impl serde::Serialize for LlmError {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}
