//! Tauri command surface for the LLM subsystem.
//!
//! `stream_chat` is the renderer's streaming entrypoint; it and the system-audio
//! capture task both go through `complete`. Cancellation of `stream_chat` is
//! per-request via the `cancel_chat` command and the `LlmState` cancel registry.

use crate::db::Db;
use crate::db::schema::AttachedFile;
use crate::llm::{pluely, provider, secrets::ProviderKind, stt, LlmError, LlmState, StreamEvent};
use serde::Deserialize;
use std::collections::HashMap;
use tauri::ipc::Channel;
use tauri::{AppHandle, State};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Chat {
    pub provider: ProviderInput,
    pub message: String,
    #[serde(default)]
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub history: Vec<provider::HistoryMessage>,
    #[serde(default)]
    pub attached_files: Vec<AttachedFile>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamChatRequest {
    #[serde(flatten)]
    pub chat: Chat,
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

/// Routes to the Pluely-hosted or custom path. Text attachments are inlined into the message
/// so both paths see them uniformly; only images and PDFs survive as structured attachments.
pub(crate) async fn complete(
    app: &AppHandle,
    llm: &LlmState,
    mut chat: Chat,
    on_delta: &mut impl FnMut(String) -> Result<(), LlmError>,
) -> Result<String, LlmError> {
    let (binary, text): (Vec<_>, Vec<_>) = chat
        .attached_files
        .drain(..)
        .partition(|f| f.mime.starts_with("image/") || f.mime == "application/pdf");
    chat.attached_files = binary;
    for f in &text {
        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&f.base64)
            .map_err(|e| LlmError::TextAttachment(f.name.clone(), e.to_string()))?;
        let content = String::from_utf8(bytes)
            .map_err(|_| LlmError::TextAttachment(f.name.clone(), "not valid UTF-8".into()))?;
        chat.message = format!(
            "{}\n\n--- attached file: {} ---\n{}",
            chat.message, f.name, content
        );
    }
    if chat.provider.is_pluely_hosted {
        pluely::stream_pluely(app, &llm.http, chat, on_delta).await
    } else {
        provider::stream_custom(&llm.http, &llm.secrets, chat, on_delta).await
    }
}

#[tauri::command]
pub async fn stream_chat(
    app: AppHandle,
    state: State<'_, LlmState>,
    request: StreamChatRequest,
    channel: Channel<StreamEvent>,
) -> Result<String, String> {
    let request_id = request.request_id;
    let mut reg = state
        .cancels
        .register(request_id.clone())
        .map_err(|e| e.to_string())?;

    let mut on_delta = |delta| {
        channel
            .send(StreamEvent::Chunk { delta })
            .map_err(|e| LlmError::Channel(e.to_string()))
    };
    let result = tokio::select! {
        biased;
        _ = &mut reg.rx => Err(LlmError::Cancelled),
        r = complete(&app, &state, request.chat, &mut on_delta) => r,
    };
    drop(reg);

    let full_response = match result {
        Ok(full) => full,
        Err(LlmError::Cancelled) => String::new(),
        Err(e) => return Err(e.to_string()),
    };
    channel
        .send(StreamEvent::Done {
            full_response,
            request_id: request_id.clone(),
        })
        .map_err(|e| e.to_string())?;
    Ok(request_id)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscribeRequest {
    pub provider: ProviderInput,
    pub audio_base64: String,
    pub mime: String,
}

#[tauri::command]
pub async fn transcribe(
    app: AppHandle,
    state: State<'_, LlmState>,
    request: TranscribeRequest,
) -> Result<String, String> {
    use base64::Engine as _;
    let audio = base64::engine::general_purpose::STANDARD
        .decode(&request.audio_base64)
        .map_err(|e| format!("audio base64: {e}"))?;
    stt::transcribe(&app, &state, &request.provider, &audio, &request.mime)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn cancel_chat(state: State<'_, LlmState>, request_id: String) {
    state.cancels.cancel(&request_id)
}

#[tauri::command]
pub async fn set_provider_secret(
    state: State<'_, LlmState>,
    kind: ProviderKind,
    provider_id: String,
    name: String,
    value: String,
) -> Result<(), String> {
    if value.is_empty() {
        return Err("value must be non-empty".to_string());
    }
    state
        .secrets
        .set(kind, &provider_id, &name, &value)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn list_provider_secret_names(
    state: State<'_, LlmState>,
    kind: ProviderKind,
    provider_id: String,
) -> Result<Vec<String>, String> {
    let mut names: Vec<String> = state
        .secrets
        .provider(kind, &provider_id)
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
    kind: ProviderKind,
    provider_id: String,
    name: String,
) -> Result<(), String> {
    state
        .secrets
        .delete(kind, &provider_id, &name)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn delete_all_provider_secrets(
    state: State<'_, LlmState>,
    kind: ProviderKind,
    provider_id: String,
) -> Result<(), String> {
    state
        .secrets
        .delete_all(kind, &provider_id)
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
