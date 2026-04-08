use crate::backend::BackendPool;
use crate::router::{RoutedBackend, Router};
use async_trait::async_trait;
use std::collections::HashSet;

#[derive(Clone)]
pub struct LeastBusyRouter {
    pool: BackendPool,
}

impl LeastBusyRouter {
    pub fn new(pool: BackendPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl Router for LeastBusyRouter {
    async fn route_excluding(
        &self,
        _model: Option<&str>,
        tags: Option<&[String]>,
        excluded: &HashSet<String>,
    ) -> anyhow::Result<RoutedBackend> {
        let backend = if let Some(tags) = tags {
            self.pool
                .get_least_busy_tagged_excluding(tags, excluded)
                .await
        } else {
            self.pool.get_least_busy_excluding(excluded).await
        }
        .ok_or_else(|| anyhow::anyhow!("No healthy backends available"))?;

        tracing::debug!("Routing to {} (least busy)", backend.config.name);
        Ok(RoutedBackend {
            name: backend.config.name.clone(),
            url: backend.config.url.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::pool::GpuMetrics;
    use crate::backend::BackendPool;
    use crate::config::Backend;
    use std::time::Duration;

    fn make_backend(name: &str, priority: u32) -> Backend {
        Backend {
            name: name.into(),
            url: "http://localhost:11434".into(),
            priority,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn routes_to_least_busy() {
        let pool = BackendPool::new(
            vec![make_backend("busy", 100), make_backend("idle", 50)],
            3,
            Duration::from_secs(60),
        );

        pool.update_gpu_metrics(
            "busy",
            GpuMetrics {
                utilization: 90.0,
                memory_used: 8000,
                memory_total: 16000,
                temperature: 75.0,
            },
        )
        .await;
        pool.update_gpu_metrics(
            "idle",
            GpuMetrics {
                utilization: 10.0,
                memory_used: 2000,
                memory_total: 16000,
                temperature: 45.0,
            },
        )
        .await;

        let router = LeastBusyRouter::new(pool);
        let result = router.route(None, None).await.unwrap();
        assert_eq!(result.name, "idle");
    }

    #[tokio::test]
    async fn handles_no_gpu_metrics() {
        let pool = BackendPool::new(
            vec![make_backend("gpu1", 100), make_backend("gpu2", 50)],
            3,
            Duration::from_secs(60),
        );

        // No GPU metrics set on either backend — should still return a backend
        let router = LeastBusyRouter::new(pool);
        let result = router.route(None, None).await;
        assert!(result.is_ok());
    }
}
