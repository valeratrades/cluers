//! Pluely-backed remote endpoints other than LLM streaming.
//!
//! LLM streaming, provider secrets, license credential storage, and the
//! `/api/response` configuration helper live in `crate::llm`. What
//! stays here is:
//! - STT (`transcribe_audio`) and its audio-only helpers (Phase 1.3 will
//!   relocate STT into a dedicated module);
//! - Pluely-side catalog endpoints: `fetch_models`, `fetch_prompts`,
//!   `generate_system_prompt_via_api`, `get_activity`.

use base64::{engine::general_purpose, Engine as _};
use reqwest::multipart::{Form, Part};
use serde::{Deserialize, Serialize};
use std::env;
use tauri::AppHandle;
use tauri_plugin_machine_uid::MachineUidExt;

use crate::llm::pluely::{
    fetch_api_response_config, report_api_error, Model, UserAudioHeader,
};
use crate::llm::secrets;

pub fn get_app_endpoint() -> Result<String, String> {
    if let Ok(endpoint) = env::var("APP_ENDPOINT") {
        return Ok(endpoint);
    }
    match option_env!("APP_ENDPOINT") {
        Some(endpoint) => Ok(endpoint.to_string()),
        None => Err("APP_ENDPOINT environment variable not set. Please ensure it's set during the build process.".to_string()),
    }
}

