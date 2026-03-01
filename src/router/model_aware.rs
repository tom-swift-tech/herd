use crate::backend::BackendPool;
use crate::router::Router;
use async_trait::async_trait;

#[derive(Clone)]
pub struct ModelAwareRouter {
    pool: BackendPool,
}

impl ModelAwareRouter {
    pub fn new(pool: BackendPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl Router for ModelAwareRouter {
    async fn route(&self, model: Option<&str>) -> anyhow::Result<String> {
        // If model specified, try to find backend with model loaded
        if let Some(model_name) = model {
            if let Some(backend) = self.pool.get_by_model(model_name).await {
                tracing::debug!(
                    "Routing {} to {} (model loaded)",
                    model_name,
                    backend.config.name
                );
                return Ok(backend.config.url);
            }
        }

        // Fall back to highest priority healthy backend
        let backend = self
            .pool
            .get_by_priority()
            .await
            .ok_or_else(|| anyhow::anyhow!("No healthy backends available"))?;

        tracing::debug!(
            "Routing to {} (no model preference)",
            backend.config.name
        );
        Ok(backend.config.url)
    }
}