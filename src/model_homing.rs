#![allow(dead_code)]
use crate::backend::BackendPool;
use crate::config::Backend;
use anyhow::Result;
use serde::Deserialize;
use std::time::Duration;
use tokio::time::interval;
use tracing::info;

#[derive(Debug, Deserialize)]
struct OllamaRunning {
    models: Vec<OllamaRunningModel>,
}

#[derive(Debug, Deserialize)]
struct OllamaRunningModel {
    name: String,
    #[serde(default)]
    model: String,
}

pub struct ModelHoming {
    idle_timeout: Duration,
    client: reqwest::Client,
}

impl ModelHoming {
    pub fn new(idle_timeout_minutes: u64) -> Self {
        Self {
            idle_timeout: Duration::from_secs(idle_timeout_minutes * 60),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap(),
        }
    }

    pub async fn spawn(self, pool: BackendPool) {
        tokio::spawn(async move {
            // Check every 5 minutes
            let mut ticker = interval(Duration::from_secs(300));
            loop {
                ticker.tick().await;
                self.check_and_home(&pool).await;
            }
        });
    }

    // NOTE: ModelHoming is superseded by ModelWarmer (Task 3).
    // This method is retained for compile compatibility only and does nothing.
    async fn check_and_home(&self, _pool: &BackendPool) {}

    async fn warm_model(&self, backend: &Backend, model: &str) -> Result<()> {
        // Send a minimal request to load the model into memory
        let url = format!("{}/api/generate", backend.url);
        let body = serde_json::json!({
            "model": model,
            "prompt": "",
            "keep_alive": "5m"
        });

        let resp = self.client.post(&url).json(&body).send().await?;

        if !resp.status().is_success() {
            anyhow::bail!("Failed to warm model: {}", resp.status());
        }

        info!("Warmed model {} on {}", model, backend.name);
        Ok(())
    }

    async fn unload_other_models(&self, backend: &Backend, keep_model: &str) -> Result<()> {
        // Get currently running models
        let url = format!("{}/api/ps", backend.url);
        let resp = self.client.get(&url).send().await?;
        let running: OllamaRunning = resp.json().await?;

        for model in running.models {
            let model_name = if model.model.is_empty() {
                &model.name
            } else {
                &model.model
            };

            if model_name != keep_model {
                // Unload this model
                let unload_url = format!("{}/api/generate", backend.url);
                let body = serde_json::json!({
                    "model": model_name,
                    "keep_alive": 0
                });

                if let Err(e) = self.client.post(&unload_url).json(&body).send().await {
                    tracing::trace!("Failed to unload {}: {}", model_name, e);
                } else {
                    info!("Unloaded {} from {}", model_name, backend.name);
                }
            }
        }

        Ok(())
    }
}
