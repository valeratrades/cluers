use rusqlite::Connection;

use super::DbError;

struct Migration {
    version: i64,
    sql: &'static str,
}

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        sql: include_str!("migrations/system-prompts.sql"),
    },
    Migration {
        version: 2,
        sql: include_str!("migrations/chat-history.sql"),
    },
    Migration {
        version: 3,
        sql: include_str!("migrations/settings.sql"),
    },
];

/// Highest schema version managed by `tauri-plugin-sql` before we took
/// over migrations. Legacy DBs have these tables already; we stamp this
/// then fall through to the loop so newer migrations still apply.
const LEGACY_STAMP_VERSION: i64 = 2;

/// Runs all pending migrations, tracked via `PRAGMA user_version`.
///
/// On first launch with a database previously managed by `tauri-plugin-sql`
/// (which records its state in `_sqlx_migrations` instead of `user_version`),
/// we detect that table and stamp `user_version` to the last version that
/// plugin managed. The schema for those versions is already there; we set
/// the counter then fall through to the loop so any migration we added on
/// top (v3+) still runs.
pub fn run_migrations(conn: &mut Connection) -> Result<(), DbError> {
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;

    if user_version(conn)? == 0 && has_sqlx_migrations(conn)? {
        tracing::info!(
            target = "pluely::db",
            "detected tauri-plugin-sql legacy schema; stamping user_version={}",
            LEGACY_STAMP_VERSION,
        );
        set_user_version(conn, LEGACY_STAMP_VERSION)?;
    }

    let current = user_version(conn)?;
    for m in MIGRATIONS {
        if m.version > current {
            let tx = conn.transaction()?;
            tx.execute_batch(m.sql)?;
            tx.execute_batch(&format!("PRAGMA user_version = {};", m.version))?;
            tx.commit()?;
        }
    }
    Ok(())
}

fn user_version(conn: &Connection) -> Result<i64, DbError> {
    let v: i64 = conn.query_row("PRAGMA user_version;", [], |r| r.get(0))?;
    Ok(v)
}

fn set_user_version(conn: &Connection, v: i64) -> Result<(), DbError> {
    conn.execute_batch(&format!("PRAGMA user_version = {};", v))?;
    Ok(())
}

fn has_sqlx_migrations(conn: &Connection) -> Result<bool, DbError> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='_sqlx_migrations';",
        [],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}
