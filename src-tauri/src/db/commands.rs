use tauri::State;

use super::queries;
use super::schema::{AppendedTurn, Conversation, ConversationSummary, NewTurn, SystemPrompt};
use super::{Db, DbError};

// -- chat history --------------------------------------------------------

#[tauri::command]
pub async fn list_conversation_summaries(
    db: State<'_, Db>,
) -> Result<Vec<ConversationSummary>, DbError> {
    db.with_conn(|c| queries::list_conversation_summaries(c)).await
}

#[tauri::command]
pub async fn load_conversation(
    db: State<'_, Db>,
    id: String,
) -> Result<Conversation, DbError> {
    db.with_conn(move |c| queries::load_conversation(c, &id)).await
}

#[tauri::command]
pub async fn append_turn(
    db: State<'_, Db>,
    conversation_id: Option<String>,
    turn: NewTurn,
) -> Result<AppendedTurn, DbError> {
    db.with_conn(move |c| queries::append_turn(c, conversation_id.as_deref(), turn)).await
}

#[tauri::command]
pub async fn delete_conversation(db: State<'_, Db>, id: String) -> Result<(), DbError> {
    db.with_conn(move |c| queries::delete_conversation(c, &id)).await
}

#[tauri::command]
pub async fn delete_all_conversations(db: State<'_, Db>) -> Result<(), DbError> {
    db.with_conn(queries::delete_all_conversations).await
}

// -- system prompts ------------------------------------------------------

#[tauri::command]
pub async fn list_system_prompts(db: State<'_, Db>) -> Result<Vec<SystemPrompt>, DbError> {
    db.with_conn(|c| queries::list_system_prompts(c)).await
}

#[tauri::command]
pub async fn create_system_prompt(
    db: State<'_, Db>,
    name: String,
    prompt: String,
) -> Result<SystemPrompt, DbError> {
    db.with_conn(move |c| queries::create_system_prompt(c, &name, &prompt)).await
}

#[tauri::command]
pub async fn edit_system_prompt(
    db: State<'_, Db>,
    id: i64,
    name: Option<String>,
    prompt: Option<String>,
) -> Result<SystemPrompt, DbError> {
    db.with_conn(move |c| {
        queries::edit_system_prompt(c, id, name.as_deref(), prompt.as_deref())
    })
    .await
}

#[tauri::command]
pub async fn delete_system_prompt(db: State<'_, Db>, id: i64) -> Result<(), DbError> {
    db.with_conn(move |c| queries::delete_system_prompt(c, id)).await
}
