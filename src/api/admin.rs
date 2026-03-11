use crate::config::Backend;
use crate::server::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct AddBackendRequest {
    pub name: String,
    pub url: String,
    #[serde(default = "default_priority")]
    pub priority: u32,
    #[serde(default)]
    pub default_model: Option<String>,
    #[serde(default)]
    pub model_filter: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

fn default_priority() -> u32 {
    50
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UpdateBackendRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_filter: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Serialize)]
pub struct BackendResponse {
    pub name: String,
    pub url: String,
    pub priority: u32,
    pub default_model: Option<String>,
    pub model_filter: Option<String>,
    pub tags: Vec<String>,
    pub healthy: bool,
    pub current_model: Option<String>,
    pub model_count: usize,
    pub idle_seconds: u64,
    pub gpu: Option<GpuResponse>,
    pub vram_total_mb: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct GpuResponse {
    pub utilization: f32,
    pub memory_used: u64,
    pub memory_total: u64,
    pub temperature: f32,
}

fn backend_to_response(b: &crate::backend::BackendState) -> BackendResponse {
    BackendResponse {
        name: b.config.name.clone(),
        url: b.config.url.clone(),
        priority: b.config.priority,
        default_model: b.config.default_model.clone(),
        model_filter: b.config.model_filter.clone(),
        tags: b.config.tags.clone(),
        healthy: b.healthy,
        current_model: b.current_model.clone(),
        model_count: b.models.len(),
        idle_seconds: b.last_request.elapsed().as_secs(),
        gpu: b.gpu_metrics.as_ref().map(|g| GpuResponse {
            utilization: g.utilization,
            memory_used: g.memory_used,
            memory_total: g.memory_total,
            temperature: g.temperature,
        }),
        vram_total_mb: b.vram_total_mb,
    }
}

pub async fn list_backends(State(state): State<AppState>) -> Json<Vec<BackendResponse>> {
    let backends = state.pool.all().await;
    let mut response = Vec::new();

    for name in backends {
        if let Some(b) = state.pool.get(&name).await {
            response.push(backend_to_response(&b));
        }
    }

    Json(response)
}

pub async fn get_backend(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<BackendResponse>, StatusCode> {
    let backend = state.pool.get(&name).await.ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(backend_to_response(&backend)))
}

pub async fn add_backend(
    State(state): State<AppState>,
    Json(req): Json<AddBackendRequest>,
) -> Result<Json<BackendResponse>, (StatusCode, String)> {
    // Check if already exists
    if state.pool.get(&req.name).await.is_some() {
        return Err((
            StatusCode::CONFLICT,
            format!("Backend '{}' already exists", req.name),
        ));
    }

    let backend = Backend {
        name: req.name.clone(),
        url: req.url,
        priority: req.priority,
        default_model: req.default_model,
        gpu_hot_url: None,
        model_filter: req.model_filter,
        health_check_path: None,
        health_check_status: None,
        tags: req.tags,
    };

    state.pool.add(backend).await;

    // Brief pause for health check to pick it up
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    let b = state.pool.get(&req.name).await.ok_or_else(|| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to add backend".to_string(),
        )
    })?;

    tracing::info!("Added backend: {}", req.name);
    Ok(Json(backend_to_response(&b)))
}

pub async fn update_backend(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(req): Json<UpdateBackendRequest>,
) -> Result<Json<BackendResponse>, (StatusCode, String)> {
    let mut backend = state.pool.get(&name).await.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            format!("Backend '{}' not found", name),
        )
    })?;

    if let Some(url) = req.url {
        backend.config.url = url;
    }
    if let Some(priority) = req.priority {
        backend.config.priority = priority;
    }
    if let Some(default_model) = req.default_model {
        backend.config.default_model = Some(default_model);
    }
    if let Some(model_filter) = req.model_filter {
        backend.config.model_filter = Some(model_filter);
    }
    if let Some(tags) = req.tags {
        backend.config.tags = tags;
    }

    state.pool.update(backend.clone()).await;

    tracing::info!("Updated backend: {}", name);
    Ok(Json(backend_to_response(&backend)))
}

