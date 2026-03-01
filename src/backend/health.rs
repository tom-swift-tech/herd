use crate::backend::BackendPool;
use std::time::Duration;
use tokio::time::interval;
use tracing::warn;

pub struct HealthChecker {
    client: reqwest::Client,
    interval: Duration,
}

impl HealthChecker {
    pub fn new(interval_secs: u64) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
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
                self.check_all(&pool).await;
            }
        });
    }

    async fn check_all(&self, pool: &BackendPool) {
        let backends = pool.all().await;
        for name in backends {
            if let Some(state) = pool.get(&name).await {
                let url = format!("{}/", state.config.url);
                match self.client.get(&url).send().await {
                    Ok(resp) if resp.status().is_success() => {
                        pool.mark_healthy(&name).await;
                        tracing::trace!("Backend {} is healthy", name);
                    }
                    Ok(resp) => {
                        warn!("Backend {} returned status {}", name, resp.status());
                        pool.mark_unhealthy(&name).await;
                    }
                    Err(e) => {
                        warn!("Backend {} health check failed: {}", name, e);
                        pool.mark_unhealthy(&name).await;
                    }
                }
            }
        }
    }
}