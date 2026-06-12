use rusqlite::{params, Connection, OptionalExtension};
use uuid::Uuid;

use super::schema::{
    AppendedMessage, AttachedFile, Conversation, ConversationId, ConversationSummary, Message,
    NewMessage, Role, SystemPrompt,
};
use super::DbError;

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock pre-1970")
        .as_millis() as i64
}

fn decode_attached_files(raw: Option<String>) -> Result<Option<Vec<AttachedFile>>, DbError> {
    match raw {
        None => Ok(None),
        Some(s) if s.is_empty() => Ok(None),
        Some(s) => Ok(Some(serde_json::from_str(&s)?)),
    }
}

fn encode_attached_files(files: &Option<Vec<AttachedFile>>) -> Result<Option<String>, DbError> {
    match files {
        None => Ok(None),
        Some(v) if v.is_empty() => Ok(None),
        Some(v) => Ok(Some(serde_json::to_string(v)?)),
    }
}

// -- conversations -------------------------------------------------------

pub fn list_conversation_summaries(
    conn: &Connection,
) -> Result<Vec<ConversationSummary>, DbError> {
    let mut stmt = conn.prepare(
        "SELECT c.id, c.title, c.created_at, c.updated_at, \
                COALESCE(COUNT(m.id), 0) AS message_count \
         FROM conversations c \
         LEFT JOIN messages m ON m.conversation_id = c.id \
         GROUP BY c.id \
         ORDER BY c.updated_at DESC",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(ConversationSummary {
                id: r.get(0)?,
                title: r.get(1)?,
                created_at: r.get(2)?,
                updated_at: r.get(3)?,
                message_count: r.get(4)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn load_conversation(conn: &Connection, id: &str) -> Result<Conversation, DbError> {
    let conv: Option<(String, String, i64, i64)> = conn
        .query_row(
            "SELECT id, title, created_at, updated_at FROM conversations WHERE id = ?",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let (id, title, created_at, updated_at) =
        conv.ok_or_else(|| DbError::ConversationNotFound(id.to_string()))?;

    let mut stmt = conn.prepare(
        "SELECT id, role, content, timestamp, attached_files \
         FROM messages WHERE conversation_id = ? ORDER BY timestamp ASC",
    )?;
    let messages = stmt
        .query_map(params![id], |r| {
            let role_str: String = r.get(1)?;
            let attached: Option<String> = r.get(4)?;
            Ok((
                r.get::<_, String>(0)?,
                role_str,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                attached,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut out = Vec::with_capacity(messages.len());
    for (mid, role_str, content, timestamp, attached) in messages {
        let role = Role::from_str(&role_str)
            .ok_or(DbError::InvalidInput("messages.role out of range"))?;
        out.push(Message {
            id: mid,
            role,
            content,
            timestamp,
            attached_files: decode_attached_files(attached)?,
        });
    }

    Ok(Conversation {
        id,
        title,
        created_at,
        updated_at,
        messages: out,
    })
}

pub fn start_conversation(conn: &Connection, title: &str) -> Result<ConversationId, DbError> {
    let title = title.trim();
    if title.is_empty() {
        return Err(DbError::InvalidInput("conversation title is empty"));
    }
    let id = Uuid::new_v4().to_string();
    let now = now_ms();
    conn.execute(
        "INSERT INTO conversations (id, title, created_at, updated_at) VALUES (?, ?, ?, ?)",
        params![id, title, now, now],
    )?;
    Ok(ConversationId {
        id,
        created_at: now,
    })
}

pub fn append_message(
    conn: &Connection,
    conversation_id: &str,
    msg: &NewMessage,
) -> Result<AppendedMessage, DbError> {
    // Ensure conversation exists; surface a clean error instead of an FK violation.
    let exists: i64 = conn.query_row(
        "SELECT COUNT(*) FROM conversations WHERE id = ?",
        params![conversation_id],
        |r| r.get(0),
    )?;
    if exists == 0 {
        return Err(DbError::ConversationNotFound(conversation_id.to_string()));
    }

    // Server-side monotonic timestamp: strictly greater than any existing
    // message in the conversation, so ASC-by-timestamp ordering is stable
    // even when multiple appends land in the same millisecond.
    let prev_max: Option<i64> = conn.query_row(
        "SELECT MAX(timestamp) FROM messages WHERE conversation_id = ?",
        params![conversation_id],
        |r| r.get(0),
    )?;
    let mut ts = now_ms();
    if let Some(p) = prev_max {
        if ts <= p {
            ts = p + 1;
        }
    }

    let id = Uuid::new_v4().to_string();
    let attached = encode_attached_files(&msg.attached_files)?;
    conn.execute(
        "INSERT INTO messages (id, conversation_id, role, content, timestamp, attached_files) \
         VALUES (?, ?, ?, ?, ?, ?)",
        params![id, conversation_id, msg.role.as_str(), msg.content, ts, attached],
    )?;
    Ok(AppendedMessage { id, timestamp: ts })
}

pub fn rename_conversation(conn: &Connection, id: &str, title: &str) -> Result<(), DbError> {
    let title = title.trim();
    if title.is_empty() {
        return Err(DbError::InvalidInput("conversation title is empty"));
    }
    let n = conn.execute(
        "UPDATE conversations SET title = ? WHERE id = ?",
        params![title, id],
    )?;
    if n == 0 {
        return Err(DbError::ConversationNotFound(id.to_string()));
    }
    Ok(())
}

pub fn delete_conversation(conn: &Connection, id: &str) -> Result<(), DbError> {
    // ON DELETE CASCADE handles messages, but the schema relies on
    // foreign_keys=ON, which we set in run_migrations and re-set per
    // connection if needed.
    let n = conn.execute("DELETE FROM conversations WHERE id = ?", params![id])?;
    if n == 0 {
        return Err(DbError::ConversationNotFound(id.to_string()));
    }
    Ok(())
}

pub fn delete_all_conversations(conn: &mut Connection) -> Result<(), DbError> {
    let tx = conn.transaction()?;
    tx.execute_batch("DELETE FROM messages; DELETE FROM conversations;")?;
    tx.commit()?;
    Ok(())
}

// -- system prompts ------------------------------------------------------

pub fn list_system_prompts(conn: &Connection) -> Result<Vec<SystemPrompt>, DbError> {
    let mut stmt = conn.prepare(
        "SELECT id, name, prompt, created_at, updated_at FROM system_prompts ORDER BY created_at DESC",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(SystemPrompt {
                id: r.get(0)?,
                name: r.get(1)?,
                prompt: r.get(2)?,
                created_at: r.get(3)?,
                updated_at: r.get(4)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn create_system_prompt(
    conn: &Connection,
    name: &str,
    prompt: &str,
) -> Result<SystemPrompt, DbError> {
    let name = name.trim();
    let prompt = prompt.trim();
    if name.is_empty() {
        return Err(DbError::InvalidInput("system prompt name is empty"));
    }
    if prompt.is_empty() {
        return Err(DbError::InvalidInput("system prompt text is empty"));
    }
    conn.execute(
        "INSERT INTO system_prompts (name, prompt) VALUES (?, ?)",
        params![name, prompt],
    )?;
    let id = conn.last_insert_rowid();
    fetch_system_prompt(conn, id)
}

pub fn edit_system_prompt(
    conn: &Connection,
    id: i64,
    name: Option<&str>,
    prompt: Option<&str>,
) -> Result<SystemPrompt, DbError> {
    let name = match name {
        Some(s) => {
            let s = s.trim();
            if s.is_empty() {
                return Err(DbError::InvalidInput("system prompt name is empty"));
            }
            Some(s.to_string())
        }
        None => None,
    };
    let prompt = match prompt {
        Some(s) => {
            let s = s.trim();
            if s.is_empty() {
                return Err(DbError::InvalidInput("system prompt text is empty"));
            }
            Some(s.to_string())
        }
        None => None,
    };

    if name.is_none() && prompt.is_none() {
        // Nothing to do; surface as input error rather than no-op.
        return Err(DbError::InvalidInput("no fields to update"));
    }

    let n = conn.execute(
        "UPDATE system_prompts \
         SET name = COALESCE(?, name), prompt = COALESCE(?, prompt) \
         WHERE id = ?",
        params![name, prompt, id],
    )?;
    if n == 0 {
        return Err(DbError::SystemPromptNotFound(id));
    }
    fetch_system_prompt(conn, id)
}

pub fn delete_system_prompt(conn: &Connection, id: i64) -> Result<(), DbError> {
    let n = conn.execute(
        "DELETE FROM system_prompts WHERE id = ?",
        params![id],
    )?;
    if n == 0 {
        return Err(DbError::SystemPromptNotFound(id));
    }
    Ok(())
}

// -- settings ------------------------------------------------------------

pub fn setting_get(conn: &Connection, key: &str) -> Result<Option<String>, DbError> {
    conn.query_row(
        "SELECT value FROM settings WHERE key = ?",
        params![key],
        |r| r.get(0),
    )
    .optional()
    .map_err(DbError::from)
}

pub fn setting_set(conn: &Connection, key: &str, value: &str) -> Result<(), DbError> {
    conn.execute(
        "INSERT INTO settings (key, value) VALUES (?, ?) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

fn fetch_system_prompt(conn: &Connection, id: i64) -> Result<SystemPrompt, DbError> {
    conn.query_row(
        "SELECT id, name, prompt, created_at, updated_at FROM system_prompts WHERE id = ?",
        params![id],
        |r| {
            Ok(SystemPrompt {
                id: r.get(0)?,
                name: r.get(1)?,
                prompt: r.get(2)?,
                created_at: r.get(3)?,
                updated_at: r.get(4)?,
            })
        },
    )
    .optional()?
    .ok_or(DbError::SystemPromptNotFound(id))
}

// -- tests ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations::run_migrations;

    fn fresh() -> Connection {
        let mut conn = Connection::open_in_memory().expect("open_in_memory");
        run_migrations(&mut conn).expect("migrations");
        conn
    }

    #[test]
    fn start_then_append_then_load() {
        let conn = fresh();
        let cid = start_conversation(&conn, "hello there").unwrap();
        let f = AttachedFile {
            id: "f1".into(),
            name: "a.png".into(),
            mime: "image/png".into(),
            base64: "AAAA".into(),
            size: 4,
        };
        let m1 = append_message(
            &conn,
            &cid.id,
            &NewMessage {
                role: Role::User,
                content: "hi".into(),
                attached_files: Some(vec![f.clone()]),
            },
        )
        .unwrap();
        let m2 = append_message(
            &conn,
            &cid.id,
            &NewMessage {
                role: Role::Assistant,
                content: "hello!".into(),
                attached_files: None,
            },
        )
        .unwrap();

        let conv = load_conversation(&conn, &cid.id).unwrap();
        assert_eq!(conv.id, cid.id);
        assert_eq!(conv.title, "hello there");
        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.messages[0].id, m1.id);
        assert_eq!(conv.messages[0].role, Role::User);
        assert_eq!(conv.messages[0].timestamp, m1.timestamp);
        let af = conv.messages[0].attached_files.as_ref().unwrap();
        assert_eq!(af.len(), 1);
        assert_eq!(af[0].id, "f1");
        assert_eq!(af[0].mime, "image/png");
        assert_eq!(conv.messages[1].id, m2.id);
        assert_eq!(conv.messages[1].role, Role::Assistant);
        assert!(conv.messages[1].attached_files.is_none());
    }

    #[test]
    fn list_summaries_orders_by_updated_at_desc() {
        let conn = fresh();
        let a = start_conversation(&conn, "a").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = start_conversation(&conn, "b").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let c = start_conversation(&conn, "c").unwrap();

        // Append to b -> should jump to top.
        std::thread::sleep(std::time::Duration::from_millis(2));
        append_message(
            &conn,
            &b.id,
            &NewMessage {
                role: Role::User,
                content: "bump".into(),
                attached_files: None,
            },
        )
        .unwrap();

        let s = list_conversation_summaries(&conn).unwrap();
        assert_eq!(s.len(), 3);
        assert_eq!(s[0].id, b.id);
        assert_eq!(s[0].message_count, 1);
        assert_eq!(s[1].id, c.id);
        assert_eq!(s[2].id, a.id);
    }

    #[test]
    fn append_assigns_monotonic_timestamp() {
        let conn = fresh();
        let cid = start_conversation(&conn, "x").unwrap();
        let m1 = append_message(
            &conn,
            &cid.id,
            &NewMessage {
                role: Role::User,
                content: "1".into(),
                attached_files: None,
            },
        )
        .unwrap();
        let m2 = append_message(
            &conn,
            &cid.id,
            &NewMessage {
                role: Role::Assistant,
                content: "2".into(),
                attached_files: None,
            },
        )
        .unwrap();
        assert!(m2.timestamp > m1.timestamp);
    }

    #[test]
    fn delete_cascades_messages() {
        let conn = fresh();
        let cid = start_conversation(&conn, "x").unwrap();
        append_message(
            &conn,
            &cid.id,
            &NewMessage {
                role: Role::User,
                content: "1".into(),
                attached_files: None,
            },
        )
        .unwrap();
        delete_conversation(&conn, &cid.id).unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE conversation_id = ?",
                params![cid.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn rename_unknown_errors() {
        let conn = fresh();
        let err = rename_conversation(&conn, "no-such-id", "anything").unwrap_err();
        assert!(matches!(err, DbError::ConversationNotFound(_)));
    }

    #[test]
    fn edit_system_prompt_partial() {
        let conn = fresh();
        let p = create_system_prompt(&conn, "n1", "p1").unwrap();

        // name only
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let p2 = edit_system_prompt(&conn, p.id, Some("n2"), None).unwrap();
        assert_eq!(p2.name, "n2");
        assert_eq!(p2.prompt, "p1");
        assert_ne!(p2.updated_at, p.updated_at);

        // prompt only
        let p3 = edit_system_prompt(&conn, p.id, None, Some("p3")).unwrap();
        assert_eq!(p3.name, "n2");
        assert_eq!(p3.prompt, "p3");

        // both
        let p4 = edit_system_prompt(&conn, p.id, Some("n4"), Some("p4")).unwrap();
        assert_eq!(p4.name, "n4");
        assert_eq!(p4.prompt, "p4");
    }

    #[test]
    fn setting_get_set_upsert() {
        let conn = fresh();
        assert_eq!(setting_get(&conn, "k").unwrap(), None);

        setting_set(&conn, "k", "v1").unwrap();
        assert_eq!(setting_get(&conn, "k").unwrap(), Some("v1".to_string()));

        // Upsert overwrites in place (no duplicate-key error).
        setting_set(&conn, "k", "v2").unwrap();
        assert_eq!(setting_get(&conn, "k").unwrap(), Some("v2".to_string()));
    }

    #[test]
    fn legacy_bridge() {
        let mut conn = Connection::open_in_memory().unwrap();
        // Simulate tauri-plugin-sql shape: schema present, _sqlx_migrations
        // table present, user_version=0.
        conn.execute_batch(include_str!("migrations/system-prompts.sql"))
            .unwrap();
        conn.execute_batch(include_str!("migrations/chat-history.sql"))
            .unwrap();
        conn.execute_batch(
            "CREATE TABLE _sqlx_migrations (version INTEGER PRIMARY KEY, description TEXT);",
        )
        .unwrap();
        conn.execute_batch("INSERT INTO _sqlx_migrations (version, description) VALUES (1, 'x');")
            .unwrap();
        // Pre-existing data we want to keep.
        conn.execute(
            "INSERT INTO conversations (id, title, created_at, updated_at) VALUES (?, ?, ?, ?)",
            params!["pre-id", "pre", 1i64, 1i64],
        )
        .unwrap();

        assert_eq!(
            conn.query_row::<i64, _, _>("PRAGMA user_version;", [], |r| r.get(0))
                .unwrap(),
            0
        );

        run_migrations(&mut conn).unwrap();

        // Legacy stamp (v2) then fall through, so the v3 `settings` migration
        // still runs: user_version lands at 3 and `settings` exists.
        let v: i64 = conn
            .query_row("PRAGMA user_version;", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 3);

        let settings_tables: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='settings'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(settings_tables, 1);

        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM conversations WHERE id='pre-id'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n, 1);
    }
}
