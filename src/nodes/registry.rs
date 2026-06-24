use crate::config::BackendType;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

const DEFAULT_MAX_AGENT_NODES: usize = 1024;
/// Default extra time an agent that announced a self-update restart is
/// protected from eviction (override via HERD_AGENT_UPDATE_GRACE_SECS,
/// applied where the registry is constructed).
pub const DEFAULT_UPDATE_GRACE: Duration = Duration::from_secs(180);

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
    /// Measured in-flight request count (`None` = backend can't report it).
    /// Wire-compat: a pre-Slice-2 agent sends `0` → deserializes to `Some(0)`.
    #[serde(default)]
    pub queue_depth: Option<u32>,
    #[serde(default)]
    pub ttft_p50_ms: Option<u32>,
    /// Backend concurrency limit (llama-server `total_slots`); `None` when
    /// unavailable. `#[serde(default)]` keeps pre-Slice-2 agents wire-compatible.
    #[serde(default)]
    pub max_concurrent: Option<u32>,
    /// Served context-window size from llama-server `/props`
    /// `default_generation_settings.n_ctx`. `None` for Ollama, openai-compat,
    /// or when the probe could not read the field.
    /// `#[serde(default)]` → older agents that don't send this field
    /// deserialize to `None` (wire-compatible).
    #[serde(default)]
    pub context_len: Option<u32>,
    #[serde(default)]
    pub rpc_capable: bool,
    #[serde(default)]
    pub rpc_port: Option<u16>,
    pub agent_version: String,
    /// Agent platform (`std::env::consts::OS`, e.g. "windows" | "linux" |
    /// "macos"). Optional for wire-compat with pre-PR#6 agents; the gateway
    /// only offers binary downloads when both `os` and `arch` are reported.
    #[serde(default)]
    pub os: Option<String>,
    /// Agent CPU architecture (`std::env::consts::ARCH`, e.g. "x86_64").
    #[serde(default)]
    pub arch: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AgentState {
    pub capabilities: AgentCapabilities,
    pub last_heartbeat: Instant,
    /// Set when the agent's latest beat announced a self-update restart
    /// (`updating: true`); cleared by the next normal beat. While set,
    /// `evict_stale` grants the node a grace window beyond the TTL so the
    /// restart gap doesn't read as an outage.
    pub updating_since: Option<Instant>,
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
    Updated {
        /// True when this beat's `models_loaded` set differs from the previous
        /// one. Lets callers persist on material change without diffing state
        /// themselves (the registry already holds both snapshots under the
        /// write lock).
        models_changed: bool,
        /// True when this beat's `agent_version` differs from the previous
        /// one — the restarted agent's first beat after a self-update.
        /// Callers must persist on this too, or the Fleet row stays stuck at
        /// 'updating' with the old version.
        version_changed: bool,
        /// True exactly when this beat cleared a previously-armed
        /// `updating_since` — i.e. the agent resumed normal beats after
        /// announcing a restart. Callers must persist on this so the Fleet
        /// row un-sticks from `'updating'` even when the version didn't
        /// change (e.g. a failed respawn that kept the old binary running).
        update_cleared: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryError {
    CapacityExceeded { max_nodes: usize },
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CapacityExceeded { max_nodes } => {
                write!(f, "agent node registry capacity exceeded ({max_nodes})")
            }
        }
    }
}

impl std::error::Error for RegistryError {}

/// Pluggable monotonic clock. `#[doc(hidden)] pub` (not `pub(crate)`) so
/// out-of-crate integration tests can inject a controllable clock via
/// [`NodeRegistry::with_clock`] and drive TTL behavior without wall-clock
/// sleeps. Production always uses `Instant::now`.
#[doc(hidden)]
pub type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// Order-insensitive comparison of two `models_loaded` lists. Agents may report
/// the same set in a different order between beats; that is not a material change.
fn same_model_set(a: &[String], b: &[String]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut a_sorted: Vec<&str> = a.iter().map(String::as_str).collect();
    let mut b_sorted: Vec<&str> = b.iter().map(String::as_str).collect();
    a_sorted.sort_unstable();
    b_sorted.sort_unstable();
    a_sorted == b_sorted
}

