// Rust half of the one-time secret migration. Was excised from
// `src-tauri/src/llm/secrets.rs`, `src-tauri/src/llm/commands.rs`,
// and `src-tauri/src/lib.rs` after running once on the dev machine.
//
// Lives here for forensic / re-run value only. Production code carries
// no migration logic — users upgrading past commit XXX lose their
// stored Pluely license / instance id / selected model and re-enter
// them. Custom-provider API keys are likewise re-entered through the
// settings UI.
//
// Pairs with `migrations.ts` (the JS half).

// ============================================================
// In `src-tauri/src/llm/secrets.rs`
// ============================================================

const SVC_META: &str = "pluely.meta";
const ACCT_MIGRATED: &str = "keychain_migrated_v1";

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

// ============================================================
// In `src-tauri/src/llm/commands.rs`
// ============================================================

#[tauri::command]
pub fn mark_secret_migration_complete(app: AppHandle) -> Result<(), String> {
    secrets::finalize_legacy_migration(&app).map_err(|e| e.to_string())
}

// ============================================================
// In `src-tauri/src/lib.rs` setup() — before `app.manage(LlmState)`
// ============================================================

if let Err(e) = llm::secrets::run_legacy_migration(&app.handle()) {
    eprintln!("Secret migration probe failed: {}", e);
}

// And in `generate_handler!`:
//     llm::commands::mark_secret_migration_complete,
