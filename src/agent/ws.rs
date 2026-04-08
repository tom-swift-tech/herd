use crate::agent::executor::AgentExecutor;
use crate::agent::permissions::PermissionEngine;
use crate::server::AppState;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;

#[derive(Debug, Deserialize)]
struct WsMessage {
    content: String,
}

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Response, StatusCode> {
    // Authenticate via query parameter (WebSocket can't use custom headers during upgrade)
    {
        let config = state.config.read().await;
        let expected = config
            .server
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or(StatusCode::FORBIDDEN)?;

        let provided = params.get("api_key").ok_or(StatusCode::UNAUTHORIZED)?;
        if !crate::server::constant_time_eq(expected.as_bytes(), provided.trim().as_bytes()) {
            return Err(StatusCode::UNAUTHORIZED);
        }
    }

    // Verify session exists
    state
        .session_store
        .get(&id)
        .await
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok(ws.on_upgrade(move |socket| handle_ws(socket, state, id)))
}

async fn handle_ws(mut socket: WebSocket, state: AppState, session_id: String) {
    tracing::info!("WebSocket connected for session {}", session_id);

    loop {
        // Wait for a message from the client
        let msg = match socket.recv().await {
            Some(Ok(Message::Text(text))) => text,
            Some(Ok(Message::Close(_))) | None => {
                tracing::debug!("WebSocket closed for session {}", session_id);
                break;
            }
            Some(Ok(_)) => continue, // Ignore binary/ping/pong
            Some(Err(e)) => {
                tracing::warn!("WebSocket error for session {}: {}", session_id, e);
                break;
            }
        };

        // Parse the user's message
        let ws_msg: WsMessage = match serde_json::from_str(&msg) {
            Ok(m) => m,
            Err(e) => {
                let err = serde_json::json!({
                    "type": "error",
                    "session_id": session_id,
                    "error": format!("Invalid message format: {}", e),
                });
                let _ = socket.send(Message::Text(err.to_string())).await;
                continue;
            }
        };

        // Acquire per-session lock to prevent concurrent mutation races
        let _lock = match state.session_store.lock_session(&session_id).await {
            Some(g) => g,
            None => {
                let err = serde_json::json!({
                    "type": "error",
                    "session_id": session_id,
                    "error": "Session not found",
                });
                let _ = socket.send(Message::Text(err.to_string())).await;
                break;
            }
        };

        // Get current session
        let mut session = match state.session_store.get(&session_id).await {
            Some(s) => s,
            None => {
                let err = serde_json::json!({
                    "type": "error",
                    "session_id": session_id,
                    "error": "Session not found",
                });
                let _ = socket.send(Message::Text(err.to_string())).await;
                break;
            }
        };

        // Create executor with event channel
        let (tx, mut rx) = mpsc::channel(64);
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

        // Spawn executor in background so we can forward events
        let content = ws_msg.content;
        let exec_handle = tokio::spawn(async move {
            executor
                .execute_streaming(&mut session, content, tx)
                .await
                .map(|result| (result, session))
        });

        // Forward events to WebSocket and audit log
        while let Some(event) = rx.recv().await {
            state.agent_audit.log_event(&event).await;
            let json = match serde_json::to_string(&event) {
                Ok(j) => j,
                Err(_) => continue,
            };
            if socket.send(Message::Text(json)).await.is_err() {
                tracing::debug!("WebSocket send failed for session {}", session_id);
                return; // Client disconnected
            }
        }

        // Executor finished — get the result and save session
        match exec_handle.await {
            Ok(Ok((_, session))) => {
                state.session_store.update(session).await;
            }
            Ok(Err(e)) => {
                tracing::error!("Agent executor error for session {}: {}", session_id, e);
            }
            Err(e) => {
                tracing::error!("Agent executor panicked for session {}: {}", session_id, e);
            }
        }
    }

    tracing::info!("WebSocket disconnected for session {}", session_id);
}
