//! Tauri command surface for the LLM subsystem.
//!
//! `stream_chat` is the sole streaming entrypoint; it routes between
//! `pluely::stream_pluely` and `provider::stream_custom` internally based
//! on `request.provider.is_pluely_hosted`. Cancellation is per-request
//! via the `cancel_chat` command and the `LlmState` cancel registry.

use crate::db::Db;
use crate::db::schema::AttachedFile;
use crate::llm::{pluely, provider, LlmError, LlmState, StreamEvent};
use serde::Deserialize;
use std::collections::HashMap;
use tauri::ipc::Channel;
use tauri::{AppHandle, State};

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
    let mut reg = state
        .cancels
        .register(request_id.clone())
        .map_err(|e| e.to_string())?;

    let http = state.http.clone();
    let is_pluely = request.provider.is_pluely_hosted;

    // Text attachments are inlined into the message here so both streaming
    // paths see them uniformly; only images and PDFs survive as structured
    // attachments.
    let mut request = request;
    let (binary, text): (Vec<_>, Vec<_>) = request
        .attached_files
        .drain(..)
        .partition(|f| f.mime.starts_with("image/") || f.mime == "application/pdf");
    request.attached_files = binary;
    let inlined = text.iter().try_for_each(|f| {
        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&f.base64)
            .map_err(|e| LlmError::TextAttachment(f.name.clone(), e.to_string()))?;
        let content = String::from_utf8(bytes)
            .map_err(|_| LlmError::TextAttachment(f.name.clone(), "not valid UTF-8".into()))?;
        request.message = format!(
            "{}\n\n--- attached file: {} ---\n{}",
            request.message, f.name, content
        );
        Ok(())
    });

    let result = match inlined {
        Err(e) => Err(e),
        Ok(()) => {
            if is_pluely {
                pluely::stream_pluely(&app, &http, request, &channel, &mut reg.rx).await
            } else {
                provider::stream_custom(&http, &state.secrets, request, &channel, &mut reg.rx)
                    .await
            }
        }
    };

    drop(reg);

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
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
pub fn cancel_chat(state: State<'_, LlmState>, request_id: String) {
    state.cancels.cancel(&request_id)
}

#[tauri::command]
pub async fn set_provider_secret(
    state: State<'_, LlmState>,
    provider_id: String,
    name: String,
    value: String,
) -> Result<(), String> {
    if value.is_empty() {
        return Err("value must be non-empty".to_string());
    }
    state
        .secrets
        .set(&provider_id, &name, &value)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn list_provider_secret_names(
    state: State<'_, LlmState>,
    provider_id: String,
) -> Result<Vec<String>, String> {
    let mut names: Vec<String> = state
        .secrets
        .provider(&provider_id)
        .await
        .map_err(|e| e.to_string())?
        .into_keys()
        .collect();
    names.sort();
    Ok(names)
}

#[tauri::command]
pub async fn delete_provider_secret(
    state: State<'_, LlmState>,
    provider_id: String,
    name: String,
) -> Result<(), String> {
    state
        .secrets
        .delete(&provider_id, &name)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn delete_all_provider_secrets(
    state: State<'_, LlmState>,
    provider_id: String,
) -> Result<(), String> {
    state
        .secrets
        .delete_all(&provider_id)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn pluely_selected_model_get(
    app: AppHandle,
) -> Result<Option<pluely::Model>, String> {
    pluely::selected_model_get(&app).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn pluely_selected_model_set(
    db: State<'_, Db>,
    model: pluely::Model,
) -> Result<(), String> {
    let json = serde_json::to_string(&model).map_err(|e| e.to_string())?;
    db.with_conn(move |c| {
        crate::db::queries::setting_set(c, pluely::SETTING_SELECTED_MODEL, &json)
    })
    .await
    .map_err(|e| e.to_string())
}
