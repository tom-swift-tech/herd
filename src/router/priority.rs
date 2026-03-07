use crate::backend::BackendPool;
use crate::router::{Router, RoutedBackend};
use async_trait::async_trait;

#[derive(Clone)]
pub struct PriorityRouter {
    pool: BackendPool,
}

impl PriorityRouter {
    pub fn new(pool: BackendPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl Router for PriorityRouter {
    async fn route(&self, _model: Option<&str>) -> anyhow::Result<RoutedBackend> {
        // Route to highest priority healthy backend
        let backend = self
            .pool
            .get_by_priority()
            .await
            .ok_or_else(|| anyhow::anyhow!("No healthy backends available"))?;

        Ok(RoutedBackend {
            name: backend.config.name.clone(),
            url: backend.config.url.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    async fn routes_to_highest_priority() {
        let pool = BackendPool::new(
            vec![make_backend("low", 10), make_backend("high", 100)],
            3,
            Duration::from_secs(60),
        );
        let router = PriorityRouter::new(pool);

        let result = router.route(None).await.unwrap();
        assert_eq!(result.name, "high");
    }

    #[tokio::test]
    async fn skips_unhealthy_backends() {
        let pool = BackendPool::new(
            vec![make_backend("high", 100), make_backend("low", 10)],
            1, // single failure marks unhealthy
            Duration::from_secs(60),
        );

        // Mark the high-priority backend unhealthy
        pool.mark_unhealthy("high").await;

        let router = PriorityRouter::new(pool);
        let result = router.route(None).await.unwrap();
        assert_eq!(result.name, "low");
    }

    #[tokio::test]
    async fn error_when_no_healthy() {
        let pool = BackendPool::new(
            vec![make_backend("only", 100)],
            1,
            Duration::from_secs(60),
        );

        pool.mark_unhealthy("only").await;

        let router = PriorityRouter::new(pool);
        let result = router.route(None).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("No healthy backends")
        );
    }
}