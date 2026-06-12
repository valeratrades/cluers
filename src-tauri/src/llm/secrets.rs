//! Custom-provider secret storage in the OS keychain.
//!
//! One keychain entry per provider: service `pluely.provider.<id>`,
//! account `"secrets"`, value = a JSON object `{name: value}`. Every
//! operation touches the keychain at most once.
//!
//! [`Secrets`] is a write-through cache: the first read of a provider loads
//! its map from the keychain; every subsequent read is served from memory,
//! so the hot path (one custom-provider message) hits D-Bus once per
//! provider per app run, and never again. Writes go to the keychain first
//! and only mutate the cache on success.
//!
//! Pre-release note (`riir`): this is a fresh keychain scheme. The old
//! per-variable / `__names__` layout and the `pluely.license`
//! `selected_model` entry are intentionally orphaned — there is no
//! migration. The user re-enters API keys / re-selects a model once.

use std::collections::HashMap;

use keyring::Entry;
use tokio::sync::Mutex;

use crate::llm::LlmError;

const SVC_PROVIDER_PREFIX: &str = "pluely.provider.";
const ACCT_SECRETS: &str = "secrets";

pub struct Secrets {
    /// provider_id -> {name -> value}; key presence == loaded from keychain.
    cache: Mutex<HashMap<String, HashMap<String, String>>>,
}

impl Secrets {
    pub fn new() -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// All secrets for a provider, loading from the keychain on first miss.
    pub async fn provider(&self, id: &str) -> Result<HashMap<String, String>, LlmError> {
        let mut cache = self.cache.lock().await;
        if let Some(map) = cache.get(id) {
            return Ok(map.clone());
        }
        let id_owned = id.to_string();
        let map = tokio::task::spawn_blocking(move || load_blocking(&id_owned))
            .await
            .expect("secrets load spawn_blocking join")?;
        cache.insert(id.to_string(), map.clone());
        Ok(map)
    }

    pub async fn set(&self, id: &str, name: &str, value: &str) -> Result<(), LlmError> {
        let mut cache = self.cache.lock().await;
        let mut map = ensure_loaded(&mut cache, id).await?;
        map.insert(name.to_string(), value.to_string());
        store(id, &map).await?;
        cache.insert(id.to_string(), map);
        Ok(())
    }

    pub async fn delete(&self, id: &str, name: &str) -> Result<(), LlmError> {
        let mut cache = self.cache.lock().await;
        let mut map = ensure_loaded(&mut cache, id).await?;
        map.remove(name);
        store(id, &map).await?;
        cache.insert(id.to_string(), map);
        Ok(())
    }

    /// Drop every secret for a provider. Does not load first, so it doubles
    /// as an escape hatch when the stored JSON is corrupt.
    pub async fn delete_all(&self, id: &str) -> Result<(), LlmError> {
        let mut cache = self.cache.lock().await;
        store(id, &HashMap::new()).await?;
        cache.insert(id.to_string(), HashMap::new());
        Ok(())
    }
}

/// Load into `cache[id]` if absent and return a clone of the loaded map.
/// The caller holds the cache lock across the keychain access so concurrent
/// read-modify-write is serialized (desirable per keyring docs).
async fn ensure_loaded(
    cache: &mut HashMap<String, HashMap<String, String>>,
    id: &str,
) -> Result<HashMap<String, String>, LlmError> {
    if let Some(map) = cache.get(id) {
        return Ok(map.clone());
    }
    let id_owned = id.to_string();
    let map = tokio::task::spawn_blocking(move || load_blocking(&id_owned))
        .await
        .expect("secrets load spawn_blocking join")?;
    cache.insert(id.to_string(), map.clone());
    Ok(map)
}

async fn store(id: &str, map: &HashMap<String, String>) -> Result<(), LlmError> {
    let id_owned = id.to_string();
    let map_owned = map.clone();
    tokio::task::spawn_blocking(move || store_blocking(&id_owned, &map_owned))
        .await
        .expect("secrets store spawn_blocking join")
}

fn provider_service(id: &str) -> String {
    format!("{}{}", SVC_PROVIDER_PREFIX, id)
}

