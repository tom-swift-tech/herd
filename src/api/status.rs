use crate::backend::{BackendPool, BackendState};
use axum::extract::State;
use axum::Json;
use serde::Serialize;
use std::sync::Arc;

#[derive(Debug, Serialize)]
pub struct Status {
    pub healthy_backends: Vec<BackendStatus>,
    pub unhealthy_backends: Vec<String>,
    pub routing_strategy: String,
}

#[derive(Debug, Serialize)]
pub struct BackendStatus {
    pub name: String,
    pub url: String,
    pub priority: u32,
    pub healthy: bool,
    pub models: Vec<String>,
    pub gpu: Option<GpuStatus>,
}

#[derive(Debug, Serialize)]
pub struct GpuStatus {
    pub utilization: f32,
    pub memory_used: u64,
    pub memory_total: u64,
    pub memory_percent: f32,
    pub temperature: f32,
}

impl From<&BackendState> for BackendStatus {
    fn from(state: &BackendState) -> Self {
        BackendStatus {
            name: state.config.name.clone(),
            url: state.config.url.clone(),
            priority: state.config.priority,
            healthy: state.healthy,
            models: state.models.clone(),
            gpu: state.gpu_metrics.as_ref().map(|g| GpuStatus {
                utilization: g.utilization,
                memory_used: g.memory_used,
                memory_total: g.memory_total,
                memory_percent: if g.memory_total > 0 {
                    (g.memory_used as f32 / g.memory_total as f32) * 100.0
                } else {
                    0.0
                },
                temperature: g.temperature,
            }),
        }
    }
}

pub async fn get_status(State(pool): State<Arc<BackendPool>>) -> Json<Status> {
    let all = pool.all().await;
    let mut healthy_backends = Vec::new();
    let mut unhealthy_backends = Vec::new();

    for name in all {
        if let Some(state) = pool.get(&name).await {
            if state.healthy {
                healthy_backends.push(BackendStatus::from(&state));
            } else {
                unhealthy_backends.push(name);
            }
        }
    }

    Json(Status {
        healthy_backends,
        unhealthy_backends,
        routing_strategy: "model_aware".to_string(),
    })
}