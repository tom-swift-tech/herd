pub mod priority;
pub mod model_aware;
pub mod least_busy;

use crate::backend::BackendPool;
use crate::config::RoutingStrategy;
use anyhow::Result;
use async_trait::async_trait;

#[async_trait]
pub trait Router: Send + Sync {
    async fn route(&self, model: Option<&str>) -> Result<String>;
}

#[derive(Clone)]
pub enum RouterEnum {
    Priority(priority::PriorityRouter),
    ModelAware(model_aware::ModelAwareRouter),
    LeastBusy(least_busy::LeastBusyRouter),
}

#[async_trait]
impl Router for RouterEnum {
    async fn route(&self, model: Option<&str>) -> Result<String> {
        match self {
            RouterEnum::Priority(r) => r.route(model).await,
            RouterEnum::ModelAware(r) => r.route(model).await,
            RouterEnum::LeastBusy(r) => r.route(model).await,
        }
    }
}

pub fn create_router(strategy: RoutingStrategy, pool: BackendPool) -> RouterEnum {
    match strategy {
        RoutingStrategy::Priority => RouterEnum::Priority(priority::PriorityRouter::new(pool)),
        RoutingStrategy::ModelAware => RouterEnum::ModelAware(model_aware::ModelAwareRouter::new(pool)),
        RoutingStrategy::LeastBusy => RouterEnum::LeastBusy(least_busy::LeastBusyRouter::new(pool)),
    }
}