fn load_blocking(id: &str) -> Result<HashMap<String, String>, LlmError> {
    let entry = Entry::new(&provider_service(id), ACCT_SECRETS)?;
    match entry.get_password() {
        Ok(json) => Ok(serde_json::from_str(&json)?),
        Err(keyring::Error::NoEntry) => Ok(HashMap::new()),
        Err(e) => Err(LlmError::Keychain(e)),
    }
}

fn store_blocking(id: &str, map: &HashMap<String, String>) -> Result<(), LlmError> {
    let entry = Entry::new(&provider_service(id), ACCT_SECRETS)?;
    if map.is_empty() {
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(LlmError::Keychain(e)),
        }
    } else {
        entry.set_password(&serde_json::to_string(map)?)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex as StdMutex, OnceLock};

    use keyring::credential::{CredentialApi, CredentialBuilderApi, CredentialPersistence};
    use keyring::Error as KeyringError;

    // The keyring `mock` builder hands out a fresh, store-less credential per
    // `Entry::new`, so it can't model persistence across entries. We install
    // one shared map-backed store (the builder is process-global) and count
    // get/set/delete calls per *service*, so tests running in parallel — each
    // with a unique provider id — observe only their own counter deltas.
    type Counts = (usize, usize, usize); // (get, set, delete)
    type Counters = Arc<StdMutex<HashMap<String, Counts>>>;
    type Store = Arc<StdMutex<HashMap<(String, String), String>>>;

    fn bump(counters: &Counters, service: &str, which: usize) {
        let mut c = counters.lock().unwrap();
        let e = c.entry(service.to_string()).or_insert((0, 0, 0));
        match which {
            0 => e.0 += 1,
            1 => e.1 += 1,
            _ => e.2 += 1,
        }
    }

    struct CountingStore {
        data: Store,
        counters: Counters,
    }

    struct CountingCred {
        service: String,
        account: String,
        data: Store,
        counters: Counters,
    }

    impl CredentialApi for CountingCred {
        fn set_secret(&self, secret: &[u8]) -> keyring::Result<()> {
            bump(&self.counters, &self.service, 1);
            let s = String::from_utf8(secret.to_vec())
                .map_err(|e| KeyringError::BadEncoding(e.into_bytes()))?;
            self.data
                .lock()
                .unwrap()
                .insert((self.service.clone(), self.account.clone()), s);
            Ok(())
        }

        fn get_secret(&self) -> keyring::Result<Vec<u8>> {
            bump(&self.counters, &self.service, 0);
            match self
                .data
                .lock()
                .unwrap()
                .get(&(self.service.clone(), self.account.clone()))
            {
                Some(v) => Ok(v.clone().into_bytes()),
                None => Err(KeyringError::NoEntry),
            }
        }

        fn delete_credential(&self) -> keyring::Result<()> {
            bump(&self.counters, &self.service, 2);
            match self
                .data
                .lock()
                .unwrap()
                .remove(&(self.service.clone(), self.account.clone()))
            {
                Some(_) => Ok(()),
                None => Err(KeyringError::NoEntry),
            }
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    impl CredentialBuilderApi for CountingStore {
        fn build(
            &self,
            _target: Option<&str>,
            service: &str,
            user: &str,
        ) -> keyring::Result<Box<keyring::credential::Credential>> {
            Ok(Box::new(CountingCred {
                service: service.to_string(),
                account: user.to_string(),
                data: Arc::clone(&self.data),
                counters: Arc::clone(&self.counters),
            }))
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        fn persistence(&self) -> CredentialPersistence {
            CredentialPersistence::UntilDelete
        }
    }

    static STORE: OnceLock<(Store, Counters)> = OnceLock::new();

    fn install() -> (Store, Counters) {
        STORE
            .get_or_init(|| {
                let data: Store = Arc::new(StdMutex::new(HashMap::new()));
                let counters: Counters = Arc::new(StdMutex::new(HashMap::new()));
                keyring::set_default_credential_builder(Box::new(CountingStore {
                    data: Arc::clone(&data),
                    counters: Arc::clone(&counters),
                }));
                (data, counters)
            })
            .clone()
    }

    fn snapshot(counters: &Counters, id: &str) -> Counts {
        counters
            .lock()
            .unwrap()
            .get(&provider_service(id))
            .copied()
            .unwrap_or((0, 0, 0))
    }

    fn delta(counters: &Counters, id: &str, before: Counts) -> Counts {
        let now = snapshot(counters, id);
        (now.0 - before.0, now.1 - before.1, now.2 - before.2)
    }

    #[tokio::test]
    async fn cache_contract() {
        let (data, counters) = install();
        let id = "cache_contract";

        let secrets = Secrets::new();
        let before = snapshot(&counters, id);
        secrets.set(id, "API_KEY", "k").await.unwrap();
        // set: load-on-miss (1 get) + store (1 set).
        assert_eq!(delta(&counters, id, before), (1, 1, 0));

        // Already cached -> no further keychain ops.
        let before = snapshot(&counters, id);
        let map = secrets.provider(id).await.unwrap();
        assert_eq!(map.get("API_KEY").map(String::as_str), Some("k"));
        assert_eq!(delta(&counters, id, before), (0, 0, 0));

        // Stored JSON is the {name: value} object under (service, "secrets").
        let stored = data
            .lock()
            .unwrap()
            .get(&(provider_service(id), ACCT_SECRETS.to_string()))
            .cloned()
            .unwrap();
        assert_eq!(stored, r#"{"API_KEY":"k"}"#);

        // Fresh instance: first provider() loads (1 get), second is cached.
        let fresh = Secrets::new();
        let before = snapshot(&counters, id);
        let map = fresh.provider(id).await.unwrap();
        assert_eq!(map.get("API_KEY").map(String::as_str), Some("k"));
        assert_eq!(delta(&counters, id, before), (1, 0, 0));
        let before = snapshot(&counters, id);
        fresh.provider(id).await.unwrap();
        assert_eq!(delta(&counters, id, before), (0, 0, 0));
    }

    #[tokio::test]
    async fn delete_last_removes_entry() {
        let (data, counters) = install();
        let id = "delete_last";

        let secrets = Secrets::new();
        secrets.set(id, "ONLY", "v").await.unwrap();
        secrets.delete(id, "ONLY").await.unwrap();

        // Empty map -> the keychain entry is deleted, not left as `{}`.
        assert!(data
            .lock()
            .unwrap()
            .get(&(provider_service(id), ACCT_SECRETS.to_string()))
            .is_none());

        let before = snapshot(&counters, id);
        let map = secrets.provider(id).await.unwrap();
        assert!(map.is_empty());
        assert_eq!(delta(&counters, id, before), (0, 0, 0));
    }

    #[tokio::test]
    async fn delete_all_without_load() {
        let (data, counters) = install();
        let id = "delete_all_no_load";

        // Seed directly so `delete_all` has something to remove.
        data.lock().unwrap().insert(
            (provider_service(id), ACCT_SECRETS.to_string()),
            r#"{"A":"1"}"#.to_string(),
        );

        let secrets = Secrets::new();
        let before = snapshot(&counters, id);
        secrets.delete_all(id).await.unwrap();
        // No gets: delete_all never loads.
        assert_eq!(delta(&counters, id, before).0, 0);
        assert!(data
            .lock()
            .unwrap()
            .get(&(provider_service(id), ACCT_SECRETS.to_string()))
            .is_none());
    }

    #[tokio::test]
    async fn multi_name_sorted() {
        let _ = install();
        let id = "multi_name";

        let secrets = Secrets::new();
        secrets.set(id, "B_KEY", "2").await.unwrap();
        secrets.set(id, "A_KEY", "1").await.unwrap();

        let map = secrets.provider(id).await.unwrap();
        assert_eq!(map.get("A_KEY").map(String::as_str), Some("1"));
        assert_eq!(map.get("B_KEY").map(String::as_str), Some("2"));

        let mut names: Vec<String> = map.into_keys().collect();
        names.sort();
        assert_eq!(names, vec!["A_KEY".to_string(), "B_KEY".to_string()]);
    }
}
