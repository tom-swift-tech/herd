use crate::backend::BackendPool;
use crate::router::Router;
use async_trait::async_trait;

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
    async fn route(&self, _model: Option<&str>) -> anyhow::Result<String> {
        // Route to least busy backend (by GPU utilization)
        let backend = self
            .pool
            .get_least_busy()
            .await
            .ok_or_else(|| anyhow::anyhow!("No healthy backends available"))?;

        tracing::debug!(
            "Routing to {} (least busy)",
            backend.config.name
        );
        Ok(backend.config.url)
    }
}