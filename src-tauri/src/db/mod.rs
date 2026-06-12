use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use serde::{Serialize, Serializer};
use thiserror::Error;

pub mod commands;
pub mod migrations;
pub mod queries;
pub mod schema;

#[derive(Debug, Error)]
pub enum DbError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("conversation not found: {0}")]
    ConversationNotFound(String),

    #[error("system prompt not found: id={0}")]
    SystemPromptNotFound(i64),

    #[error("invalid input: {0}")]
    InvalidInput(&'static str),

    #[error("attached_files JSON: {0}")]
    AttachedFilesJson(#[from] serde_json::Error),
}

impl Serialize for DbError {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

/// SQLite connection holder. Single connection guarded by a mutex; commands
/// run their queries inside `tokio::task::spawn_blocking` so the async runtime
/// stays free. Mutex poisoning panics by design (fail fast — see CLAUDE.md).
pub struct Db {
    inner: Arc<Mutex<Connection>>,
}

impl Db {
    /// Open (or create) the database at `path`, ensure parent dirs exist,
    /// enable foreign keys, and run any pending migrations.
    pub fn open(path: PathBuf) -> Result<Self, DbError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create app_config_dir");
        }
        tracing::info!(target = "pluely::db", "opening sqlite at {}", path.display());
        let mut conn = Connection::open(&path)?;
        migrations::run_migrations(&mut conn)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(conn)),
        })
    }

    /// Run `f` against the single connection inside `spawn_blocking` so the
    /// async runtime stays free. Mutex poisoning panics by design (fail fast
    /// — see CLAUDE.md).
    pub(crate) async fn with_conn<F, T>(&self, f: F) -> Result<T, DbError>
    where
        F: FnOnce(&mut Connection) -> Result<T, DbError> + Send + 'static,
        T: Send + 'static,
    {
        let arc = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let mut guard = arc.lock().expect("db mutex poisoned");
            f(&mut guard)
        })
        .await
        .expect("db spawn_blocking join")
    }
}
