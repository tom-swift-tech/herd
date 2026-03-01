use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::backend::BackendPool;

#[derive(Debug, Deserialize)]
pub struct AddBackend {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub priority: u32,
    pub gpu_hot_url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RemoveBackend {
    pub name: String,
}

#[derive(Debug, Serialize)]
pub struct AdminResponse {
    pub success: bool,
    pub message: String,
}

pub async fn add_backend(
    State(_pool): State<Arc<BackendPool>>,
    Json(_payload): Json<AddBackend>,
) -> Result<Json<AdminResponse>, StatusCode> {
    // TODO: Implement dynamic backend addition
    // For now, backends are configured via YAML
    Ok(Json(AdminResponse {
        success: true,
        message: "Backend addition requires restart. Use YAML config.".to_string(),
    }))
}

pub async fn remove_backend(
    State(_pool): State<Arc<BackendPool>>,
    Json(_payload): Json<RemoveBackend>,
) -> Result<Json<AdminResponse>, StatusCode> {
    // TODO: Implement dynamic backend removal
    // For now, backends are configured via YAML
    Ok(Json(AdminResponse {
        success: true,
        message: "Backend removal requires restart. Use YAML config.".to_string(),
    }))
}

pub async fn drain_backend(
    State(_pool): State<Arc<BackendPool>>,
    Json(_payload): Json<RemoveBackend>,
) -> Result<Json<AdminResponse>, StatusCode> {
    // TODO: Implement connection draining
    // For now, just mark as unhealthy
    Ok(Json(AdminResponse {
        success: true,
        message: "Drain not yet implemented. Mark backend as unhealthy instead.".to_string(),
    }))
}