pub fn get_api_access_key() -> Result<String, String> {
    if let Ok(key) = env::var("API_ACCESS_KEY") {
        return Ok(key);
    }
    match option_env!("API_ACCESS_KEY") {
        Some(key) => Ok(key.to_string()),
        None => Err("API_ACCESS_KEY environment variable not set. Please ensure it's set during the build process.".to_string()),
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AudioResponse {
    success: bool,
    transcription: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ModelsResponse {
    models: Vec<Model>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SystemPromptResponse {
    prompt_name: String,
    system_prompt: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PluelyPrompt {
    title: String,
    prompt: String,
    #[serde(rename = "modelId")]
    model_id: String,
    #[serde(rename = "modelName")]
    model_name: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PluelyPromptsResponse {
    prompts: Vec<PluelyPrompt>,
    total: i32,
    #[serde(rename = "last_updated")]
    last_updated: Option<String>,
}

#[tauri::command]
pub async fn transcribe_audio(
    app: AppHandle,
    audio_base64: String,
) -> Result<AudioResponse, String> {
    let selected_model = secrets::pluely_selected_model_get().map_err(|e| e.to_string())?;
    let provider = selected_model.as_ref().map(|m| m.provider.clone());
    let model = selected_model.as_ref().map(|m| m.model.clone());

    let client = reqwest::Client::new();
    let api_config = fetch_api_response_config(&app, &client, provider.clone(), model.clone())
        .await
        .map_err(|e| e.to_string())?;
    let user_audio_config = api_config.user_audio.as_ref().ok_or_else(|| {
        "Audio transcription is not configured for this workspace. Please contact support."
            .to_string()
    })?;

    let audio_bytes = decode_audio_base64(&audio_base64)?;
    let error_provider = provider.clone();
    let error_model = model.clone();
    match perform_user_audio_transcription(
        &client,
        &user_audio_config.url,
        &user_audio_config.user_token,
        &user_audio_config.model,
        user_audio_config.headers.as_ref(),
        &audio_bytes,
    )
    .await
    {
        Ok(transcription) => Ok(AudioResponse {
            success: true,
            transcription: Some(transcription),
            error: None,
        }),
        Err(primary_error) => {
            let fallback_error_message = if let (Some(fallback_url), Some(fallback_token)) = (
                user_audio_config.fallback_url.as_ref(),
                user_audio_config.fallback_user_token.as_ref(),
            ) {
                let fallback_model = user_audio_config
                    .fallback_model
                    .as_ref()
                    .unwrap_or(&user_audio_config.model);

                match perform_user_audio_transcription(
                    &client,
                    fallback_url,
                    fallback_token,
                    fallback_model,
                    user_audio_config.headers.as_ref(),
                    &audio_bytes,
                )
                .await
                {
                    Ok(transcription) => {
                        return Ok(AudioResponse {
                            success: true,
                            transcription: Some(transcription),
                            error: None,
                        });
                    }
                    Err(fallback_error) => Some(fallback_error),
                }
            } else {
                Some("fallback not configured".to_string())
            };

            tracing::warn!(
                primary_error = %primary_error,
                fallback_error = %fallback_error_message
                    .as_deref()
                    .unwrap_or("not attempted"),
                "Audio transcription failed for all configured endpoints"
            );
            let error_msg = match &fallback_error_message {
                Some(fb) => format!("Primary: {} | Fallback: {}", primary_error, fb),
                None => primary_error.clone(),
            };
            report_api_error(
                &app,
                &client,
                error_msg,
                "/api/transcribe".to_string(),
                error_model,
                error_provider,
            )
            .await;
            Err("Transcription failed. Please try again.".to_string())
        }
    }
}

fn decode_audio_base64(audio_base64: &str) -> Result<Vec<u8>, String> {
    let trimmed = audio_base64.trim();
    let base64_str = if let Some(idx) = trimmed.find(',') {
        &trimmed[idx + 1..]
    } else {
        trimmed
    };
    general_purpose::STANDARD
        .decode(base64_str)
        .map_err(|e| format!("Failed to decode audio data: {}", e))
}

async fn perform_user_audio_transcription(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    model: &str,
    headers: Option<&Vec<UserAudioHeader>>,
    audio_bytes: &[u8],
) -> Result<String, String> {
    let audio_part = Part::bytes(audio_bytes.to_vec())
        .file_name("audio.wav")
        .mime_str("audio/wav")
        .map_err(|e| format!("Failed to prepare audio payload: {}", e))?;

    let mut form = Form::new()
        .part("file", audio_part)
        .text("model", model.to_string());

    if let Some(extra_headers) = headers {
        for header in extra_headers {
            let key = header.key.trim();
            if key.is_empty() {
                continue;
            }
            form = form.text(key.to_string(), header.value.clone());
        }
    }

    let response = client
        .post(url)
        .bearer_auth(token)
        .multipart(form)
        .send()
        .await
        .map_err(|e| format!("Transcription request failed to send: {}", e))?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unable to read transcription error response".to_string());
        return Err(format!(
            "Transcription request returned {} with body: {}",
            status, error_text
        ));
    }

    let body_text = response
        .text()
        .await
        .map_err(|e| format!("Failed to read transcription response: {}", e))?;

    if body_text.trim().is_empty() {
        return Err("Transcription response was empty".to_string());
    }

    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body_text) {
        if let Some(text) = json.get("text").and_then(|v| v.as_str()) {
            return Ok(text.to_string());
        }
        if let Some(text) = json
            .get("transcription")
            .and_then(|v| v.as_str())
            .or_else(|| json.get("result").and_then(|v| v.as_str()))
        {
            return Ok(text.to_string());
        }
        return Ok(json.to_string());
    }

    Ok(body_text)
}

#[tauri::command]
pub async fn fetch_models(app: AppHandle) -> Result<Vec<Model>, String> {
    let app_endpoint = get_app_endpoint()?;
    let api_access_key = get_api_access_key()?;

    let license_key = secrets::pluely_license_key()
        .map_err(|e| e.to_string())?
        .unwrap_or_default();
    let instance_id = secrets::pluely_instance_id()
        .map_err(|e| e.to_string())?
        .unwrap_or_default();
    let machine_id = app
        .machine_uid()
        .get_machine_uid()
        .ok()
        .and_then(|uid| uid.id)
        .unwrap_or_default();
    let app_version = app.package_info().version.to_string();

    let client = reqwest::Client::new();
    let url = format!("{}/api/models", app_endpoint);

    let response = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {}", api_access_key))
        .header("license_key", &license_key)
        .header("instance", &instance_id)
        .header("machine_id", &machine_id)
        .header("app_version", &app_version)
        .send()
        .await
        .map_err(|e| format!("Failed to make models request: {}", e))?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown server error".to_string());
        if let Ok(j) = serde_json::from_str::<serde_json::Value>(&error_text) {
            if let Some(msg) = j
                .get("error")
                .and_then(|e| e.as_str())
                .or_else(|| j.get("message").and_then(|m| m.as_str()))
            {
                return Err(format!("Server error ({}): {}", status, msg));
            }
        }
        return Err(format!("Server error ({}): {}", status, error_text));
    }

    let models_response: ModelsResponse = response
        .json()
        .await
        .map_err(|e| format!("Failed to parse models response: {}", e))?;
    Ok(models_response.models)
}

#[tauri::command]
pub async fn fetch_prompts() -> Result<PluelyPromptsResponse, String> {
    let app_endpoint = get_app_endpoint()?;
    let api_access_key = get_api_access_key()?;

    let client = reqwest::Client::new();
    let url = format!("{}/api/prompts", app_endpoint);

    let response = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {}", api_access_key))
        .send()
        .await
        .map_err(|e| format!("Failed to make prompts request: {}", e))?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown server error".to_string());
        if let Ok(j) = serde_json::from_str::<serde_json::Value>(&error_text) {
            if let Some(msg) = j
                .get("error")
                .and_then(|e| e.as_str())
                .or_else(|| j.get("message").and_then(|m| m.as_str()))
            {
                return Err(format!("Server error ({}): {}", status, msg));
            }
        }
        return Err(format!("Server error ({}): {}", status, error_text));
    }

    response
        .json::<PluelyPromptsResponse>()
        .await
        .map_err(|e| format!("Failed to parse prompts response: {}", e))
}

#[tauri::command]
pub async fn generate_system_prompt_via_api(
    app: AppHandle,
    user_prompt: String,
) -> Result<SystemPromptResponse, String> {
    let app_endpoint = get_app_endpoint()?;
    let api_access_key = get_api_access_key()?;
    let license_key = secrets::pluely_license_key()
        .map_err(|e| e.to_string())?
        .ok_or("No license found. Please activate your license first.")?;
    let instance_id = secrets::pluely_instance_id()
        .map_err(|e| e.to_string())?
        .ok_or("No license found. Please activate your license first.")?;
    let machine_id: String = app
        .machine_uid()
        .get_machine_uid()
        .map_err(|e| format!("machine_uid: {}", e))?
        .id
        .ok_or("machine_uid empty")?;
    let app_version: String = app.package_info().version.to_string();

    let client = reqwest::Client::new();
    let url = format!("{}/api/prompt", app_endpoint);

    let response = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {}", api_access_key))
        .header("license_key", &license_key)
        .header("instance", &instance_id)
        .header("machine_id", &machine_id)
        .header("app_version", &app_version)
        .json(&serde_json::json!({ "user_prompt": user_prompt }))
        .send()
        .await
        .map_err(|e| format!("Failed to make prompt request: {}", e))?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown server error".to_string());
        if let Ok(j) = serde_json::from_str::<serde_json::Value>(&error_text) {
            if let Some(msg) = j
                .get("error")
                .and_then(|e| e.as_str())
                .or_else(|| j.get("message").and_then(|m| m.as_str()))
            {
                return Err(format!("Server error ({}): {}", status, msg));
            }
        }
        return Err(format!("Server error ({}): {}", status, error_text));
    }

    response
        .json::<SystemPromptResponse>()
        .await
        .map_err(|e| format!("Failed to parse system prompt response: {}", e))
}

#[tauri::command]
pub async fn get_activity(app: AppHandle) -> Result<serde_json::Value, String> {
    let app_endpoint = get_app_endpoint()?;
    let api_access_key = get_api_access_key()?;
    let license_key = secrets::pluely_license_key()
        .map_err(|e| e.to_string())?
        .ok_or("No license found. Please activate your license first.")?;
    let instance_id = secrets::pluely_instance_id()
        .map_err(|e| e.to_string())?
        .ok_or("No license found. Please activate your license first.")?;

    let machine_id = match app.machine_uid().get_machine_uid() {
        Ok(id) => id.id.unwrap_or_default(),
        Err(_) => String::new(),
    };
    if machine_id.is_empty() {
        return Err("Machine identifier unavailable".to_string());
    }

    let app_version = app.package_info().version.to_string();

    let client = reqwest::Client::new();
    let activity_url = format!("{}/api/activity", app_endpoint.trim_end_matches('/'));

    let response = client
        .get(&activity_url)
        .header("Authorization", format!("Bearer {}", api_access_key))
        .header("license_key", &license_key)
        .header("instance_name", &instance_id)
        .header("machine_id", machine_id)
        .header("app_version", app_version)
        .send()
        .await
        .map_err(|e| format!("Failed to request activity: {}", e))?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown server error".to_string());
        if let Ok(j) = serde_json::from_str::<serde_json::Value>(&error_text) {
            if let Some(msg) = j
                .get("message")
                .and_then(|m| m.as_str())
                .or_else(|| j.get("error").and_then(|m| m.as_str()))
            {
                return Err(format!("Server error ({}): {}", status, msg));
            }
        }
        return Err(format!("Server error ({}): {}", status, error_text));
    }

    response
        .json::<serde_json::Value>()
        .await
        .map_err(|e| format!("Failed to parse activity response: {}", e))
}
