use crate::llm::secrets;
use serde::{Deserialize, Serialize};
use std::env;
use tauri::AppHandle;
use tauri_plugin_machine_uid::MachineUidExt;
use uuid::Uuid;

fn get_payment_endpoint() -> Result<String, String> {
    if let Ok(endpoint) = env::var("PAYMENT_ENDPOINT") {
        return Ok(endpoint);
    }

    match option_env!("PAYMENT_ENDPOINT") {
        Some(endpoint) => Ok(endpoint.to_string()),
        None => Err("PAYMENT_ENDPOINT environment variable not set. Please ensure it's set during the build process.".to_string())
    }
}

fn get_api_access_key() -> Result<String, String> {
    if let Ok(key) = env::var("API_ACCESS_KEY") {
        return Ok(key.to_string());
    }

    match option_env!("API_ACCESS_KEY") {
        Some(key) => Ok(key.to_string()),
        None => Err("API_ACCESS_KEY environment variable not set. Please ensure it's set during the build process.".to_string())
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ActivationRequest {
    license_key: String,
    instance_name: String,
    machine_id: String,
    app_version: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ActivationResponse {
    activated: bool,
    error: Option<String>,
    license_key: Option<String>,
    instance: Option<InstanceInfo>,
    is_dev_license: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ValidateResponse {
    is_active: bool,
    last_validated_at: Option<String>,
    is_dev_license: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InstanceInfo {
    id: String,
    name: String,
    created_at: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CheckoutResponse {
    success: Option<bool>,
    checkout_url: Option<String>,
    error: Option<String>,
}

fn map_reqwest_error(e: reqwest::Error) -> String {
    let error_msg = format!("{}", e);
    if error_msg.contains("url (") {
        let parts: Vec<&str> = error_msg.split(" for url (").collect();
        if parts.len() > 1 {
            return format!("Failed to make chat request: {}", parts[0]);
        }
    }
    format!("Failed to make chat request: {}", error_msg)
}

#[tauri::command]
pub async fn activate_license_api(
    app: AppHandle,
    license_key: String,
) -> Result<ActivationResponse, String> {
    let payment_endpoint = get_payment_endpoint()?;
    let api_access_key = get_api_access_key()?;

    let instance_name = Uuid::new_v4().to_string();
    let machine_id: String = app.machine_uid().get_machine_uid().unwrap().id.unwrap();
    let app_version: String = env!("CARGO_PKG_VERSION").to_string();
    let activation_request = ActivationRequest {
        license_key: license_key.clone(),
        instance_name: instance_name.clone(),
        machine_id: machine_id.clone(),
        app_version: app_version.clone(),
    };

    let client = reqwest::Client::new();
    let url = format!("{}/activate", payment_endpoint);

    let response = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {}", api_access_key))
        .json(&activation_request)
        .send()
        .await
        .map_err(map_reqwest_error)?;

    let activation_response: ActivationResponse =
        response.json().await.map_err(map_reqwest_error)?;

    // Persist credentials in the OS keychain on successful activation so
    // subsequent commands can resolve license/instance without a roundtrip
    // through JS. The Pluely backend returns the canonical license key and
    // the instance ID it minted for this activation.
    if activation_response.activated {
        if let (Some(lk), Some(inst)) = (
            activation_response.license_key.as_deref(),
            activation_response.instance.as_ref().map(|i| i.id.as_str()),
        ) {
            secrets::pluely_license_set(lk, inst).map_err(|e| e.to_string())?;
        }
    }

    Ok(activation_response)
}

#[tauri::command]
pub async fn deactivate_license_api(app: AppHandle) -> Result<ActivationResponse, String> {
    let payment_endpoint = get_payment_endpoint()?;
    let api_access_key = get_api_access_key()?;
    let machine_id: String = app.machine_uid().get_machine_uid().unwrap().id.unwrap();
    let license_key = secrets::pluely_license_key()
        .map_err(|e| e.to_string())?
        .unwrap_or_default();
    let instance_id = secrets::pluely_instance_id()
        .map_err(|e| e.to_string())?
        .unwrap_or_default();
    let app_version: String = env!("CARGO_PKG_VERSION").to_string();
    let deactivation_request = ActivationRequest {
        license_key: license_key.clone(),
        instance_name: instance_id.clone(),
        machine_id: machine_id.clone(),
        app_version: app_version.clone(),
    };

    let client = reqwest::Client::new();
    let url = format!("{}/deactivate", payment_endpoint);

    let response = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {}", api_access_key))
        .json(&deactivation_request)
        .send()
        .await
        .map_err(map_reqwest_error)?;
    let deactivation_response: ActivationResponse =
        response.json().await.map_err(map_reqwest_error)?;

    // Clear keychain credentials on successful deactivation. We treat
    // "not activated" responses as success-for-cleanup as well — if the
    // server says we don't have an active license anymore, we shouldn't
    // keep stale credentials locally.
    if !deactivation_response.activated || deactivation_response.error.is_none() {
        secrets::pluely_license_clear().map_err(|e| e.to_string())?;
    }

    Ok(deactivation_response)
}

#[tauri::command]
pub async fn validate_license_api(app: AppHandle) -> Result<ValidateResponse, String> {
    let payment_endpoint = get_payment_endpoint()?;
    let api_access_key = get_api_access_key()?;
    let machine_id: String = app.machine_uid().get_machine_uid().unwrap().id.unwrap();
    let license_key = secrets::pluely_license_key()
        .map_err(|e| e.to_string())?
        .unwrap_or_default();
    let instance_id = secrets::pluely_instance_id()
        .map_err(|e| e.to_string())?
        .unwrap_or_default();
    let app_version: String = env!("CARGO_PKG_VERSION").to_string();

    if license_key.is_empty() || instance_id.is_empty() {
        return Ok(ValidateResponse {
            is_active: false,
            last_validated_at: None,
            is_dev_license: false,
        });
    }

    let validate_request = ActivationRequest {
        license_key: license_key.clone(),
        instance_name: instance_id.clone(),
        machine_id: machine_id.clone(),
        app_version: app_version.clone(),
    };

    let client = reqwest::Client::new();
    let url = format!("{}/validate", payment_endpoint);

    let response = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {}", api_access_key))
        .json(&validate_request)
        .send()
        .await
        .map_err(map_reqwest_error)?;

    let validate_response: ValidateResponse = response.json().await.map_err(map_reqwest_error)?;
    Ok(validate_response)
}

#[tauri::command]
pub async fn get_checkout_url() -> Result<CheckoutResponse, String> {
    let payment_endpoint = get_payment_endpoint()?;
    let api_access_key = get_api_access_key()?;

    let client = reqwest::Client::new();
    let url = format!("{}/checkout", payment_endpoint);

    let response = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {}", api_access_key))
        .json(&serde_json::json!({}))
        .send()
        .await
        .map_err(map_reqwest_error)?;

    let checkout_response: CheckoutResponse = response.json().await.map_err(map_reqwest_error)?;
    Ok(checkout_response)
}
