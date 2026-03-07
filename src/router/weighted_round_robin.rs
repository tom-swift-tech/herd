use crate::backend::BackendPool;
use crate::router::{Router, RoutedBackend};
use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(Clone)]
pub struct WeightedRoundRobinRouter {
    pool: BackendPool,
    counter: Arc<AtomicUsize>,
}

impl WeightedRoundRobinRouter {
    pub fn new(pool: BackendPool) -> Self {
        Self {
            pool,
            counter: Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[async_trait]
impl Router for WeightedRoundRobinRouter {
    async fn route(&self, _model: Option<&str>) -> anyhow::Result<RoutedBackend> {
        let backends = self.pool.backends.read().await;
        let healthy: Vec<_> = backends.iter().filter(|b| b.healthy).collect();

        if healthy.is_empty() {
            return Err(anyhow::anyhow!("No healthy backends available"));
        }

        let total_weight: u32 = healthy.iter().map(|b| b.config.priority).sum();
        if total_weight == 0 {
            return Err(anyhow::anyhow!("All healthy backends have zero weight"));
        }

        let tick = self.counter.fetch_add(1, Ordering::Relaxed);
        let slot = (tick as u32) % total_weight;

        // Cumulative weight comparison: find the backend whose cumulative range covers the slot
        let mut cumulative = 0u32;
        let backend = healthy
            .iter()
            .find(|b| {
                cumulative += b.config.priority;
                slot < cumulative
            })
            .expect("slot must fall within total_weight");

        tracing::debug!(
            "Routing to {} (weighted round-robin, slot {}/{})",
            backend.config.name,
            slot,
            total_weight
        );

        Ok(RoutedBackend {
            name: backend.config.name.clone(),
            url: backend.config.url.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Backend;
    use std::time::Duration;

    fn make_pool(backends: Vec<Backend>) -> BackendPool {
        BackendPool::new(backends, 3, Duration::from_secs(60))
    }

    #[tokio::test]
    async fn distributes_by_weight() {
        let pool = make_pool(vec![
            Backend {
                name: "heavy".into(),
                url: "http://heavy".into(),
                priority: 3,
                ..Backend::default()
            },
            Backend {
                name: "light".into(),
                url: "http://light".into(),
                priority: 1,
                ..Backend::default()
            },
        ]);

        let router = WeightedRoundRobinRouter::new(pool);

        let mut heavy_count = 0u32;
        let mut light_count = 0u32;
        let total_requests = 400;

        for _ in 0..total_requests {
            let routed = router.route(None).await.unwrap();
            match routed.name.as_str() {
                "heavy" => heavy_count += 1,
                "light" => light_count += 1,
                other => panic!("unexpected backend: {}", other),
            }
        }

        // With weights 3:1, heavy should get 75% and light 25%
        assert_eq!(heavy_count, 300);
        assert_eq!(light_count, 100);
    }

    #[tokio::test]
    async fn single_backend() {
        let pool = make_pool(vec![Backend {
            name: "solo".into(),
            url: "http://solo".into(),
            priority: 5,
            ..Backend::default()
        }]);

        let router = WeightedRoundRobinRouter::new(pool);

        for _ in 0..10 {
            let routed = router.route(None).await.unwrap();
            assert_eq!(routed.name, "solo");
        }
    }

    #[tokio::test]
    async fn skips_unhealthy_backends() {
        let pool = make_pool(vec![
            Backend {
                name: "healthy".into(),
                url: "http://healthy".into(),
                priority: 1,
                ..Backend::default()
            },
            Backend {
                name: "down".into(),
                url: "http://down".into(),
                priority: 10,
                ..Backend::default()
            },
        ]);

        // Mark "down" as unhealthy
        {
            let mut backends = pool.backends.write().await;
            backends.iter_mut().find(|b| b.config.name == "down").unwrap().healthy = false;
        }

        let router = WeightedRoundRobinRouter::new(pool);

        for _ in 0..10 {
            let routed = router.route(None).await.unwrap();
            assert_eq!(routed.name, "healthy");
        }
    }

    #[tokio::test]
    async fn no_healthy_backends_returns_error() {
        let pool = make_pool(vec![Backend {
            name: "down".into(),
            url: "http://down".into(),
            priority: 1,
            ..Backend::default()
        }]);

        {
            let mut backends = pool.backends.write().await;
            backends[0].healthy = false;
        }

        let router = WeightedRoundRobinRouter::new(pool);
        assert!(router.route(None).await.is_err());
    }
}
