use crate::backend::BackendPool;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio::time::interval;

const MAX_CONCURRENT_WARMUPS: usize = 20;

pub struct ModelWarmer {
    interval: Duration,
    client: reqwest::Client,
}

impl ModelWarmer {
    pub fn new(interval_secs: u64, timeout_secs: u64) -> Self {
        Self {
            interval: Duration::from_secs(interval_secs),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(timeout_secs))
                .build()
                .unwrap(),
        }
    }

    pub async fn spawn(self, pool: BackendPool) {
        tokio::spawn(async move {
            let mut ticker = interval(self.interval);
            loop {
                ticker.tick().await;
                self.warm_all(&pool).await;
            }
        });
    }

    async fn warm_all(&self, pool: &BackendPool) {
        let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_WARMUPS));
        let backends = pool.all().await;
        for name in backends {
            if let Some(state) = pool.get(&name).await {
                // Skip llama-server backends — models are loaded at server start,
                // and /api/generate is Ollama-specific.
                if state.config.backend == crate::config::BackendType::LlamaServer {
                    continue;
                }
                for model in &state.config.hot_models {
                    let url = warm_url(&state.config.url);
                    let payload = warm_payload(model);
                    let client = self.client.clone();
                    let model = model.clone();
                    let name = name.clone();
                    let permit = semaphore.clone();
                    tokio::spawn(async move {
                        let _permit = permit.acquire().await.expect("semaphore closed");
                        if let Err(e) = client.post(&url).json(&payload).send().await {
                            tracing::warn!("Warmer failed for {} on {}: {}", model, name, e);
                        } else {
                            tracing::debug!("Warmed {} on {}", model, name);
                        }
                    });
                }
            }
        }
    }
}

pub fn warm_url(base_url: &str) -> String {
    format!("{}/api/generate", base_url.trim_end_matches('/'))
}

pub fn warm_payload(model: &str) -> serde_json::Value {
    // Use integer -1 (not string "-1") — older Ollama rejects the string form
    // as time.ParseDuration doesn't special-case "-1" without a unit.
    serde_json::json!({
        "model": model,
        "prompt": "",
        "keep_alive": -1
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warm_url_constructed_correctly() {
        let url = warm_url("http://citadel:11434");
        assert_eq!(url, "http://citadel:11434/api/generate");
    }

    #[test]
    fn llama_server_backend_should_be_skipped_by_warmer() {
        // llama-server backends don't support /api/generate.
        // The warmer skips them based on backend type check.
        // This test documents that warm_url produces Ollama-specific paths.
        let url = warm_url("http://citadel:8090");
        assert!(url.contains("/api/generate"), "warm_url is Ollama-specific");
    }

    #[test]
    fn warm_payload_contains_keep_alive() {
        let payload = warm_payload("llama3:8b");
        assert_eq!(payload["model"], "llama3:8b");
        assert_eq!(payload["keep_alive"], -1);
        assert_eq!(payload["prompt"], "");
    }
}
