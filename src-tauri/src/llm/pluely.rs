//! Pluely-hosted streaming path + the `/api/response`, `/api/activity`,
//! `/api/error` machinery that used to live in `api.rs`.

use crate::api::{get_api_access_key, get_app_endpoint};
use crate::db::{queries, Db};
use crate::llm::{commands::StreamChatRequest, stream, LlmError, StreamEvent};
use serde::{Deserialize, Serialize};
use tauri::ipc::Channel;
use tauri::{AppHandle, Manager};
use tauri_plugin_machine_uid::MachineUidExt;
use tokio::sync::oneshot;

pub const SETTING_SELECTED_MODEL: &str = "pluely_selected_model";

/// The Pluely-hosted model the user picked, read from the SQLite `settings`
/// table (not the keychain — a model choice is a preference, not a secret).
pub async fn selected_model_get(app: &AppHandle) -> Result<Option<Model>, LlmError> {
    let db = app.state::<Db>();
    let raw = db
        .with_conn(|c| queries::setting_get(c, SETTING_SELECTED_MODEL))
        .await?;
    match raw {
        None => Ok(None),
        Some(json) => Ok(Some(serde_json::from_str(&json)?)),
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Model {
    pub provider: String,
    pub name: String,
    pub id: String,
    pub model: String,
    pub description: String,
    pub modality: String,
    #[serde(rename = "isAvailable")]
    pub is_available: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ApiResponseConfig {
    pub url: String,
    pub user_token: String,
    pub model: String,
    pub body: String,
    pub customer_id: Option<i64>,
    pub customer_email: Option<String>,
    pub customer_name: Option<String>,
    #[serde(rename = "user_audio")]
    pub user_audio: Option<UserAudioConfig>,
    pub errors: Option<Vec<ApiConfigError>>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ApiConfigError {
    pub includes: String,
    pub error: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct UserAudioHeader {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct UserAudioConfig {
    pub url: String,
    #[serde(rename = "fallback_url")]
    pub fallback_url: Option<String>,
    pub model: String,
    #[serde(rename = "fallback_model")]
    pub fallback_model: Option<String>,
    #[serde(rename = "user_token")]
    pub user_token: String,
    #[serde(rename = "fallback_user_token")]
    pub fallback_user_token: Option<String>,
    pub headers: Option<Vec<UserAudioHeader>>,
}

/// `LlmError::PluelyConfig` carries the user-visible error string.
pub async fn fetch_api_response_config(
    app: &AppHandle,
    http: &reqwest::Client,
    provider: Option<String>,
    model: Option<String>,
) -> Result<ApiResponseConfig, LlmError> {
    let endpoint = get_app_endpoint().map_err(LlmError::PluelyConfig)?;
    let access_key = get_api_access_key().map_err(LlmError::PluelyConfig)?;
    let machine_id = app
        .machine_uid()
        .get_machine_uid()
        .map_err(|e| LlmError::PluelyConfig(format!("machine_uid: {e}")))?
        .id
        .ok_or_else(|| LlmError::PluelyConfig("machine_uid empty".to_string()))?;

    let url = format!("{}/api/response", endpoint);
    let mut req = http
        .get(&url)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {}", access_key))
        .header("machine_id", &machine_id);
    if let Some(p) = provider {
        req = req.header("provider", p);
    }
    if let Some(m) = model {
        req = req.header("model", m);
    }

    let response = req.send().await?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if let Ok(j) = serde_json::from_str::<serde_json::Value>(&body) {
            if let Some(msg) = j
                .get("error")
                .and_then(|e| e.as_str())
                .or_else(|| j.get("message").and_then(|m| m.as_str()))
            {
                return Err(LlmError::PluelyConfig(format!(
                    "Server error ({}): {}",
                    status, msg
                )));
            }
        }
        return Err(LlmError::PluelyConfig(format!(
            "Server error ({}): {}",
            status, body
        )));
    }
    let cfg: ApiResponseConfig = response.json().await?;
    Ok(cfg)
}

pub fn map_api_error_message(error_rules: &[ApiConfigError], sources: &[String]) -> String {
    for source in sources {
        for rule in error_rules {
            if !rule.includes.is_empty() && source.contains(&rule.includes) {
                return rule.error.clone();
            }
        }
    }
    if let Some(default_rule) = error_rules.iter().find(|r| r.includes.trim().is_empty()) {
        return default_rule.error.clone();
    }
    error_rules
        .first()
        .map(|r| r.error.clone())
        .unwrap_or_else(|| {
            "Something went wrong. Please try switching to a different model or contact support.".to_string()
        })
}

pub async fn report_api_error(
    app: &AppHandle,
    http: &reqwest::Client,
    error_message: String,
    endpoint_path: String,
    model: Option<String>,
    provider: Option<String>,
) {
    let Ok(endpoint) = get_app_endpoint() else { return };
    let Ok(access_key) = get_api_access_key() else { return };

    let machine_id = match app.machine_uid().get_machine_uid() {
        Ok(id) => id.id.unwrap_or_default(),
        Err(_) => return,
    };
    if machine_id.is_empty() {
        return;
    }
    let app_version = app.package_info().version.to_string();

    let final_model = model.unwrap_or_default();
    let final_provider = provider.unwrap_or_default();

    let payload = serde_json::json!({
        "machine_id": machine_id,
        "error_message": error_message,
        "app_version": app_version,
        "endpoint": endpoint_path,
        "model": final_model,
        "provider": final_provider,
    });

    let url = format!("{}/api/error", endpoint.trim_end_matches('/'));
    if let Err(e) = http
        .post(&url)
        .header("Authorization", format!("Bearer {}", access_key))
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await
    {
        tracing::warn!("Failed to report API error: {}", e);
    }
}

async fn user_activity(
    app: &AppHandle,
    http: &reqwest::Client,
    activity_metrics: Option<serde_json::Value>,
    ai_model: String,
) {
    let Ok(endpoint) = get_app_endpoint() else { return };
    let Ok(access_key) = get_api_access_key() else { return };
    let machine_id = match app.machine_uid().get_machine_uid() {
        Ok(id) => id.id.unwrap_or_default(),
        Err(_) => return,
    };
    if machine_id.is_empty() {
        return;
    }
    let app_version = app.package_info().version.to_string();

    let mut payload = serde_json::json!({
        "machine_id": machine_id,
        "app_version": app_version,
        "ai_model": ai_model,
    });
    if let Some(metrics) = activity_metrics {
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("usage".to_string(), metrics);
        }
    }

    let url = format!("{}/api/activity", endpoint.trim_end_matches('/'));
    let _ = http
        .post(&url)
        .header("Authorization", format!("Bearer {}", access_key))
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await;
}

pub async fn stream_pluely(
    app: &AppHandle,
    http: &reqwest::Client,
    request: StreamChatRequest,
    channel: &Channel<StreamEvent>,
    cancel_rx: &mut oneshot::Receiver<()>,
) -> Result<String, LlmError> {
    let selected_model = selected_model_get(app).await?;
    let (provider_name, model_name) = selected_model
        .as_ref()
        .map(|m| (Some(m.provider.clone()), Some(m.model.clone())))
        .unwrap_or((None, None));

    let api_config =
        fetch_api_response_config(app, http, provider_name.clone(), model_name.clone()).await?;

    let extra_body: serde_json::Value = if api_config.body.is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_str(&api_config.body).unwrap_or_else(|_| serde_json::json!({}))
    };

    // Build messages in OpenAI-compatible shape.
    let mut messages: Vec<serde_json::Value> = Vec::new();
    if let Some(sp) = request.system_prompt.as_deref() {
        if !sp.is_empty() {
            messages.push(serde_json::json!({ "role": "system", "content": sp }));
        }
    }
    for h in &request.history {
        messages.push(serde_json::json!({
            "role": h.role.as_str(),
            "content": [{"type": "text", "text": h.content}],
        }));
    }
    let mut content_parts: Vec<serde_json::Value> =
        vec![serde_json::json!({"type": "text", "text": request.message})];
    for f in &request.attached_files {
        if f.mime.starts_with("image/") {
            content_parts.push(serde_json::json!({
                "type": "image_url",
                "image_url": { "url": format!("data:{};base64,{}", f.mime, f.base64) }
            }));
        } else {
            // OpenAI-compatible `file` part. Providers that can't take
            // PDFs reject the request visibly instead of us silently
            // dropping the attachment.
            assert_eq!(
                f.mime, "application/pdf",
                "stream_chat forwards only image/* and PDF attachments"
            );
            content_parts.push(serde_json::json!({
                "type": "file",
                "file": {
                    "filename": f.name,
                    "file_data": format!("data:{};base64,{}", f.mime, f.base64)
                }
            }));
        }
    }
    messages.push(serde_json::json!({ "role": "user", "content": content_parts }));

    let mut body = serde_json::json!({
        "model": api_config.model,
        "messages": messages,
        "stream": true,
    });
    if let (Some(extra), Some(req_obj)) = (extra_body.as_object(), body.as_object_mut()) {
        for (k, v) in extra {
            req_obj.insert(k.clone(), v.clone());
        }
    }

    let error_rules = api_config.errors.clone().unwrap_or_default();
    let send_fut = http
        .post(&api_config.url)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {}", api_config.user_token))
        .json(&body)
        .send();

    let response = tokio::select! {
        biased;
        _ = &mut *cancel_rx => return Err(LlmError::Cancelled),
        r = send_fut => match r {
            Ok(r) => r,
            Err(e) => {
                let final_msg = map_api_error_message(&error_rules, &[e.to_string()]);
                report_api_error(
                    app, http, e.to_string(), "/api/chat".to_string(),
                    model_name.clone(), provider_name.clone(),
                ).await;
                return Err(LlmError::PluelyConfig(final_msg));
            }
        }
    };

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response.text().await.unwrap_or_default();
        let mut sources = vec![error_text.clone(), status.to_string()];
        if let Ok(j) = serde_json::from_str::<serde_json::Value>(&error_text) {
            if let Some(s) = j.get("error").and_then(|v| v.as_str()) {
                sources.push(s.to_string());
            }
            if let Some(s) = j.get("message").and_then(|v| v.as_str()) {
                sources.push(s.to_string());
            }
        }
        let final_msg = map_api_error_message(&error_rules, &sources);
        report_api_error(
            app,
            http,
            format!("{}: {}", status, error_text),
            "/api/chat".to_string(),
            model_name.clone(),
            provider_name.clone(),
        )
        .await;
        return Err(LlmError::PluelyConfig(final_msg));
    }

    let outcome = stream::stream_sse(response, channel, cancel_rx, |parsed| {
        parsed
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
            .and_then(|c| c.get("delta"))
            .and_then(|d| d.get("content"))
            .and_then(|c| c.as_str())
            .map(String::from)
    })
    .await;

    let outcome = match outcome {
        Ok(o) => o,
        Err(LlmError::Reqwest(e)) => {
            let final_msg = map_api_error_message(&error_rules, &[e.to_string()]);
            report_api_error(
                app,
                http,
                e.to_string(),
                "/api/chat".to_string(),
                model_name.clone(),
                provider_name.clone(),
            )
            .await;
            return Err(LlmError::PluelyConfig(final_msg));
        }
        Err(other) => return Err(other),
    };

    if !outcome.full_response.is_empty() {
        let ai_model = model_name.clone().unwrap_or_else(|| api_config.model.clone());
        user_activity(app, http, outcome.usage.clone(), ai_model).await;
    }

    Ok(outcome.full_response)
}
