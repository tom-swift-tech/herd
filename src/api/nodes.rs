use crate::nodes::{NodeRegistration, NodeRegistrationResponse, NodeUpdate};
use crate::server::{constant_time_eq, AppState};
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::{Deserialize, Serialize};

const SCRIPT_WINDOWS: &str = include_str!("../../scripts/herd-tune.ps1");
const SCRIPT_LINUX: &str = include_str!("../../scripts/herd-tune.sh");

#[derive(Serialize)]
pub struct NodesListResponse {
    pub nodes: Vec<crate::nodes::Node>,
}

#[derive(Deserialize)]
pub struct EnrollmentQuery {
    pub enrollment_key: Option<String>,
}

/// POST /api/nodes/register — called by herd-tune scripts
pub async fn register_node(
    State(state): State<AppState>,
    Query(query): Query<EnrollmentQuery>,
    headers: HeaderMap,
    Json(reg): Json<NodeRegistration>,
) -> Result<(StatusCode, Json<NodeRegistrationResponse>), (StatusCode, Json<serde_json::Value>)> {
    // Check enrollment key if configured
    {
        let config = state.config.read().await;
        if let Some(ref expected) = config.server.enrollment_key {
            let provided = query
                .enrollment_key
                .as_deref()
                .or_else(|| headers.get("x-enrollment-key")?.to_str().ok());
            match provided {
                Some(key) if constant_time_eq(key.as_bytes(), expected.as_bytes()) => {}
                _ => {
                    return Err((
                        StatusCode::UNAUTHORIZED,
                        Json(serde_json::json!({"error": "Invalid or missing enrollment key"})),
                    ));
                }
            }
        }
    }

    let (id, is_new) = state.node_db.upsert_node(&reg).map_err(|e| {
        tracing::error!("Failed to register node: {}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Registration failed: {}", e)})),
        )
    })?;

    let status_code = if is_new {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    let message = if is_new {
        "Node registered successfully. Health polling started.".to_string()
    } else {
        "Node re-registered successfully. Configuration updated.".to_string()
    };

    tracing::info!(
        "Node {} ({}) {} — id={}",
        reg.hostname,
        reg.effective_url(),
        if is_new {
            "registered"
        } else {
            "re-registered"
        },
        id
    );

    Ok((
        status_code,
        Json(NodeRegistrationResponse {
            id,
            hostname: reg.hostname,
            status: "registered".to_string(),
            message,
        }),
    ))
}

/// GET /api/nodes — list all registered nodes
pub async fn list_nodes(
    State(state): State<AppState>,
) -> Result<Json<NodesListResponse>, (StatusCode, Json<serde_json::Value>)> {
    let nodes = state.node_db.list_nodes().map_err(|e| {
        tracing::error!("Failed to list nodes: {}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Failed to list nodes: {}", e)})),
        )
    })?;
    Ok(Json(NodesListResponse { nodes }))
}

/// GET /api/nodes/:id — get a single node
pub async fn get_node(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<crate::nodes::Node>, (StatusCode, Json<serde_json::Value>)> {
    match state.node_db.get_node(&id) {
        Ok(Some(node)) => Ok(Json(node)),
        Ok(None) => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Node not found"})),
        )),
        Err(e) => {
            tracing::error!("Failed to get node: {}", e);
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("Failed to get node: {}", e)})),
            ))
        }
    }
}

/// Check api_key from request headers against config. Returns Ok if no key
/// is configured or if the provided key matches.
fn check_api_key(
    headers: &HeaderMap,
    expected: &Option<String>,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    let Some(expected) = expected.as_deref() else {
        return Ok(());
    };
    let provided = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .or_else(|| {
            headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.strip_prefix("Bearer "))
        });
    match provided {
        Some(key) if constant_time_eq(key.as_bytes(), expected.as_bytes()) => Ok(()),
        _ => Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "API key required"})),
        )),
    }
}

/// PUT /api/nodes/:id — update operator-controlled fields
pub async fn update_node(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(update): Json<NodeUpdate>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let config = state.config.read().await;
    check_api_key(&headers, &config.server.api_key)?;
    drop(config);

    match state.node_db.update_node(&id, &update) {
        Ok(true) => Ok(Json(serde_json::json!({"status": "updated"}))),
        Ok(false) => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Node not found"})),
        )),
        Err(e) => {
            tracing::error!("Failed to update node: {}", e);
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("Failed to update node: {}", e)})),
            ))
        }
    }
}

/// DELETE /api/nodes/:id — remove node from fleet
pub async fn delete_node(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let config = state.config.read().await;
    check_api_key(&headers, &config.server.api_key)?;
    drop(config);
    match state.node_db.delete_node(&id) {
        Ok(true) => {
            tracing::info!("Node {} removed from fleet", id);
            Ok(Json(serde_json::json!({"status": "deleted"})))
        }
        Ok(false) => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Node not found"})),
        )),
        Err(e) => {
            tracing::error!("Failed to delete node: {}", e);
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("Failed to delete node: {}", e)})),
            ))
        }
    }
}

/// GET /api/nodes/script?os={windows|linux} — serve herd-tune script with endpoint baked in
pub async fn download_script(
    State(state): State<AppState>,
    req: axum::http::Request<axum::body::Body>,
) -> Result<axum::response::Response, (StatusCode, Json<serde_json::Value>)> {
    // Parse query params from URI
    let query = req.uri().query().unwrap_or("");
    let os_param = query
        .split('&')
        .find_map(|p| p.strip_prefix("os="))
        .unwrap_or("windows");

    let (template, filename, content_type) = match os_param {
        "linux" | "bash" | "sh" => (SCRIPT_LINUX, "herd-tune.sh", "application/x-sh"),
        _ => (SCRIPT_WINDOWS, "herd-tune.ps1", "application/octet-stream"),
    };

    // Determine public URL: prefer explicit env var, fall back to Host header
    let public_url = std::env::var("HERD_PUBLIC_URL").ok().unwrap_or_else(|| {
        let host = req
            .headers()
            .get("host")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("localhost:40114");
        format!("http://{}", host)
    });

    let config = state.config.read().await;
    let enrollment_key = config.server.enrollment_key.clone().unwrap_or_default();
    drop(config);

    let script = template
        .replace("%%HERD_ENDPOINT%%", public_url.trim_end_matches('/'))
        .replace("%%ENROLLMENT_KEY%%", &enrollment_key);

    let response = axum::response::Response::builder()
        .status(StatusCode::OK)
        .header("content-type", content_type)
        .header(
            "content-disposition",
            format!("attachment; filename=\"{}\"", filename),
        )
        .body(axum::body::Body::from(script))
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
        })?;

    Ok(response)
}
