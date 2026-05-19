//! OS keychain helpers, plus the one-time legacy migration off the
//! plaintext `secure_storage.json` file (and JS-side localStorage
//! variable dicts).
//!
//! Namespacing:
//!
//! | Domain                 | service                     | account            |
//! |------------------------|-----------------------------|--------------------|
//! | Provider secret value  | `pluely.provider.<id>`      | `<variable_name>`  |
//! | Provider names index   | `pluely.provider.<id>`      | `__names__` (JSON) |
//! | Pluely license key     | `pluely.license`            | `license_key`      |
//! | Pluely instance id     | `pluely.license`            | `instance_id`      |
//! | Pluely selected model  | `pluely.license`            | `selected_model`   |
//! | Migration done marker  | `pluely.meta`               | `keychain_migrated_v1` |
//!
//! `keyring-rs` v3 has no portable "list entries by service" primitive,
//! so we maintain an explicit names-list entry per provider.

use crate::llm::pluely::Model;
use crate::llm::LlmError;
use keyring::Entry;
use serde::Deserialize;
use std::path::PathBuf;
use tauri::{AppHandle, Manager};

const SVC_PROVIDER_PREFIX: &str = "pluely.provider.";
const SVC_LICENSE: &str = "pluely.license";
const SVC_META: &str = "pluely.meta";

const ACCT_NAMES: &str = "__names__";
const ACCT_LICENSE_KEY: &str = "license_key";
const ACCT_INSTANCE_ID: &str = "instance_id";
const ACCT_SELECTED_MODEL: &str = "selected_model";
const ACCT_MIGRATED: &str = "keychain_migrated_v1";

fn entry(service: &str, account: &str) -> Result<Entry, LlmError> {
    Entry::new(service, account).map_err(|e| LlmError::Keychain(e.to_string()))
}

fn read_opt(service: &str, account: &str) -> Result<Option<String>, LlmError> {
    match entry(service, account)?.get_password() {
        Ok(v) => Ok(Some(v)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(LlmError::Keychain(e.to_string())),
    }
}

fn write(service: &str, account: &str, value: &str) -> Result<(), LlmError> {
    entry(service, account)?
        .set_password(value)
        .map_err(|e| LlmError::Keychain(e.to_string()))
}

fn delete(service: &str, account: &str) -> Result<(), LlmError> {
    match entry(service, account)?.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(LlmError::Keychain(e.to_string())),
    }
}

fn provider_service(id: &str) -> String {
    format!("{}{}", SVC_PROVIDER_PREFIX, id)
}

fn read_names(provider_id: &str) -> Result<Vec<String>, LlmError> {
    let svc = provider_service(provider_id);
    match read_opt(&svc, ACCT_NAMES)? {
        None => Ok(Vec::new()),
        Some(json) => serde_json::from_str(&json).map_err(LlmError::from),
    }
}

fn write_names(provider_id: &str, names: &[String]) -> Result<(), LlmError> {
    let svc = provider_service(provider_id);
    write(&svc, ACCT_NAMES, &serde_json::to_string(names)?)
}

pub fn set_provider_secret(provider_id: &str, name: &str, value: &str) -> Result<(), LlmError> {
    let svc = provider_service(provider_id);
    write(&svc, name, value)?;
    let mut names = read_names(provider_id)?;
    if !names.iter().any(|n| n == name) {
        names.push(name.to_string());
        write_names(provider_id, &names)?;
    }
    Ok(())
}

pub fn get_provider_secret(provider_id: &str, name: &str) -> Result<Option<String>, LlmError> {
    read_opt(&provider_service(provider_id), name)
}

pub fn list_provider_secret_names(provider_id: &str) -> Result<Vec<String>, LlmError> {
    read_names(provider_id)
}

pub fn delete_provider_secret(provider_id: &str, name: &str) -> Result<(), LlmError> {
    let svc = provider_service(provider_id);
    delete(&svc, name)?;
    let mut names = read_names(provider_id)?;
    names.retain(|n| n != name);
    write_names(provider_id, &names)
}

