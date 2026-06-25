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
    /// `#[doc(hidden)] pub` so both in-crate unit tests and out-of-crate
    /// integration tests can drive a single pass deterministically without the
    /// timer. Not part of the stable API — the production driver is [`spawn`].
    ///
    /// [`spawn`]: AgentPoolSync::spawn
    #[doc(hidden)]
    pub async fn reconcile(registry: &NodeRegistry, pool: &BackendPool) {
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
                max_context_len: caps.context_len,
                ..Default::default()
            };

            if let Some(mut st) = pool.get(&name).await {
                // Update existing entry — refresh config, models, health, VRAM, and live telemetry.
                st.config = backend;
                // Propagate the reported context-window size so dim 3 remains
                // current across backend restarts (e.g. a model reload that
                // changes --ctx-size).
                st.config.max_context_len = caps.context_len;
                st.models = caps.models_loaded.clone();
                st.healthy = true; // fresh ⇒ alive
                if caps.vram_total_mb > 0 {
                    st.vram_total_mb = Some(caps.vram_total_mb);
                    st.vram_populated = true;
                }
                st.queue_depth = caps.queue_depth;
                st.ttft_p50_ms = caps.ttft_p50_ms;
                st.vram_free_mb = Some(caps.vram_free_mb);
                st.max_concurrent = caps.max_concurrent;
                st.gpu_model = caps.gpu_model.clone();
                pool.update(st).await;
            } else {
                // New entry: add, then set models, VRAM, and live telemetry.
                pool.add(backend).await;
                pool.update_models(&name, caps.models_loaded.clone()).await;
                if caps.vram_total_mb > 0 {
                    pool.set_vram(&name, caps.vram_total_mb).await;
                }
                pool.set_agent_telemetry(
                    &name,
                    caps.queue_depth,
                    caps.ttft_p50_ms,
                    caps.vram_free_mb,
                    caps.max_concurrent,
                    caps.gpu_model.clone(),
                )
                .await;
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
            // Non-zero so telemetry-population assertions will fail if population is removed.
            queue_depth: Some(3),
            ttft_p50_ms: Some(42),
            max_concurrent: Some(8),
            context_len: None,
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

    /// Test 5: A freshly-added agent entry carries queue_depth, ttft_p50_ms,
    /// vram_free_mb, and max_concurrent from capabilities. Covers the "add"
    /// branch of reconcile.
    #[tokio::test]
    async fn new_agent_carries_telemetry_fields() {
        let reg = NodeRegistry::new(Duration::from_secs(30));
        let pool = Arc::new(BackendPool::new(vec![], 3, Duration::from_secs(30)));

        let caps = sample_caps("tele-node");
        // Confirm sample_caps has the values we assert below.
        assert_eq!(caps.queue_depth, Some(3));
        assert_eq!(caps.ttft_p50_ms, Some(42));
        assert_eq!(caps.vram_free_mb, 30_000);
        assert_eq!(caps.max_concurrent, Some(8));

        reg.heartbeat(caps).await.unwrap();
        AgentPoolSync::reconcile(&reg, &pool).await;

        let entry = pool.get("agent:tele-node").await.unwrap();
        assert_eq!(
            entry.queue_depth,
            Some(3),
            "queue_depth must be populated on add"
        );
        assert_eq!(
            entry.ttft_p50_ms,
            Some(42),
            "ttft_p50_ms must be populated on add"
        );
        assert_eq!(
            entry.vram_free_mb,
            Some(30_000),
            "vram_free_mb must be populated on add"
        );
        assert_eq!(
            entry.max_concurrent,
            Some(8),
            "max_concurrent must be populated on add (Slice 2)"
        );
    }

    /// Test 6: An already-existing agent entry has its telemetry fields refreshed
    /// when reconcile runs a second time (the "update" branch).
    #[tokio::test]
    async fn updated_agent_refreshes_telemetry_fields() {
        let reg = NodeRegistry::new(Duration::from_secs(30));
        let pool = Arc::new(BackendPool::new(vec![], 3, Duration::from_secs(30)));

        // First heartbeat + reconcile — goes through the add branch.
        let caps1 = sample_caps("tele-update");
        reg.heartbeat(caps1).await.unwrap();
        AgentPoolSync::reconcile(&reg, &pool).await;

        // Second heartbeat with different telemetry values, then reconcile via update branch.
        let mut caps2 = sample_caps("tele-update");
        caps2.queue_depth = Some(7);
        caps2.ttft_p50_ms = Some(99);
        caps2.vram_free_mb = 20_000;
        caps2.max_concurrent = Some(16);
        reg.heartbeat(caps2).await.unwrap();
        AgentPoolSync::reconcile(&reg, &pool).await;

        let entry = pool.get("agent:tele-update").await.unwrap();
        assert_eq!(
            entry.queue_depth,
            Some(7),
            "queue_depth must be refreshed on update"
        );
        assert_eq!(
            entry.ttft_p50_ms,
            Some(99),
            "ttft_p50_ms must be refreshed on update"
        );
        assert_eq!(
            entry.vram_free_mb,
            Some(20_000),
            "vram_free_mb must be refreshed on update"
        );
        assert_eq!(
            entry.max_concurrent,
            Some(16),
            "max_concurrent must be refreshed on update"
        );
    }

    /// Test 7: Static and enrolled (`node:`) entries leave all four new fields None
    /// after reconcile — telemetry fields are agent-only.
    #[tokio::test]
    async fn static_and_enrolled_entries_have_no_telemetry() {
        let reg = NodeRegistry::new(Duration::from_secs(30));
        let pool = Arc::new(BackendPool::new(vec![], 3, Duration::from_secs(30)));

        pool.add(Backend {
            name: "static-plain".to_string(),
            url: "http://plain:8080".to_string(),
            priority: 100,
            ..Default::default()
        })
        .await;
        pool.add(Backend {
            name: "node:enrolled-one".to_string(),
            url: "http://enrolled:11434".to_string(),
            priority: 80,
            ..Default::default()
        })
        .await;

        // Reconcile with one live agent — should not disturb the other entries.
        reg.heartbeat(sample_caps("unrelated")).await.unwrap();
        AgentPoolSync::reconcile(&reg, &pool).await;

        let static_entry = pool.get("static-plain").await.unwrap();
        assert_eq!(
            static_entry.queue_depth, None,
            "static queue_depth must be None"
        );
        assert_eq!(
            static_entry.ttft_p50_ms, None,
            "static ttft_p50_ms must be None"
        );
        assert_eq!(
            static_entry.vram_free_mb, None,
            "static vram_free_mb must be None"
        );
        assert_eq!(
            static_entry.max_concurrent, None,
            "static max_concurrent must be None"
        );

        let enrolled_entry = pool.get("node:enrolled-one").await.unwrap();
        assert_eq!(
            enrolled_entry.queue_depth, None,
            "enrolled queue_depth must be None"
        );
        assert_eq!(
            enrolled_entry.ttft_p50_ms, None,
            "enrolled ttft_p50_ms must be None"
        );
        assert_eq!(
            enrolled_entry.vram_free_mb, None,
            "enrolled vram_free_mb must be None"
        );
        assert_eq!(
            enrolled_entry.max_concurrent, None,
            "enrolled max_concurrent must be None"
        );
    }

    /// Test 8: An agent with context_len set propagates it to
    /// BackendState.config.max_context_len via the add branch.
    #[tokio::test]
    async fn agent_add_propagates_context_len_to_max_context_len() {
        let reg = NodeRegistry::new(Duration::from_secs(30));
        let pool = Arc::new(BackendPool::new(vec![], 3, Duration::from_secs(30)));

        let mut caps = sample_caps("ctx-node");
        caps.context_len = Some(32_768);
        reg.heartbeat(caps).await.unwrap();
        AgentPoolSync::reconcile(&reg, &pool).await;

        let entry = pool.get("agent:ctx-node").await.unwrap();
        assert_eq!(
            entry.config.max_context_len,
            Some(32_768),
            "max_context_len must be populated from context_len on add"
        );
    }

    /// Test 9: An existing agent entry has max_context_len refreshed via the
    /// update branch when a new context_len arrives (e.g. after backend restart
    /// with a different --ctx-size).
    #[tokio::test]
    async fn agent_update_refreshes_max_context_len() {
        let reg = NodeRegistry::new(Duration::from_secs(30));
        let pool = Arc::new(BackendPool::new(vec![], 3, Duration::from_secs(30)));

        // First beat: no context_len yet.
        let caps1 = sample_caps("ctx-update");
        reg.heartbeat(caps1).await.unwrap();
        AgentPoolSync::reconcile(&reg, &pool).await;

        let entry = pool.get("agent:ctx-update").await.unwrap();
        assert_eq!(
            entry.config.max_context_len, None,
            "max_context_len must be None before any context_len reported"
        );

        // Second beat: context_len now available.
        let mut caps2 = sample_caps("ctx-update");
        caps2.context_len = Some(65_536);
        reg.heartbeat(caps2).await.unwrap();
        AgentPoolSync::reconcile(&reg, &pool).await;

        let entry = pool.get("agent:ctx-update").await.unwrap();
        assert_eq!(
            entry.config.max_context_len,
            Some(65_536),
            "max_context_len must be refreshed on update branch"
        );
    }

    /// Test 10: A static or enrolled entry leaves max_context_len unaffected by
    /// reconcile (it is agent-only; static entries keep their config as-is).
    #[tokio::test]
    async fn static_entry_max_context_len_unchanged_by_reconcile() {
        let reg = NodeRegistry::new(Duration::from_secs(30));
        let pool = Arc::new(BackendPool::new(vec![], 3, Duration::from_secs(30)));

        pool.add(Backend {
            name: "static-ctx".to_string(),
            url: "http://static:8080".to_string(),
            priority: 100,
            max_context_len: Some(4096),
            ..Default::default()
        })
        .await;

        // Reconcile with a live agent — static entry must be untouched.
        reg.heartbeat(sample_caps("other")).await.unwrap();
        AgentPoolSync::reconcile(&reg, &pool).await;

        let entry = pool.get("static-ctx").await.unwrap();
        assert_eq!(
            entry.config.max_context_len,
            Some(4096),
            "static max_context_len must not be altered by agent reconcile"
        );
    }
}