pub async fn remove_backend(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, StatusCode> {
    if state.pool.remove(&name).await {
        tracing::info!("Removed backend: {}", name);
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

// ---------------------------------------------------------------------------
// Model management endpoints (proxy to Ollama API on the backend)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct PullModelRequest {
    pub name: String,
}

/// POST /admin/backends/:name/pull — Pull a model on a specific backend.
/// Streams Ollama pull progress as SSE.
pub async fn pull_model(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(req): Json<PullModelRequest>,
) -> Result<axum::response::Response, (StatusCode, String)> {
    let backend = state.pool.get(&name).await.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            format!("Backend '{}' not found", name),
        )
    })?;

    let url = format!("{}/api/pull", backend.config.url.trim_end_matches('/'));
    tracing::info!("Pulling model '{}' on backend '{}'", req.name, name);

    // Stream the pull response from Ollama
    let resp = state
        .client
        .post(&url)
        .json(&serde_json::json!({"name": req.name, "stream": true}))
        .timeout(std::time::Duration::from_secs(3600)) // 1 hour for large models
        .send()
        .await
        .map_err(|e| {
            (
                StatusCode::BAD_GATEWAY,
                format!("Failed to reach backend '{}': {}", name, e),
            )
        })?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err((
            StatusCode::BAD_GATEWAY,
            format!("Ollama pull failed ({}): {}", status, body),
        ));
    }

    // Stream Ollama's NDJSON progress through as SSE
    let stream = resp.bytes_stream();
    let body = axum::body::Body::from_stream(stream);

    axum::response::Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/x-ndjson")
        .header("cache-control", "no-cache")
        .body(body)
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to build response: {}", e),
            )
        })
}

/// DELETE /admin/backends/:name/models/:model — Delete a model from a specific backend.
pub async fn delete_model(
    State(state): State<AppState>,
    Path((name, model)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let backend = state.pool.get(&name).await.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            format!("Backend '{}' not found", name),
        )
    })?;

    let url = format!("{}/api/delete", backend.config.url.trim_end_matches('/'));
    tracing::info!("Deleting model '{}' from backend '{}'", model, name);

    let resp = state
        .client
        .delete(&url)
        .json(&serde_json::json!({"name": model}))
        .send()
        .await
        .map_err(|e| {
            (
                StatusCode::BAD_GATEWAY,
                format!("Failed to reach backend '{}': {}", name, e),
            )
        })?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err((
            StatusCode::BAD_GATEWAY,
            format!("Ollama delete failed ({}): {}", status, body),
        ));
    }

    // Refresh model list after deletion
    let _ = resp.text().await;

    tracing::info!("Deleted model '{}' from backend '{}'", model, name);
    Ok(Json(serde_json::json!({
        "status": "deleted",
        "model": model,
        "backend": name,
    })))
}

/// GET /admin/backends/:name/models — List all models on a specific backend.
pub async fn list_backend_models(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let backend = state.pool.get(&name).await.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            format!("Backend '{}' not found", name),
        )
    })?;

    // Fetch fresh model list from Ollama
    let url = format!("{}/api/tags", backend.config.url.trim_end_matches('/'));
    let resp = state
        .client
        .get(&url)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| {
            (
                StatusCode::BAD_GATEWAY,
                format!("Failed to reach backend '{}': {}", name, e),
            )
        })?;

    let data: serde_json::Value = resp.json().await.map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            format!("Invalid response from '{}': {}", name, e),
        )
    })?;

    Ok(Json(serde_json::json!({
        "backend": name,
        "vram_total_mb": backend.vram_total_mb,
        "models": data.get("models").cloned().unwrap_or(serde_json::json!([])),
    })))
}