pub fn delete_all_provider_secrets(provider_id: &str) -> Result<(), LlmError> {
    let svc = provider_service(provider_id);
    let names = read_names(provider_id)?;
    for n in &names {
        delete(&svc, n)?;
    }
    delete(&svc, ACCT_NAMES)
}

pub fn pluely_license_status() -> Result<bool, LlmError> {
    Ok(read_opt(SVC_LICENSE, ACCT_LICENSE_KEY)?.is_some()
        && read_opt(SVC_LICENSE, ACCT_INSTANCE_ID)?.is_some())
}

pub fn pluely_license_key() -> Result<Option<String>, LlmError> {
    read_opt(SVC_LICENSE, ACCT_LICENSE_KEY)
}

pub fn pluely_instance_id() -> Result<Option<String>, LlmError> {
    read_opt(SVC_LICENSE, ACCT_INSTANCE_ID)
}

pub fn pluely_license_set(license_key: &str, instance_id: &str) -> Result<(), LlmError> {
    write(SVC_LICENSE, ACCT_LICENSE_KEY, license_key)?;
    write(SVC_LICENSE, ACCT_INSTANCE_ID, instance_id)
}

pub fn pluely_license_clear() -> Result<(), LlmError> {
    delete(SVC_LICENSE, ACCT_LICENSE_KEY)?;
    delete(SVC_LICENSE, ACCT_INSTANCE_ID)?;
    delete(SVC_LICENSE, ACCT_SELECTED_MODEL)
}

pub fn pluely_selected_model_get() -> Result<Option<Model>, LlmError> {
    let json = match read_opt(SVC_LICENSE, ACCT_SELECTED_MODEL)? {
        None => return Ok(None),
        Some(s) => s,
    };
    Ok(Some(serde_json::from_str(&json)?))
}

pub fn pluely_selected_model_set(model: &Model) -> Result<(), LlmError> {
    write(SVC_LICENSE, ACCT_SELECTED_MODEL, &serde_json::to_string(model)?)
}

fn is_migrated() -> Result<bool, LlmError> {
    Ok(read_opt(SVC_META, ACCT_MIGRATED)?.is_some())
}

fn mark_migrated() -> Result<(), LlmError> {
    write(SVC_META, ACCT_MIGRATED, "1")
}

fn legacy_storage_path(app: &AppHandle) -> Result<PathBuf, LlmError> {
    Ok(app
        .path()
        .app_data_dir()
        .map_err(|e| LlmError::Keychain(format!("app_data_dir: {e}")))?
        .join("secure_storage.json"))
}

#[derive(Deserialize)]
struct LegacySecureStorage {
    license_key: Option<String>,
    instance_id: Option<String>,
    selected_pluely_model: Option<String>,
}

/// Read `secure_storage.json` (if present and not yet migrated) and copy
/// its license/instance/selected-model into the keychain. Does NOT delete
/// the file or set the migration marker — that happens in
/// `finalize_legacy_migration`, after the JS side has finished its
/// matching scrub of `localStorage.variables`.
pub fn run_legacy_migration(app: &AppHandle) -> Result<(), LlmError> {
    if is_migrated()? {
        return Ok(());
    }
    let path = legacy_storage_path(app)?;
    if !path.exists() {
        return Ok(());
    }
    let content = std::fs::read_to_string(&path)
        .map_err(|e| LlmError::Keychain(format!("read secure_storage.json: {e}")))?;
    let storage: LegacySecureStorage = serde_json::from_str(&content)?;
    if let (Some(lk), Some(iid)) = (storage.license_key, storage.instance_id) {
        pluely_license_set(&lk, &iid)?;
    }
    if let Some(model_json) = storage.selected_pluely_model {
        write(SVC_LICENSE, ACCT_SELECTED_MODEL, &model_json)?;
    }
    Ok(())
}

/// Called by the JS migration shim once it has scrubbed its localStorage
/// variable dicts. Deletes the plaintext file and writes the keychain
/// "migrated" marker.
pub fn finalize_legacy_migration(app: &AppHandle) -> Result<(), LlmError> {
    let path = legacy_storage_path(app)?;
    if path.exists() {
        std::fs::remove_file(&path)
            .map_err(|e| LlmError::Keychain(format!("remove secure_storage.json: {e}")))?;
    }
    mark_migrated()
}
