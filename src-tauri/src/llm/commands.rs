//! Tauri command surface for the LLM subsystem.
//!
//! `stream_chat` is the sole streaming entrypoint; it routes between
//! `pluely::stream_pluely` and `provider::stream_custom` internally based
//! on `request.provider.is_pluely_hosted`. Cancellation is per-request
//! via the `cancel_chat` command and a `oneshot::Sender` stored in
//! `LlmState`.

use crate::db::schema::AttachedFile;
use crate::llm::{pluely, provider, secrets, LlmError, LlmState, StreamEvent};
use serde::Deserialize;
use std::collections::HashMap;
use tauri::ipc::Channel;
use tauri::{AppHandle, State};
use tokio::sync::oneshot;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamChatRequest {
    pub provider: ProviderInput,
    pub message: String,
    #[serde(default)]
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub history: Vec<provider::HistoryMessage>,
    #[serde(default)]
    pub attached_files: Vec<AttachedFile>,
    pub request_id: String,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ProviderInput {
    pub id: String,
    #[serde(default)]
    pub curl: String,
    #[serde(default)]
    pub response_content_path: String,
    #[serde(default)]
    pub streaming: bool,
    #[serde(default)]
    pub is_pluely_hosted: bool,
    #[serde(default)]
    pub user_variables: HashMap<String, String>,
}

#[tauri::command]
pub async fn stream_chat(
    app: AppHandle,
    state: State<'_, LlmState>,
    request: StreamChatRequest,
    channel: Channel<StreamEvent>,
) -> Result<String, String> {
    let request_id = request.request_id.clone();
    let (cancel_tx, mut cancel_rx) = oneshot::channel::<()>();
    {
        let mut cancels = state
            .cancels
            .lock()
            .expect("LlmState::cancels mutex poisoned");
        cancels.insert(request_id.clone(), cancel_tx);
    }

    let http = state.http.clone();
    let is_pluely = request.provider.is_pluely_hosted;
    let result = if is_pluely {
        pluely::stream_pluely(&app, &http, request, &channel, &mut cancel_rx).await
    } else {
        provider::stream_custom(&http, request, &channel, &mut cancel_rx).await
    };

    {
        let mut cancels = state
            .cancels
            .lock()
            .expect("LlmState::cancels mutex poisoned");
        cancels.remove(&request_id);
    }

    match result {
        Ok(full) => {
            channel
                .send(StreamEvent::Done {
                    full_response: full,
                    request_id: request_id.clone(),
                })
                .map_err(|e| e.to_string())?;
            Ok(request_id)
        }
        Err(LlmError::Cancelled) => {
            channel
                .send(StreamEvent::Done {
                    full_response: String::new(),
                    request_id: request_id.clone(),
                })
                .map_err(|e| e.to_string())?;
            Ok(request_id)
        }
        Err(e) => {
            let msg = e.to_string();
            channel
                .send(StreamEvent::Error {
                    message: msg.clone(),
                    request_id,
                })
                .map_err(|err| err.to_string())?;
            Err(msg)
        }
    }
}

#[tauri::command]
pub fn cancel_chat(state: State<'_, LlmState>, request_id: String) {
    let sender = {
        let mut cancels = state
            .cancels
            .lock()
            .expect("LlmState::cancels mutex poisoned");
        cancels.remove(&request_id)
    };
    if let Some(tx) = sender {
        // Race: if the stream already completed (and dropped its
        // Receiver) between our `remove` and this `send`, `send` returns
        // Err. That outcome is indistinguishable from "we successfully
        // cancelled an in-flight stream" from the caller's point of
        // view, and is the correct interpretation of the race.
        let _: Result<(), ()> = tx.send(());
    }
}

#[tauri::command]
pub fn set_provider_secret(
    provider_id: String,
    name: String,
    value: String,
) -> Result<(), String> {
    if value.is_empty() {
        return Err("value must be non-empty".to_string());
    }
    secrets::set_provider_secret(&provider_id, &name, &value).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn list_provider_secret_names(provider_id: String) -> Result<Vec<String>, String> {
    secrets::list_provider_secret_names(&provider_id).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn delete_provider_secret(provider_id: String, name: String) -> Result<(), String> {
    secrets::delete_provider_secret(&provider_id, &name).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn delete_all_provider_secrets(provider_id: String) -> Result<(), String> {
    secrets::delete_all_provider_secrets(&provider_id).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn pluely_license_status() -> Result<bool, String> {
    secrets::pluely_license_status().map_err(|e| e.to_string())
}

#[tauri::command]
pub fn pluely_license_set(license_key: String, instance_id: String) -> Result<(), String> {
    secrets::pluely_license_set(&license_key, &instance_id).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn pluely_license_clear() -> Result<(), String> {
    secrets::pluely_license_clear().map_err(|e| e.to_string())
}

#[tauri::command]
pub fn pluely_selected_model_get() -> Result<Option<pluely::Model>, String> {
    secrets::pluely_selected_model_get().map_err(|e| e.to_string())
}

#[tauri::command]
pub fn pluely_selected_model_set(model: pluely::Model) -> Result<(), String> {
    secrets::pluely_selected_model_set(&model).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn mark_secret_migration_complete(app: AppHandle) -> Result<(), String> {
    secrets::finalize_legacy_migration(&app).map_err(|e| e.to_string())
}
