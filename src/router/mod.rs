pub mod deployment;
pub mod least_busy;
pub mod model_aware;
pub mod priority;
pub mod scored;
pub mod weighted_round_robin;

use crate::backend::BackendPool;
use crate::config::{RoutingConfig, RoutingStrategy};
use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashSet;

/// Optional request context for score-aware routing.
/// All fields are optional; a `None` field makes its dependent dimension
/// "not present" (neutral, weight-dropped) — never a penalty.
#[derive(Clone, Debug, Default)]
pub struct RouteContext {
    /// Prompt size in tokens, if the proxy has cheaply estimated it pre-routing.
    pub prompt_tokens: Option<u32>,
    /// Requested context-window length, if the request carried one (e.g. num_ctx).
    pub requested_ctx_len: Option<u32>,
}

#[async_trait]
pub trait Router: Send + Sync {
    async fn route(&self, model: Option<&str>, tags: Option<&[String]>) -> Result<RoutedBackend> {
        self.route_excluding(model, tags, &HashSet::new()).await
    }

    async fn route_excluding(
        &self,
        model: Option<&str>,
        tags: Option<&[String]>,
        excluded: &HashSet<String>,
    ) -> Result<RoutedBackend>;

    /// Score-aware route. Default body delegates to `route_excluding`, ignoring
    /// `ctx`, so the four legacy routers inherit it verbatim and are untouched.
    async fn route_scored(
        &self,
        model: Option<&str>,
        tags: Option<&[String]>,
        excluded: &HashSet<String>,
        _ctx: &RouteContext,
    ) -> Result<RoutedBackend> {
        self.route_excluding(model, tags, excluded).await
    }
}

#[derive(Clone, Debug)]
pub struct RoutedBackend {
    pub name: String,
    pub url: String,
}

#[derive(Clone)]
pub enum RouterEnum {
    Priority(priority::PriorityRouter),
    ModelAware(model_aware::ModelAwareRouter),
    LeastBusy(least_busy::LeastBusyRouter),
    WeightedRoundRobin(weighted_round_robin::WeightedRoundRobinRouter),
    Scored(scored::ScoredRouter),
}

#[async_trait]
impl Router for RouterEnum {
    async fn route_excluding(
        &self,
        model: Option<&str>,
        tags: Option<&[String]>,
        excluded: &HashSet<String>,
    ) -> Result<RoutedBackend> {
        match self {
            RouterEnum::Priority(r) => r.route_excluding(model, tags, excluded).await,
            RouterEnum::ModelAware(r) => r.route_excluding(model, tags, excluded).await,
            RouterEnum::LeastBusy(r) => r.route_excluding(model, tags, excluded).await,
            RouterEnum::WeightedRoundRobin(r) => r.route_excluding(model, tags, excluded).await,
            RouterEnum::Scored(r) => r.route_excluding(model, tags, excluded).await,
        }
    }

    /// Override required: if left as trait default, `RouterEnum::route_scored` would
    /// delegate to `route_excluding` → `ScoredRouter::route_excluding` → `route_scored`
    /// with a DEFAULT ctx, silently dropping any caller-supplied ctx. Each variant
    /// dispatches to its own `route_scored` (the real impl for Scored; the ctx-blind
    /// trait default for the four legacy routers).
    async fn route_scored(
        &self,
        model: Option<&str>,
        tags: Option<&[String]>,
        excluded: &HashSet<String>,
        ctx: &RouteContext,
    ) -> Result<RoutedBackend> {
        match self {
            RouterEnum::Priority(r) => r.route_scored(model, tags, excluded, ctx).await,
            RouterEnum::ModelAware(r) => r.route_scored(model, tags, excluded, ctx).await,
            RouterEnum::LeastBusy(r) => r.route_scored(model, tags, excluded, ctx).await,
            RouterEnum::WeightedRoundRobin(r) => r.route_scored(model, tags, excluded, ctx).await,
            RouterEnum::Scored(r) => r.route_scored(model, tags, excluded, ctx).await,
        }
    }
}

/// Construct the router for the given strategy.
/// `routing` is passed so the Scored arm can read `routing.scored`; legacy arms
/// ignore it — no behaviour change for Priority/ModelAware/LeastBusy/WRR.
pub fn create_router(
    strategy: RoutingStrategy,
    pool: BackendPool,
    routing: &RoutingConfig,
) -> RouterEnum {
    match strategy {
        RoutingStrategy::Priority => RouterEnum::Priority(priority::PriorityRouter::new(pool)),
        RoutingStrategy::ModelAware => {
            RouterEnum::ModelAware(model_aware::ModelAwareRouter::new(pool))
        }
        RoutingStrategy::LeastBusy => RouterEnum::LeastBusy(least_busy::LeastBusyRouter::new(pool)),
        RoutingStrategy::WeightedRoundRobin => RouterEnum::WeightedRoundRobin(
            weighted_round_robin::WeightedRoundRobinRouter::new(pool),
        ),
        RoutingStrategy::Scored => {
            RouterEnum::Scored(scored::ScoredRouter::new(pool, &routing.scored))
        }
    }
}