#[derive(Clone)]
pub struct NodeRegistry {
    nodes: Arc<RwLock<HashMap<String, AgentState>>>,
    ttl: Duration,
    max_nodes: usize,
    update_grace: Duration,
    clock: Clock,
}

impl NodeRegistry {
    pub fn new(ttl: Duration) -> Self {
        Self::with_clock(ttl, Arc::new(Instant::now))
    }

    pub fn with_max_nodes(ttl: Duration, max_nodes: usize) -> Self {
        Self::with_clock_and_max(ttl, Arc::new(Instant::now), max_nodes)
    }

    /// Construct with an injected [`Clock`]. `#[doc(hidden)] pub` so integration
    /// tests in `tests/` (an external crate that links the non-`cfg(test)` lib,
    /// where the in-crate `test_clock` helper is invisible) can drive TTL-based
    /// freshness deterministically. Production uses [`NodeRegistry::new`].
    #[doc(hidden)]
    pub fn with_clock(ttl: Duration, clock: Clock) -> Self {
        Self::with_clock_and_max(ttl, clock, DEFAULT_MAX_AGENT_NODES)
    }

    fn with_clock_and_max(ttl: Duration, clock: Clock, max_nodes: usize) -> Self {
        Self {
            nodes: Arc::new(RwLock::new(HashMap::new())),
            ttl,
            max_nodes: max_nodes.max(1),
            update_grace: DEFAULT_UPDATE_GRACE,
            clock,
        }
    }

    /// Override the self-update eviction grace (HERD_AGENT_UPDATE_GRACE_SECS
    /// is resolved by the caller — the registry never reads env itself).
    pub fn with_update_grace(mut self, grace: Duration) -> Self {
        self.update_grace = grace;
        self
    }

    /// Record a normal heartbeat (no self-update restart announced).
    pub async fn heartbeat(
        &self,
        caps: AgentCapabilities,
    ) -> Result<HeartbeatOutcome, RegistryError> {
        self.heartbeat_with(caps, false).await
    }

    /// Record a heartbeat. `updating: true` is the agent's announcement that
    /// it is about to restart for a self-update — it stamps `updating_since`
    /// so `evict_stale` grants the node a grace window; any normal beat
    /// clears it.
    pub async fn heartbeat_with(
        &self,
        caps: AgentCapabilities,
        updating: bool,
    ) -> Result<HeartbeatOutcome, RegistryError> {
        let now = (self.clock)();
        let mut nodes = self.nodes.write().await;
        let node_id = caps.node_id.clone();
        match nodes.get_mut(&node_id) {
            Some(existing) => {
                let models_changed =
                    !same_model_set(&existing.capabilities.models_loaded, &caps.models_loaded);
                let version_changed = existing.capabilities.agent_version != caps.agent_version;
                // True when this normal beat clears a previously-armed
                // updating_since (agent resumed after restart announcement).
                // Computed BEFORE mutating updating_since so the check sees
                // the previous state.
                let update_cleared = !updating && existing.updating_since.is_some();
                existing.capabilities = caps;
                existing.last_heartbeat = now;
                if updating {
                    // Keep the first announcement's timestamp: a stuck agent
                    // re-sending updating beats must not extend its grace
                    // window indefinitely.
                    existing.updating_since.get_or_insert(now);
                } else {
                    existing.updating_since = None;
                }
                Ok(HeartbeatOutcome::Updated {
                    models_changed,
                    version_changed,
                    update_cleared,
                })
            }
            None => {
                if nodes.len() >= self.max_nodes {
                    return Err(RegistryError::CapacityExceeded {
                        max_nodes: self.max_nodes,
                    });
                }
                nodes.insert(
                    node_id,
                    AgentState {
                        capabilities: caps,
                        last_heartbeat: now,
                        updating_since: updating.then_some(now),
                    },
                );
                Ok(HeartbeatOutcome::Registered)
            }
        }
    }

