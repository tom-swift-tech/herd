use crate::agent::executor::AgentExecutor;
use crate::agent::permissions::PermissionEngine;
use crate::server::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Deserialize)]
pub struct CreateSessionRequest {
    pub model: String,
    #[serde(default)]
    pub system_prompt: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SessionSummary {
    pub id: String,
    pub model: String,
    pub status: String,
    pub message_count: usize,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Serialize)]
pub struct SessionDetail {
    pub id: String,
    pub model: String,
    pub status: String,
    pub messages: Vec<crate::agent::AgentMessage>,
    pub created_at: i64,
    pub updated_at: i64,
}

fn summarize(s: &crate::agent::Session) -> SessionSummary {
    SessionSummary {
        id: s.id.clone(),
        model: s.model.clone(),
        status: s.status.to_string(),
        message_count: s.messages.len(),
        created_at: s.created_at,
        updated_at: s.updated_at,
    }
}

pub async fn create_session(
    State(state): State<AppState>,
    Json(req): Json<CreateSessionRequest>,
) -> Result<(StatusCode, Json<SessionSummary>), (StatusCode, String)> {
    let session = state
        .session_store
        .create(req.model, req.system_prompt)
        .await
        .map_err(|e| (StatusCode::CONFLICT, e))?;

    tracing::info!(
        "Created agent session {} (model: {})",
        session.id,
        session.model
    );
    state
        .agent_audit
        .log_session_created(&session.id, &session.model)
        .await;
    Ok((StatusCode::CREATED, Json(summarize(&session))))
}

pub async fn list_sessions(State(state): State<AppState>) -> Json<Vec<SessionSummary>> {
    let sessions = state.session_store.list().await;
    Json(sessions.iter().map(summarize).collect())
}

pub async fn get_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<SessionDetail>, StatusCode> {
    let session = state
        .session_store
        .get(&id)
        .await
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Json(SessionDetail {
        id: session.id,
        model: session.model,
        status: session.status.to_string(),
        messages: session.messages,
        created_at: session.created_at,
        updated_at: session.updated_at,
    }))
}

pub async fn delete_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    if state.session_store.delete(&id).await {
        tracing::info!("Deleted agent session {}", id);
        state.agent_audit.log_session_deleted(&id).await;
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

#[derive(Debug, Deserialize)]
pub struct SendMessageRequest {
    pub content: String,
}

#[derive(Debug, Serialize)]
pub struct MessageResponse {
    pub content: String,
    pub tool_calls_made: usize,
}

pub async fn send_message(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<SendMessageRequest>,
) -> Result<Json<MessageResponse>, (StatusCode, String)> {
    // Acquire per-session lock to prevent concurrent mutation races
    let _lock = state
        .session_store
        .lock_session(&id)
        .await
        .ok_or((StatusCode::NOT_FOUND, "Session not found".into()))?;

    let mut session = state
        .session_store
        .get(&id)
        .await
        .ok_or((StatusCode::NOT_FOUND, "Session not found".into()))?;

    let message_count_before = session.messages.len();

    let config = state.config.read().await;
    let permissions = PermissionEngine::new(&config.agent.permissions);
    let max_tool_rounds = config.agent.max_tool_rounds;
    drop(config);
    let router = state.router.read().await.clone();
    let executor = AgentExecutor::new(
        Arc::clone(&state.client),
        router,
        permissions,
        max_tool_rounds,
        state.routing_timeout(),
    );

    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let audit = Arc::clone(&state.agent_audit);

    // Drain events to audit log in background
    let audit_handle = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            audit.log_event(&event).await;
        }
    });

    let content = executor
        .execute_streaming(&mut session, req.content, tx)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Wait for audit drain to complete
    let _ = audit_handle.await;

    // Count tool calls made during this execution
    let tool_calls_made = session.messages[message_count_before..]
        .iter()
        .filter(|m| m.role == crate::agent::MessageRole::Tool)
        .count();

    state.session_store.update(session).await;

    Ok(Json(MessageResponse {
        content,
        tool_calls_made,
    }))
}
