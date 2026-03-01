pub mod priority;
pub mod model_aware;
pub mod least_busy;

use crate::backend::BackendPool;
use crate::config::RoutingStrategy;
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

#[async_trait]
pub trait Router: Send + Sync {
    async fn route(&self, model: Option<&str>) -> Result<String>;
}

pub fn create_router(strategy: RoutingStrategy, pool: Arc<BackendPool>) -> Box<dyn Router> {
    match strategy {
        RoutingStrategy::Priority => Box::new(priority::PriorityRouter::new(pool)),
        RoutingStrategy::ModelAware => Box::new(model_aware::ModelAwareRouter::new(pool)),
        RoutingStrategy::LeastBusy => Box::new(least_busy::LeastBusyRouter::new(pool)),
    }
}