    pub async fn evict_stale(&self) -> Vec<String> {
        let now = (self.clock)();
        let ttl = self.ttl;
        let grace = self.update_grace;
        let mut nodes = self.nodes.write().await;
        let mut evicted: Vec<String> = nodes
            .iter()
            .filter(|(_, state)| {
                let stale = now.duration_since(state.last_heartbeat) > ttl;
                // A node mid-self-update gets `update_grace` from its
                // announcement before normal TTL eviction resumes.
                let in_update_grace = state
                    .updating_since
                    .is_some_and(|since| now.duration_since(since) <= grace);
                stale && !in_update_grace
            })
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

/// Test-only manual clock that `NodeRegistry` can sample. Uses a shared
/// mutex-protected `Instant` that tests advance explicitly. Keeps `Instant`
/// (monotonic) semantics so production code is unchanged. Shared across the
/// `nodes` test modules (registry + pool_sync) so TTL-driven behavior is tested
/// deterministically without wall-clock sleeps.
#[cfg(test)]
pub(crate) mod test_clock {
    use super::Clock;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    #[derive(Clone)]
    pub(crate) struct TestClock {
        now: Arc<Mutex<Instant>>,
    }

    impl TestClock {
        pub(crate) fn new() -> Self {
            Self {
                now: Arc::new(Mutex::new(Instant::now())),
            }
        }

        pub(crate) fn advance(&self, delta: Duration) {
            let mut guard = self.now.lock().unwrap();
            *guard += delta;
        }

        pub(crate) fn as_fn(&self) -> Clock {
            let now = self.now.clone();
            Arc::new(move || *now.lock().unwrap())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_clock::TestClock;
    use super::*;

    fn sample_caps(node_id: &str) -> AgentCapabilities {
        AgentCapabilities {
            node_id: node_id.to_string(),
            backend: BackendType::LlamaServer,
            address: "http://127.0.0.1:8080".to_string(),
            gpu_model: Some("RTX 5090".to_string()),
            vram_total_mb: 32_768,
            vram_free_mb: 30_000,
            models_loaded: vec!["llama-3-8b".to_string()],
            queue_depth: Some(0),
            ttft_p50_ms: Some(42),
            max_concurrent: Some(4),
            context_len: None,
            rpc_capable: false,
            rpc_port: None,
            agent_version: "1.2.0".to_string(),
            os: Some("linux".to_string()),
            arch: Some("x86_64".to_string()),
        }
    }

    fn registry_with_clock(ttl: Duration) -> (NodeRegistry, TestClock) {
        let clock = TestClock::new();
        let reg = NodeRegistry::with_clock(ttl, clock.as_fn());
        (reg, clock)
    }

    fn registry_with_clock_and_max(ttl: Duration, max_nodes: usize) -> (NodeRegistry, TestClock) {
        let clock = TestClock::new();
        let reg = NodeRegistry::with_clock_and_max(ttl, clock.as_fn(), max_nodes);
        (reg, clock)
    }

    #[tokio::test]
    async fn heartbeat_on_unknown_returns_registered() {
        let (reg, _clock) = registry_with_clock(Duration::from_secs(30));
        let outcome = reg.heartbeat(sample_caps("a")).await.unwrap();
        assert_eq!(outcome, HeartbeatOutcome::Registered);
        assert_eq!(reg.len().await, 1);
    }

    #[tokio::test]
    async fn heartbeat_on_known_returns_updated() {
        let (reg, _clock) = registry_with_clock(Duration::from_secs(30));
        reg.heartbeat(sample_caps("a")).await.unwrap();
        let outcome = reg.heartbeat(sample_caps("a")).await.unwrap();
        assert_eq!(
            outcome,
            HeartbeatOutcome::Updated {
                models_changed: false,
                version_changed: false,
                update_cleared: false,
            }
        );
        assert_eq!(reg.len().await, 1);
    }

    #[tokio::test]
    async fn heartbeat_reports_models_changed_on_set_difference() {
        let (reg, _clock) = registry_with_clock(Duration::from_secs(30));
        reg.heartbeat(sample_caps("a")).await.unwrap();

        let mut caps = sample_caps("a");
        caps.models_loaded = vec!["llama-3-8b".to_string(), "qwen3-32b".to_string()];
        let outcome = reg.heartbeat(caps).await.unwrap();
        assert_eq!(
            outcome,
            HeartbeatOutcome::Updated {
                models_changed: true,
                version_changed: false,
                update_cleared: false,
            }
        );
    }

    #[tokio::test]
    async fn heartbeat_ignores_model_order_and_dynamic_fields() {
        let (reg, _clock) = registry_with_clock(Duration::from_secs(30));
        let mut caps = sample_caps("a");
        caps.models_loaded = vec!["m1".to_string(), "m2".to_string()];
        reg.heartbeat(caps).await.unwrap();

        // Same set, different order, different dynamic load — not material.
        let mut caps = sample_caps("a");
        caps.models_loaded = vec!["m2".to_string(), "m1".to_string()];
        caps.vram_free_mb = 1;
        caps.queue_depth = Some(99);
        let outcome = reg.heartbeat(caps).await.unwrap();
        assert_eq!(
            outcome,
            HeartbeatOutcome::Updated {
                models_changed: false,
                version_changed: false,
                update_cleared: false,
            }
        );
    }

    #[tokio::test]
    async fn heartbeat_updates_last_heartbeat_timestamp() {
        let (reg, clock) = registry_with_clock(Duration::from_secs(30));
        reg.heartbeat(sample_caps("a")).await.unwrap();
        let first_ts = reg.get("a").await.unwrap().last_heartbeat;
        clock.advance(Duration::from_secs(5));
        reg.heartbeat(sample_caps("a")).await.unwrap();
        let second_ts = reg.get("a").await.unwrap().last_heartbeat;
        assert!(second_ts > first_ts);
    }

    #[tokio::test]
    async fn evict_stale_removes_nodes_past_ttl() {
        let (reg, clock) = registry_with_clock(Duration::from_secs(30));
        reg.heartbeat(sample_caps("a")).await.unwrap();
        clock.advance(Duration::from_secs(31));
        let evicted = reg.evict_stale().await;
        assert_eq!(evicted, vec!["a".to_string()]);
        assert_eq!(reg.len().await, 0);
    }

    #[tokio::test]
    async fn evict_stale_keeps_fresh_nodes() {
        let (reg, clock) = registry_with_clock(Duration::from_secs(30));
        reg.heartbeat(sample_caps("a")).await.unwrap();
        clock.advance(Duration::from_secs(10));
        let evicted = reg.evict_stale().await;
        assert!(evicted.is_empty());
        assert_eq!(reg.len().await, 1);
    }

    #[tokio::test]
    async fn fresh_nodes_excludes_stale_but_not_yet_evicted() {
        let (reg, clock) = registry_with_clock(Duration::from_secs(30));
        reg.heartbeat(sample_caps("a")).await.unwrap();
        clock.advance(Duration::from_secs(31));
        assert!(reg.fresh_nodes().await.is_empty());
        assert_eq!(reg.list().await.len(), 1);
    }

    #[tokio::test]
    async fn agent_state_is_fresh_respects_ttl() {
        let fresh = AgentState {
            capabilities: sample_caps("a"),
            last_heartbeat: Instant::now(),
            updating_since: None,
        };
        assert!(fresh.is_fresh(Duration::from_secs(30)));

        let stale = AgentState {
            capabilities: sample_caps("a"),
            last_heartbeat: Instant::now() - Duration::from_secs(31),
            updating_since: None,
        };
        assert!(!stale.is_fresh(Duration::from_secs(30)));
    }

    #[tokio::test]
    async fn list_returns_all_current_states() {
        let (reg, _clock) = registry_with_clock(Duration::from_secs(30));
        reg.heartbeat(sample_caps("a")).await.unwrap();
        reg.heartbeat(sample_caps("b")).await.unwrap();
        reg.heartbeat(sample_caps("c")).await.unwrap();
        assert_eq!(reg.list().await.len(), 3);
    }

    #[tokio::test]
    async fn get_returns_none_for_unknown() {
        let (reg, _clock) = registry_with_clock(Duration::from_secs(30));
        assert!(reg.get("missing").await.is_none());
        reg.heartbeat(sample_caps("a")).await.unwrap();
        assert!(reg.get("a").await.is_some());
    }

    #[tokio::test]
    async fn heartbeat_re_registers_after_eviction() {
        let (reg, clock) = registry_with_clock(Duration::from_secs(30));
        reg.heartbeat(sample_caps("a")).await.unwrap();
        clock.advance(Duration::from_secs(31));
        let evicted = reg.evict_stale().await;
        assert_eq!(evicted, vec!["a".to_string()]);
        let outcome = reg.heartbeat(sample_caps("a")).await.unwrap();
        assert_eq!(outcome, HeartbeatOutcome::Registered);
    }

    #[tokio::test]
    async fn heartbeat_rejects_new_node_when_registry_is_full() {
        let (reg, _clock) = registry_with_clock_and_max(Duration::from_secs(30), 1);
        reg.heartbeat(sample_caps("a")).await.unwrap();

        let err = reg.heartbeat(sample_caps("b")).await.unwrap_err();
        assert_eq!(err, RegistryError::CapacityExceeded { max_nodes: 1 });
        assert_eq!(reg.len().await, 1);
    }

    #[tokio::test]
    async fn heartbeat_allows_existing_node_update_when_registry_is_full() {
        let (reg, _clock) = registry_with_clock_and_max(Duration::from_secs(30), 1);
        reg.heartbeat(sample_caps("a")).await.unwrap();

        let outcome = reg.heartbeat(sample_caps("a")).await.unwrap();
        assert_eq!(
            outcome,
            HeartbeatOutcome::Updated {
                models_changed: false,
                version_changed: false,
                update_cleared: false,
            }
        );
        assert_eq!(reg.len().await, 1);
    }

    // ---- self-update grace and version-change reporting (PR #6b) ----

    fn caps_with_version(node_id: &str, version: &str) -> AgentCapabilities {
        let mut caps = sample_caps(node_id);
        caps.agent_version = version.to_string();
        caps
    }

    #[tokio::test]
    async fn heartbeat_reports_version_changed_after_update() {
        let (reg, _clock) = registry_with_clock(Duration::from_secs(30));
        reg.heartbeat(caps_with_version("a", "1.2.0"))
            .await
            .unwrap();
        let outcome = reg
            .heartbeat(caps_with_version("a", "1.3.0"))
            .await
            .unwrap();
        assert_eq!(
            outcome,
            HeartbeatOutcome::Updated {
                models_changed: false,
                version_changed: true,
                update_cleared: false,
            }
        );
    }

    #[tokio::test]
    async fn updating_beat_sets_and_normal_beat_clears_updating_since() {
        let (reg, _clock) = registry_with_clock(Duration::from_secs(30));
        reg.heartbeat(sample_caps("a")).await.unwrap();
        assert!(reg.get("a").await.unwrap().updating_since.is_none());

        reg.heartbeat_with(sample_caps("a"), true).await.unwrap();
        assert!(reg.get("a").await.unwrap().updating_since.is_some());

        reg.heartbeat(sample_caps("a")).await.unwrap();
        assert!(reg.get("a").await.unwrap().updating_since.is_none());
    }

    #[tokio::test]
    async fn repeated_updating_beats_keep_the_first_announcement_instant() {
        let (reg, clock) = registry_with_clock(Duration::from_secs(30));
        reg.heartbeat_with(sample_caps("a"), true).await.unwrap();
        let first = reg.get("a").await.unwrap().updating_since.unwrap();
        clock.advance(Duration::from_secs(10));
        reg.heartbeat_with(sample_caps("a"), true).await.unwrap();
        assert_eq!(
            reg.get("a").await.unwrap().updating_since,
            Some(first),
            "a stuck agent must not extend its own grace window"
        );
    }

    #[tokio::test]
    async fn evict_stale_spares_updating_node_within_grace() {
        let (reg, clock) = registry_with_clock(Duration::from_secs(30));
        reg.heartbeat_with(sample_caps("a"), true).await.unwrap();

        // Well past the 30s TTL but inside the default 180s update grace.
        clock.advance(Duration::from_secs(120));
        assert!(reg.evict_stale().await.is_empty());
        assert_eq!(reg.len().await, 1);
    }

    #[tokio::test]
    async fn evict_stale_evicts_updating_node_after_grace_expires() {
        let (reg, clock) = registry_with_clock(Duration::from_secs(30));
        let reg = reg.with_update_grace(Duration::from_secs(60));
        reg.heartbeat_with(sample_caps("a"), true).await.unwrap();

        clock.advance(Duration::from_secs(61));
        assert_eq!(reg.evict_stale().await, vec!["a".to_string()]);
        assert_eq!(reg.len().await, 0);
    }

    #[tokio::test]
    async fn normal_beat_after_update_restores_ttl_eviction() {
        let (reg, clock) = registry_with_clock(Duration::from_secs(30));
        reg.heartbeat_with(sample_caps("a"), true).await.unwrap();
        clock.advance(Duration::from_secs(10));
        // Restarted agent beats normally — grace is gone, plain TTL applies.
        reg.heartbeat(caps_with_version("a", "1.3.0"))
            .await
            .unwrap();
        clock.advance(Duration::from_secs(31));
        assert_eq!(reg.evict_stale().await, vec!["a".to_string()]);
    }

    #[tokio::test]
    async fn updating_node_within_grace_is_not_fresh_for_routing() {
        // Grace protects against *eviction* only — a restarting node must
        // still drop out of the routable set as soon as its TTL lapses.
        let (reg, clock) = registry_with_clock(Duration::from_secs(30));
        reg.heartbeat_with(sample_caps("a"), true).await.unwrap();
        clock.advance(Duration::from_secs(31));
        assert!(reg.fresh_nodes().await.is_empty());
        assert_eq!(reg.list().await.len(), 1);
    }

    // ---- update_cleared field (PR #6b failed-respawn fix) ----

    #[tokio::test]
    async fn update_cleared_is_false_on_updating_beat() {
        let (reg, _clock) = registry_with_clock(Duration::from_secs(30));
        reg.heartbeat(sample_caps("a")).await.unwrap();
        let outcome = reg.heartbeat_with(sample_caps("a"), true).await.unwrap();
        assert_eq!(
            outcome,
            HeartbeatOutcome::Updated {
                models_changed: false,
                version_changed: false,
                update_cleared: false,
            },
            "an updating beat must not report update_cleared"
        );
    }

    #[tokio::test]
    async fn update_cleared_is_true_on_first_normal_beat_after_updating() {
        let (reg, _clock) = registry_with_clock(Duration::from_secs(30));
        reg.heartbeat(sample_caps("a")).await.unwrap();
        reg.heartbeat_with(sample_caps("a"), true).await.unwrap();

        // First normal beat after an updating beat must clear the flag.
        let outcome = reg.heartbeat(sample_caps("a")).await.unwrap();
        assert_eq!(
            outcome,
            HeartbeatOutcome::Updated {
                models_changed: false,
                version_changed: false,
                update_cleared: true,
            },
            "first normal beat after updating must report update_cleared=true"
        );
    }

    #[tokio::test]
    async fn update_cleared_is_false_on_subsequent_normal_beats() {
        let (reg, _clock) = registry_with_clock(Duration::from_secs(30));
        reg.heartbeat(sample_caps("a")).await.unwrap();
        reg.heartbeat_with(sample_caps("a"), true).await.unwrap();
        // Clear beat.
        reg.heartbeat(sample_caps("a")).await.unwrap();
        // Subsequent steady beat: updating_since is already None, nothing to clear.
        let outcome = reg.heartbeat(sample_caps("a")).await.unwrap();
        assert_eq!(
            outcome,
            HeartbeatOutcome::Updated {
                models_changed: false,
                version_changed: false,
                update_cleared: false,
            },
            "steady-state beat after clearing must not report update_cleared"
        );
    }
}
