use crate::backend::{BackendPool, GpuMetrics};
use anyhow::Result;
use serde::Deserialize;
use std::time::Duration;
use tokio::time::interval;
use tracing::info;

#[derive(Debug, Deserialize)]
struct OllamaModels {
    models: Vec<OllamaModel>,
}

#[derive(Debug, Deserialize)]
struct OllamaModel {
    name: String,
}

#[derive(Debug, Deserialize)]
struct GpuHotData {
    gpus: Vec<GpuInfo>,
}

#[derive(Debug, Deserialize)]
struct GpuInfo {
    #[serde(rename = "index")]
    _index: u32,
    name: String,
    utilization: f32,
    memory_used: u64,
    memory_total: u64,
    temperature: f32,
}

pub struct ModelDiscovery {
    client: reqwest::Client,
    interval: Duration,
}

impl ModelDiscovery {
    pub fn new(interval_secs: u64) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap(),
            interval: Duration::from_secs(interval_secs),
        }
    }

    pub async fn spawn(self, pool: BackendPool) {
        tokio::spawn(async move {
            let mut ticker = interval(self.interval);
            loop {
                ticker.tick().await;
                self.discover_all(&pool).await;
            }
        });
    }

    async fn discover_all(&self, pool: &BackendPool) {
        let backends = pool.all().await;
        for name in backends {
            if let Some(state) = pool.get(&name).await {
                // Discover available models
                if let Err(e) = self.discover_models(&pool, &state.config).await {
                    tracing::warn!("Failed to discover models for {}: {}", name, e);
                }

                // Discover GPU metrics if configured
                if let Some(ref gpu_url) = state.config.gpu_hot_url {
                    if let Err(e) = self.discover_gpu_metrics(&pool, &name, gpu_url).await {
                        tracing::warn!("Failed to get GPU metrics for {}: {}", name, e);
                    }
                }
            }
        }
        info!("Model discovery complete");
    }

    async fn discover_models(&self, pool: &BackendPool, backend: &crate::config::Backend) -> Result<()> {
        let url = format!("{}/api/tags", backend.url);
        let resp = self.client.get(&url).send().await?;
        let models: OllamaModels = resp.json().await?;

        let model_names: Vec<String> = models.models.into_iter().map(|m| m.name).collect();
        pool.update_models(&backend.name, model_names).await;

        Ok(())
    }

    async fn discover_gpu_metrics(&self, pool: &BackendPool, name: &str, url: &str) -> Result<()> {
        let url = format!("{}/api/gpu-data", url);
        let resp = self.client.get(&url).send().await?;
        let data: GpuHotData = resp.json().await?;

        // Use first GPU for now (could aggregate multi-GPU later)
        if let Some(gpu) = data.gpus.first() {
            let metrics = GpuMetrics {
                utilization: gpu.utilization,
                memory_used: gpu.memory_used,
                memory_total: gpu.memory_total,
                temperature: gpu.temperature,
            };
            pool.update_gpu_metrics(name, metrics).await;
        }

        Ok(())
    }
}