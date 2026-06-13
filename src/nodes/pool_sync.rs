use crate::backend::BackendPool;
use crate::nodes::NodeRegistry;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

/// Mirrors agent nodes from the in-memory NodeRegistry into the BackendPool so
/// the existing routers treat them identically to static/enrolled backends.
///
/// Owns ONLY the `"agent:"` key prefix — it never touches `"node:"` (enrolled,
/// owned by `NodeHealthPoller::sync_to_pool`) or static-config entries.
pub struct AgentPoolSync {
    interval: Duration,
}

impl AgentPoolSync {
    pub fn new(interval_secs: u64) -> Self {
        Self {
            interval: Duration::from_secs(interval_secs.max(1)),
        }
    }

    /// Spawn the reconciler as a background task. Each tick reconciles the whole
    /// agent set: add newly-fresh nodes, update changed ones, remove no-longer-fresh.
    pub fn spawn(self, registry: Arc<NodeRegistry>, pool: Arc<BackendPool>) {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(self.interval);
            loop {
                ticker.tick().await;
                Self::reconcile(&registry, &pool).await;
            }
        });
    }

    /// One reconcile pass. Source = `registry.fresh_nodes()` (strict TTL, no grace).
    /// Key = `"agent:{node_id}"`. An agent in `fresh_nodes()` is alive by
    /// definition → `healthy = true`.
    ///
    /// `pub(crate)` so unit tests can drive a single pass deterministically
    /// without the timer.
    pub(crate) async fn reconcile(registry: &NodeRegistry, pool: &BackendPool) {
        let fresh = registry.fresh_nodes().await; // Vec<AgentState>
        let fresh_keys: HashSet<String> = fresh
            .iter()
            .map(|s| format!("agent:{}", s.capabilities.node_id))
            .collect();

        // Remove ONLY agent entries that are no longer fresh.
        // NEVER touch "node:"/static entries.
        for name in pool.all().await {
            if name.starts_with("agent:") && !fresh_keys.contains(&name) {
                pool.remove(&name).await;
                tracing::debug!("Removed agent backend {} from pool", name);
            }
        }

        // Add or update fresh agent entries.
        for state in &fresh {
            let caps = &state.capabilities;
            let name = format!("agent:{}", caps.node_id);
            let backend = crate::config::Backend {
                name: name.clone(),
                url: caps.address.clone(),
                backend: caps.backend,
                priority: 50, // house default for agent nodes
                tags: Vec::new(),
                ..Default::default()
            };

            if let Some(mut st) = pool.get(&name).await {
                // Update existing entry — refresh config, models, health, and VRAM.
                st.config = backend;
                st.models = caps.models_loaded.clone();
                st.healthy = true; // fresh ⇒ alive
                if caps.vram_total_mb > 0 {
                    st.vram_total_mb = Some(caps.vram_total_mb);
                    st.vram_populated = true;
                }
                pool.update(st).await;
            } else {
                // New entry: add, then set models and VRAM.
                pool.add(backend).await;
                pool.update_models(&name, caps.models_loaded.clone()).await;
                if caps.vram_total_mb > 0 {
                    pool.set_vram(&name, caps.vram_total_mb).await;
                }
                tracing::info!("Added agent backend {} to pool ({})", name, caps.address);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::BackendPool;
    use crate::config::{Backend, BackendType};
    use crate::nodes::registry::test_clock::TestClock;
    use crate::nodes::{AgentCapabilities, NodeRegistry};
    use crate::router::model_aware::ModelAwareRouter;
    use crate::router::Router;
    use std::time::Duration;

    fn sample_caps(node_id: &str) -> AgentCapabilities {
        AgentCapabilities {
            node_id: node_id.to_string(),
            backend: BackendType::LlamaServer,
            address: "http://10.0.0.1:8080".to_string(),
            gpu_model: Some("RTX 5090".to_string()),
            vram_total_mb: 32_768,
            vram_free_mb: 30_000,
            models_loaded: vec!["llama-3-8b".to_string()],
            queue_depth: 0,
            ttft_p50_ms: Some(42),
            rpc_capable: false,
            rpc_port: None,
            agent_version: "1.2.0".to_string(),
            os: Some("linux".to_string()),
            arch: Some("x86_64".to_string()),
        }
    }

    /// Test 1: A fresh agent registered via heartbeat appears in the pool and is
    /// routable by the ModelAwareRouter.
    #[tokio::test]
    async fn fresh_agent_appears_in_pool_and_is_routable() {
        let reg = NodeRegistry::new(Duration::from_secs(30));
        let pool = Arc::new(BackendPool::new(vec![], 3, Duration::from_secs(30)));

        // Register an agent with a specific model.
        let mut caps = sample_caps("node-alpha");
        caps.address = "http://10.0.0.5:8080".to_string();
        caps.models_loaded = vec!["llama-3-8b".to_string()];
        reg.heartbeat(caps).await.unwrap();

        AgentPoolSync::reconcile(&reg, &pool).await;

        // Pool must contain the agent entry.
        let all = pool.all().await;
        assert!(
            all.contains(&"agent:node-alpha".to_string()),
            "expected agent:node-alpha in pool, got {:?}",
            all
        );

        // Entry must be healthy with the right URL and model.
        let entry = pool.get("agent:node-alpha").await.unwrap();
        assert!(entry.healthy);
        assert_eq!(entry.config.url, "http://10.0.0.5:8080");
        assert!(entry.models.contains(&"llama-3-8b".to_string()));

        // Must route via ModelAwareRouter.
        let router = ModelAwareRouter::new((*pool).clone());
        let routed = router.route(Some("llama-3-8b"), None).await.unwrap();
        assert_eq!(routed.name, "agent:node-alpha");
    }

    /// Test 2: A stale agent (TTL expired, no longer in fresh_nodes()) is removed
    /// from the pool on the next reconcile, and the pool becomes empty → router
    /// returns an error (the 503 path).
    ///
    /// Determinism strategy: drive a manual `TestClock` (mirrors
    /// `fresh_nodes_excludes_stale_but_not_yet_evicted` in registry.rs) instead
    /// of sleeping. Register an agent, reconcile once (entry added), advance the
    /// clock past the TTL so `fresh_nodes()` returns empty, reconcile again →
    /// pool empties. No wall-clock dependence, so it can't flake under CI load.
    #[tokio::test]
    async fn stale_agent_is_drained_and_pool_empties_to_503() {
        let clock = TestClock::new();
        let reg = NodeRegistry::with_clock(Duration::from_secs(30), clock.as_fn());
        let pool = Arc::new(BackendPool::new(vec![], 3, Duration::from_secs(30)));

        reg.heartbeat(sample_caps("node-beta")).await.unwrap();

        // First reconcile: entry should appear.
        AgentPoolSync::reconcile(&reg, &pool).await;
        assert!(
            pool.all().await.contains(&"agent:node-beta".to_string()),
            "agent should be added on first reconcile"
        );

        // Advance past the 30 s TTL so the node leaves fresh_nodes().
        clock.advance(Duration::from_secs(31));

        // Second reconcile: entry should be removed.
        AgentPoolSync::reconcile(&reg, &pool).await;
        assert!(
            pool.all().await.is_empty(),
            "pool should be empty after agent expires"
        );

        // Router must return an error (503 path).
        let router = ModelAwareRouter::new((*pool).clone());
        let result = router.route(Some("llama-3-8b"), None).await;
        assert!(result.is_err(), "expected error when pool is empty");
    }

    /// Test 3: reconcile never removes "node:" (enrolled) or static entries,
    /// even with zero or unrelated agents registered.
    #[tokio::test]
    async fn agent_reconcile_never_removes_node_or_static_entries() {
        let reg = NodeRegistry::new(Duration::from_secs(30));
        let pool = Arc::new(BackendPool::new(vec![], 3, Duration::from_secs(30)));

        // Seed with a static entry and an enrolled-style entry.
        pool.add(Backend {
            name: "static-gpu".to_string(),
            url: "http://static:8080".to_string(),
            priority: 100,
            ..Default::default()
        })
        .await;
        pool.add(Backend {
            name: "node:citadel".to_string(),
            url: "http://citadel:11434".to_string(),
            priority: 80,
            ..Default::default()
        })
        .await;

        // Reconcile with zero agents registered.
        AgentPoolSync::reconcile(&reg, &pool).await;

        let all = pool.all().await;
        assert!(
            all.contains(&"static-gpu".to_string()),
            "static entry must survive reconcile"
        );
        assert!(
            all.contains(&"node:citadel".to_string()),
            "enrolled node entry must survive reconcile"
        );

        // Also with one fresh agent — static/node entries must still coexist.
        reg.heartbeat(sample_caps("node-gamma")).await.unwrap();
        AgentPoolSync::reconcile(&reg, &pool).await;

        let all = pool.all().await;
        assert!(
            all.contains(&"static-gpu".to_string()),
            "static entry must survive alongside agent"
        );
        assert!(
            all.contains(&"node:citadel".to_string()),
            "enrolled entry must survive alongside agent"
        );
        assert!(
            all.contains(&"agent:node-gamma".to_string()),
            "agent entry must be added"
        );
    }

    /// Test 4: An enrolled pool entry "node:citadel" and an agent whose node_id
    /// is "citadel" coexist as two distinct pool keys — "node:citadel" and
    /// "agent:citadel" — without deduplication (decision 14 from the v1.2 design).
    #[tokio::test]
    async fn enrolled_and_agent_same_host_yield_two_entries() {
        let reg = NodeRegistry::new(Duration::from_secs(30));
        let pool = Arc::new(BackendPool::new(vec![], 3, Duration::from_secs(30)));

        // Add the enrolled entry (as NodeHealthPoller would).
        pool.add(Backend {
            name: "node:citadel".to_string(),
            url: "http://citadel:11434".to_string(),
            priority: 80,
            ..Default::default()
        })
        .await;

        // Register an agent with the same host/node_id.
        let mut caps = sample_caps("citadel");
        caps.address = "http://citadel:8080".to_string();
        reg.heartbeat(caps).await.unwrap();

        AgentPoolSync::reconcile(&reg, &pool).await;

        let all = pool.all().await;
        assert!(
            all.contains(&"node:citadel".to_string()),
            "enrolled entry must be present"
        );
        assert!(
            all.contains(&"agent:citadel".to_string()),
            "agent entry must be present as a distinct key"
        );
        // Both keys are distinct — two entries, not one.
        assert_eq!(
            all.iter()
                .filter(|n| *n == "node:citadel" || *n == "agent:citadel")
                .count(),
            2,
            "enrolled and agent entries must be two distinct pool rows"
        );
    }
}
