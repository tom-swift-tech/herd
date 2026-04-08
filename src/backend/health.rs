use crate::backend::BackendPool;
use std::time::Duration;
use tokio::time::interval;
use tracing::warn;

pub struct HealthChecker {
    client: reqwest::Client,
    interval: Duration,
}

impl HealthChecker {
    pub fn new(interval: Duration) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            interval,
        }
    }

    pub async fn spawn(self, pool: BackendPool) {
        tokio::spawn(async move {
            let mut ticker = interval(self.interval);
            loop {
                ticker.tick().await;
                self.check_all(&pool).await;
            }
        });
    }

    async fn check_all(&self, pool: &BackendPool) {
        let backends = pool.all().await;
        for name in backends {
            if let Some(state) = pool.get(&name).await {
                if !state.healthy && state.last_check.elapsed() < pool.recovery_time() {
                    tracing::trace!(
                        "Backend {} is unhealthy, skipping until recovery time elapses",
                        name
                    );
                    continue;
                }
                let path = state
                    .config
                    .health_check_path
                    .as_deref()
                    .unwrap_or(state.config.default_health_check_path());
                let url = format!("{}{}", state.config.url.trim_end_matches('/'), path);
                match self.client.get(&url).send().await {
                    Ok(resp) => {
                        let expected = state.config.health_check_status.unwrap_or(200);
                        if resp.status().as_u16() == expected
                            || (expected == 200 && resp.status().is_success())
                        {
                            pool.mark_healthy(&name).await;
                            tracing::trace!("Backend {} is healthy", name);
                        } else {
                            warn!(
                                "Backend {} returned status {} (expected {})",
                                name,
                                resp.status(),
                                expected
                            );
                            pool.mark_unhealthy(&name).await;
                            pool.clear_gpu_metrics(&name).await;
                        }
                    }
                    Err(e) => {
                        warn!("Backend {} health check failed: {}", name, e);
                        pool.mark_unhealthy(&name).await;
                        pool.clear_gpu_metrics(&name).await;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::config::Backend;

    #[test]
    fn default_health_check_path() {
        let b = Backend::default();
        assert!(b.health_check_path.is_none());
        assert!(b.health_check_status.is_none());
    }

    #[test]
    fn custom_health_check_config_deserializes() {
        let yaml = r#"
            name: test
            url: http://localhost:11434
            priority: 50
            health_check_path: /health
            health_check_status: 204
        "#;
        let b: Backend = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(b.health_check_path.as_deref(), Some("/health"));
        assert_eq!(b.health_check_status, Some(204));
    }

    #[test]
    fn llama_server_default_health_check_path() {
        let b = Backend {
            name: "llama1".into(),
            url: "http://localhost:8090".into(),
            backend: crate::config::BackendType::LlamaServer,
            priority: 50,
            ..Default::default()
        };
        let path = b
            .health_check_path
            .as_deref()
            .unwrap_or(b.default_health_check_path());
        assert_eq!(path, "/health");
    }

    #[test]
    fn ollama_default_health_check_path() {
        let b = Backend::default();
        let path = b
            .health_check_path
            .as_deref()
            .unwrap_or(b.default_health_check_path());
        assert_eq!(path, "/");
    }

    #[test]
    fn explicit_health_check_path_overrides_default() {
        let b = Backend {
            name: "custom".into(),
            url: "http://localhost:8090".into(),
            backend: crate::config::BackendType::LlamaServer,
            health_check_path: Some("/v1/models".into()),
            priority: 50,
            ..Default::default()
        };
        let path = b
            .health_check_path
            .as_deref()
            .unwrap_or(b.default_health_check_path());
        assert_eq!(path, "/v1/models");
    }
}
