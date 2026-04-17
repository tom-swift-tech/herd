use crate::config::BackendType;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCapabilities {
    pub node_id: String,
    pub backend: BackendType,
    pub address: String,
    #[serde(default)]
    pub gpu_model: Option<String>,
    pub vram_total_mb: u64,
    pub vram_free_mb: u64,
    #[serde(default)]
    pub models_loaded: Vec<String>,
    #[serde(default)]
    pub queue_depth: u32,
    #[serde(default)]
    pub ttft_p50_ms: Option<u32>,
    #[serde(default)]
    pub rpc_capable: bool,
    #[serde(default)]
    pub rpc_port: Option<u16>,
    pub agent_version: String,
}

#[derive(Debug, Clone)]
pub struct AgentState {
    pub capabilities: AgentCapabilities,
    pub last_heartbeat: Instant,
}

impl AgentState {
    pub fn age(&self) -> Duration {
        self.last_heartbeat.elapsed()
    }

    pub fn is_fresh(&self, ttl: Duration) -> bool {
        self.age() <= ttl
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeartbeatOutcome {
    Registered,
    Updated,
}

type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

#[derive(Clone)]
pub struct NodeRegistry {
    nodes: Arc<RwLock<HashMap<String, AgentState>>>,
    ttl: Duration,
    clock: Clock,
}

impl NodeRegistry {
    pub fn new(ttl: Duration) -> Self {
        Self::with_clock(ttl, Arc::new(Instant::now))
    }

    fn with_clock(ttl: Duration, clock: Clock) -> Self {
        Self {
            nodes: Arc::new(RwLock::new(HashMap::new())),
            ttl,
            clock,
        }
    }

    pub async fn heartbeat(&self, caps: AgentCapabilities) -> HeartbeatOutcome {
        let now = (self.clock)();
        let mut nodes = self.nodes.write().await;
        let node_id = caps.node_id.clone();
        match nodes.get_mut(&node_id) {
            Some(existing) => {
                existing.capabilities = caps;
                existing.last_heartbeat = now;
                HeartbeatOutcome::Updated
            }
            None => {
                nodes.insert(
                    node_id,
                    AgentState {
                        capabilities: caps,
                        last_heartbeat: now,
                    },
                );
                HeartbeatOutcome::Registered
            }
        }
    }

    pub async fn evict_stale(&self) -> Vec<String> {
        let now = (self.clock)();
        let ttl = self.ttl;
        let mut nodes = self.nodes.write().await;
        let mut evicted: Vec<String> = nodes
            .iter()
            .filter(|(_, state)| now.duration_since(state.last_heartbeat) > ttl)
            .map(|(id, _)| id.clone())
            .collect();
        evicted.sort();
        for id in &evicted {
            nodes.remove(id);
        }
        evicted
    }

    pub async fn list(&self) -> Vec<AgentState> {
        self.nodes.read().await.values().cloned().collect()
    }

    pub async fn fresh_nodes(&self) -> Vec<AgentState> {
        let now = (self.clock)();
        let ttl = self.ttl;
        self.nodes
            .read()
            .await
            .values()
            .filter(|state| now.duration_since(state.last_heartbeat) <= ttl)
            .cloned()
            .collect()
    }

    pub async fn get(&self, node_id: &str) -> Option<AgentState> {
        self.nodes.read().await.get(node_id).cloned()
    }

    pub async fn len(&self) -> usize {
        self.nodes.read().await.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.nodes.read().await.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn sample_caps(node_id: &str) -> AgentCapabilities {
        AgentCapabilities {
            node_id: node_id.to_string(),
            backend: BackendType::LlamaServer,
            address: "http://127.0.0.1:8080".to_string(),
            gpu_model: Some("RTX 5090".to_string()),
            vram_total_mb: 32_768,
            vram_free_mb: 30_000,
            models_loaded: vec!["llama-3-8b".to_string()],
            queue_depth: 0,
            ttft_p50_ms: Some(42),
            rpc_capable: false,
            rpc_port: None,
            agent_version: "1.2.0".to_string(),
        }
    }

    /// Test-only manual clock that `NodeRegistry` can sample. Uses a shared
    /// mutex-protected `Instant` that tests advance explicitly. Keeps
    /// `Instant` (monotonic) semantics so production code is unchanged.
    #[derive(Clone)]
    struct TestClock {
        now: Arc<Mutex<Instant>>,
    }

    impl TestClock {
        fn new() -> Self {
            Self {
                now: Arc::new(Mutex::new(Instant::now())),
            }
        }

        fn advance(&self, delta: Duration) {
            let mut guard = self.now.lock().unwrap();
            *guard += delta;
        }

        fn as_fn(&self) -> Clock {
            let now = self.now.clone();
            Arc::new(move || *now.lock().unwrap())
        }
    }

    fn registry_with_clock(ttl: Duration) -> (NodeRegistry, TestClock) {
        let clock = TestClock::new();
        let reg = NodeRegistry::with_clock(ttl, clock.as_fn());
        (reg, clock)
    }

    #[tokio::test]
    async fn heartbeat_on_unknown_returns_registered() {
        let (reg, _clock) = registry_with_clock(Duration::from_secs(30));
        let outcome = reg.heartbeat(sample_caps("a")).await;
        assert_eq!(outcome, HeartbeatOutcome::Registered);
        assert_eq!(reg.len().await, 1);
    }

    #[tokio::test]
    async fn heartbeat_on_known_returns_updated() {
        let (reg, _clock) = registry_with_clock(Duration::from_secs(30));
        reg.heartbeat(sample_caps("a")).await;
        let outcome = reg.heartbeat(sample_caps("a")).await;
        assert_eq!(outcome, HeartbeatOutcome::Updated);
        assert_eq!(reg.len().await, 1);
    }

    #[tokio::test]
    async fn heartbeat_updates_last_heartbeat_timestamp() {
        let (reg, clock) = registry_with_clock(Duration::from_secs(30));
        reg.heartbeat(sample_caps("a")).await;
        let first_ts = reg.get("a").await.unwrap().last_heartbeat;
        clock.advance(Duration::from_secs(5));
        reg.heartbeat(sample_caps("a")).await;
        let second_ts = reg.get("a").await.unwrap().last_heartbeat;
        assert!(second_ts > first_ts);
    }

    #[tokio::test]
    async fn evict_stale_removes_nodes_past_ttl() {
        let (reg, clock) = registry_with_clock(Duration::from_secs(30));
        reg.heartbeat(sample_caps("a")).await;
        clock.advance(Duration::from_secs(31));
        let evicted = reg.evict_stale().await;
        assert_eq!(evicted, vec!["a".to_string()]);
        assert_eq!(reg.len().await, 0);
    }

    #[tokio::test]
    async fn evict_stale_keeps_fresh_nodes() {
        let (reg, clock) = registry_with_clock(Duration::from_secs(30));
        reg.heartbeat(sample_caps("a")).await;
        clock.advance(Duration::from_secs(10));
        let evicted = reg.evict_stale().await;
        assert!(evicted.is_empty());
        assert_eq!(reg.len().await, 1);
    }

    #[tokio::test]
    async fn fresh_nodes_excludes_stale_but_not_yet_evicted() {
        let (reg, clock) = registry_with_clock(Duration::from_secs(30));
        reg.heartbeat(sample_caps("a")).await;
        clock.advance(Duration::from_secs(31));
        assert!(reg.fresh_nodes().await.is_empty());
        assert_eq!(reg.list().await.len(), 1);
    }

    #[tokio::test]
    async fn agent_state_is_fresh_respects_ttl() {
        let fresh = AgentState {
            capabilities: sample_caps("a"),
            last_heartbeat: Instant::now(),
        };
        assert!(fresh.is_fresh(Duration::from_secs(30)));

        let stale = AgentState {
            capabilities: sample_caps("a"),
            last_heartbeat: Instant::now() - Duration::from_secs(31),
        };
        assert!(!stale.is_fresh(Duration::from_secs(30)));
    }

    #[tokio::test]
    async fn list_returns_all_current_states() {
        let (reg, _clock) = registry_with_clock(Duration::from_secs(30));
        reg.heartbeat(sample_caps("a")).await;
        reg.heartbeat(sample_caps("b")).await;
        reg.heartbeat(sample_caps("c")).await;
        assert_eq!(reg.list().await.len(), 3);
    }

    #[tokio::test]
    async fn get_returns_none_for_unknown() {
        let (reg, _clock) = registry_with_clock(Duration::from_secs(30));
        assert!(reg.get("missing").await.is_none());
        reg.heartbeat(sample_caps("a")).await;
        assert!(reg.get("a").await.is_some());
    }

    #[tokio::test]
    async fn heartbeat_re_registers_after_eviction() {
        let (reg, clock) = registry_with_clock(Duration::from_secs(30));
        reg.heartbeat(sample_caps("a")).await;
        clock.advance(Duration::from_secs(31));
        let evicted = reg.evict_stale().await;
        assert_eq!(evicted, vec!["a".to_string()]);
        let outcome = reg.heartbeat(sample_caps("a")).await;
        assert_eq!(outcome, HeartbeatOutcome::Registered);
    }
}
