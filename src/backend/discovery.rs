use crate::backend::{BackendPool, GpuMetrics};
use crate::config::Backend;
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
struct OllamaRunning {
    models: Vec<OllamaRunningModel>,
}

#[derive(Debug, Deserialize)]
struct OllamaRunningModel {
    name: String,
    #[serde(default)]
    model: String,
    /// VRAM used by this model in bytes
    #[serde(default)]
    size_vram: u64,
}

#[derive(Debug, Deserialize)]
struct GpuHotData {
    gpus: Vec<GpuInfo>,
}

#[derive(Debug, Deserialize)]
struct GpuInfo {
    #[serde(rename = "index")]
    _index: u32,
    #[allow(dead_code)]
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
                if let Err(e) = self.discover_models(pool, &state.config).await {
                    tracing::warn!("Failed to discover models for {}: {}", name, e);
                }

                // Discover currently loaded model
                if let Err(e) = self.discover_running(pool, &state.config).await {
                    tracing::trace!("No running model on {}: {}", name, e);
                }

                // Discover GPU metrics via explicit gpu_hot_url, or auto-derive from backend host on port 1312
                let gpu_url = if let Some(ref configured) = state.config.gpu_hot_url {
                    Some(configured.clone())
                } else {
                    let host = state
                        .config
                        .url
                        .trim_start_matches("http://")
                        .trim_start_matches("https://")
                        .split(':')
                        .next()
                        .unwrap_or("");
                    if !host.is_empty() {
                        Some(format!("http://{}:1312", host))
                    } else {
                        None
                    }
                };
                if let Some(ref gpu_url) = gpu_url {
                    if let Err(e) = self.discover_gpu_metrics(pool, &name, gpu_url).await {
                        tracing::trace!("No gpu-hot on {}: {}", name, e);
                    }
                }

                // VRAM probe: run once per backend on first discovery
                if !state.vram_probed && state.healthy {
                    info!("Probing VRAM on {} (first discovery)...", name);
                    match self.probe_vram(pool, &state.config).await {
                        Ok(vram_mb) => {
                            info!("Backend {} VRAM: {} MB", name, vram_mb);
                            pool.set_vram(&name, vram_mb).await;
                        }
                        Err(e) => {
                            tracing::warn!("VRAM probe failed on {}: {}", name, e);
                            // Mark as probed so we don't retry every cycle
                            pool.mark_vram_probed(&name).await;
                        }
                    }
                }
            }
        }
        info!("Model discovery complete");
    }

    async fn discover_models(&self, pool: &BackendPool, backend: &Backend) -> Result<()> {
        let url = format!("{}/api/tags", backend.url);
        let resp = self.client.get(&url).send().await?;
        let models: OllamaModels = resp.json().await?;

        let mut model_names: Vec<String> = models.models.into_iter().map(|m| m.name).collect();

        // Apply model_filter regex if configured
        if let Some(ref filter) = backend.model_filter {
            match regex::Regex::new(filter) {
                Ok(re) => {
                    let before = model_names.len();
                    model_names.retain(|name| re.is_match(name));
                    if model_names.len() < before {
                        tracing::debug!(
                            "model_filter '{}' on {}: kept {}/{} models",
                            filter,
                            backend.name,
                            model_names.len(),
                            before
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "Invalid model_filter '{}' on {}: {}",
                        filter,
                        backend.name,
                        e
                    );
                }
            }
        }

        pool.update_models(&backend.name, model_names).await;

        Ok(())
    }

    async fn discover_running(&self, pool: &BackendPool, backend: &Backend) -> Result<()> {
        let url = format!("{}/api/ps", backend.url);
        let resp = self.client.get(&url).send().await?;
        let running: OllamaRunning = resp.json().await?;

        // Get the first running model (if any)
        let current = running.models.first().map(|m| {
            if m.model.is_empty() {
                m.name.clone()
            } else {
                m.model.clone()
            }
        });

        pool.update_current_model(&backend.name, current).await;
        Ok(())
    }

    /// Probe VRAM by pulling a small model, running a short prompt, then reading
    /// `size_vram` from `/api/ps`. This gives us the GPU's total usable VRAM.
    async fn probe_vram(&self, pool: &BackendPool, backend: &Backend) -> Result<u64> {
        let probe_model = "llama3.2:3b";
        let base = backend.url.trim_end_matches('/');

        // Use a longer timeout for pull operations
        let pull_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(600))
            .build()?;

        // 1. Pull the probe model (may already exist)
        tracing::info!("Pulling {} on {} for VRAM probe...", probe_model, backend.name);
        let pull_resp = pull_client
            .post(format!("{}/api/pull", base))
            .json(&serde_json::json!({"name": probe_model, "stream": false}))
            .send()
            .await?;

        if !pull_resp.status().is_success() {
            anyhow::bail!(
                "Pull failed with status {} on {}",
                pull_resp.status(),
                backend.name
            );
        }
        // Consume response body
        let _ = pull_resp.text().await;

        // 2. Run a tiny generation to force the model into VRAM
        tracing::info!("Running VRAM probe generation on {}...", backend.name);
        let gen_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()?;
        let gen_resp = gen_client
            .post(format!("{}/api/generate", base))
            .json(&serde_json::json!({
                "model": probe_model,
                "prompt": "hi",
                "stream": false,
                "options": {"num_predict": 1}
            }))
            .send()
            .await?;

        if !gen_resp.status().is_success() {
            anyhow::bail!(
                "Generate failed with status {} on {}",
                gen_resp.status(),
                backend.name
            );
        }
        let _ = gen_resp.text().await;

        // 3. Read VRAM from /api/ps
        tokio::time::sleep(Duration::from_secs(1)).await;
        let ps_resp = self
            .client
            .get(format!("{}/api/ps", base))
            .send()
            .await?;
        let running: OllamaRunning = ps_resp.json().await?;

        let vram_bytes = running
            .models
            .first()
            .map(|m| m.size_vram)
            .unwrap_or(0);

        // Convert to MB
        let vram_mb = if vram_bytes > 0 {
            vram_bytes / (1024 * 1024)
        } else {
            // Fallback: try GPU metrics if available
            if let Some(state) = pool.get(&backend.name).await {
                if let Some(gpu) = &state.gpu_metrics {
                    gpu.memory_total
                } else {
                    0
                }
            } else {
                0
            }
        };

        // Refresh model list since we just pulled one
        let _ = self.discover_models(pool, backend).await;

        Ok(vram_mb)
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
