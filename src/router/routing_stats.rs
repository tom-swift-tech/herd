/// Per-(backend, model) EWMA history store for Phase-3 scoring dims 14–17.
///
/// Writes happen **off-path** (post-request hook). Reads happen **on-path** inside
/// `ScoredRouter::route_scored`; the caller takes ONE read-lock snapshot and looks
/// up each candidate. No wall-clock reads — decay is request-count-based only.
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::RwLock;

// ── Constants ────────────────────────────────────────────────────────────────

/// EWMA smoothing factor. Each new sample contributes 20% weight; history 80%.
pub const ALPHA: f64 = 0.2;

/// Minimum observed samples before dims 14–17 are considered trustworthy enough
/// to participate in scoring. Under this threshold the key is treated as absent
/// → neutral 0.5, weight-dropped — no cold-start penalty.
pub const MIN_SAMPLES: u32 = 5;

/// Reference latency (ms) for dim 14. A backend at this latency normalises to 0.0;
/// one near 0 ms normalises to 1.0.
pub const LAT_REF: f64 = 5_000.0;

/// Reference tokens/sec for dim 16. A backend at or above this normalises to 1.0.
pub const TPS_REF: f64 = 100.0;

/// Reference health-transition count for dim 17. At this many transitions the
/// backend normalises to 0.0 (maximally flappy).
pub const FLAP_REF: f64 = 10.0;

/// Width of the rolling error ring, in samples (bits packed into a u32).
pub const ERR_RING_WIDTH: u32 = 32;

/// Soft ceiling on the number of (backend, model) entries. At the ceiling the
/// least-recently-updated entry is evicted before inserting the new one — NOT a
/// silent drop. Evicted pairs re-warm from cold-start neutrality.
pub const MAX_ROUTING_STATS: usize = 20_000;

// ── Per-entry statistics ─────────────────────────────────────────────────────

/// Per-(backend, model) statistics maintained by the post-request hook.
#[derive(Clone, Debug)]
pub struct BackendModelStats {
    /// EWMA of end-to-end request latency in ms (alpha = ALPHA).
    pub ewma_latency_ms: f64,
    /// EWMA of output tokens/sec (alpha = ALPHA).
    pub ewma_tps: f64,
    /// Rolling error ring: the 32 most-recent requests packed as bits. A 1-bit
    /// means that slot was an error; 0 means success. Shifted left on each update.
    pub err_window: u32,
    /// Total observed samples (updates). Gates the cold-start threshold.
    pub samples: u32,
    /// Running count of health-state transitions observed on this (backend, model).
    /// Incremented externally when the pool marks a backend healthy↔unhealthy.
    pub health_transitions: u32,
    /// Monotonic update-tick counter (== total request count across all entries at
    /// the time of the last update). Used for LRU eviction at the ceiling. Never
    /// wall-clock — request-count-based only, preserving scoring determinism.
    pub last_update_tick: u64,
}

impl Default for BackendModelStats {
    fn default() -> Self {
        Self {
            ewma_latency_ms: 0.0,
            ewma_tps: 0.0,
            err_window: 0,
            samples: 0,
            health_transitions: 0,
            last_update_tick: 0,
        }
    }
}

impl BackendModelStats {
    /// Compute the recent error rate from the rolling ring (fraction of 1-bits).
    ///
    /// Returns a value in `[0.0, 1.0]`. With `samples < ERR_RING_WIDTH` only the
    /// first `samples` bits are meaningful; the rest are zero-initialised and count
    /// as successes — a conservative (optimistic) estimate until the ring fills.
    #[inline]
    pub fn error_rate(&self) -> f64 {
        let errors = self.err_window.count_ones();
        let total = self.samples.min(ERR_RING_WIDTH);
        if total == 0 {
            0.0
        } else {
            errors as f64 / total as f64
        }
    }
}

// ── Store ────────────────────────────────────────────────────────────────────

/// Shared, in-memory per-(backend, model) history store.
///
/// `BTreeMap` for deterministic iteration (house rule). Keyed by
/// `(backend_name, model_name)` — both owned `String`s.
#[derive(Clone, Debug, Default)]
pub struct RoutingStats {
    inner: Arc<RwLock<Inner>>,
}

#[derive(Debug, Default)]
struct Inner {
    map: BTreeMap<(String, String), BackendModelStats>,
    /// Global monotonic tick counter. Incremented once per `update()` call.
    /// Each entry records the tick at its last write; the LRU entry (lowest tick
    /// among all entries) is evicted when the ceiling is reached.
    tick: u64,
}

impl RoutingStats {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a completed request outcome for (backend, model).
    ///
    /// Called on the **post-request hook** — off the scoring path. Updates the
    /// EWMA latency, EWMA tps (when tokens_out is available), and the error ring.
    /// Evicts the LRU entry when the ceiling is reached.
    pub async fn update(
        &self,
        backend: &str,
        model: &str,
        duration_ms: u64,
        is_error: bool,
        tokens_out: Option<u32>,
    ) {
        let mut guard = self.inner.write().await;
        let tick = guard.tick + 1;
        guard.tick = tick;

        let key = (backend.to_string(), model.to_string());

        // Evict LRU entry when we would exceed the ceiling on a NEW key.
        if !guard.map.contains_key(&key) && guard.map.len() >= MAX_ROUTING_STATS {
            // Find the entry with the smallest last_update_tick.
            let lru_key = guard
                .map
                .iter()
                .min_by_key(|(_, v)| v.last_update_tick)
                .map(|(k, _)| k.clone());
            if let Some(k) = lru_key {
                guard.map.remove(&k);
            }
        }

        let entry = guard.map.entry(key).or_default();

        // Update EWMA latency.
        let lat = duration_ms as f64;
        if entry.samples == 0 {
            // First observation: seed the EWMA so it isn't permanently biased toward 0.
            entry.ewma_latency_ms = lat;
        } else {
            entry.ewma_latency_ms = ALPHA * lat + (1.0 - ALPHA) * entry.ewma_latency_ms;
        }

        // Update EWMA tps when token count is available and request succeeded.
        if let Some(toks) = tokens_out {
            if !is_error && duration_ms > 0 {
                let tps = toks as f64 / (duration_ms as f64 / 1_000.0);
                if entry.samples == 0 {
                    entry.ewma_tps = tps;
                } else {
                    entry.ewma_tps = ALPHA * tps + (1.0 - ALPHA) * entry.ewma_tps;
                }
            }
        }

        // Shift the error ring and set the newest bit.
        entry.err_window = (entry.err_window << 1) | (is_error as u32);

        entry.samples = entry.samples.saturating_add(1);
        entry.last_update_tick = tick;
    }

    /// Increment the health-transition counter for (backend, model).
    ///
    /// Called by the health checker when a backend flips healthy↔unhealthy.
    /// This is separate from `update()` because transitions aren't tied to
    /// individual requests.
    pub async fn record_health_transition(&self, backend: &str, model: &str) {
        let mut guard = self.inner.write().await;
        let tick = guard.tick + 1;
        guard.tick = tick;

        let key = (backend.to_string(), model.to_string());
        let entry = guard.map.entry(key).or_default();
        entry.health_transitions = entry.health_transitions.saturating_add(1);
        entry.last_update_tick = tick;
    }

    /// Take a point-in-time snapshot of stats for a single (backend, model) key.
    ///
    /// Returns `None` if the key has never been updated. The caller on the scoring
    /// path calls this once per candidate and passes the result into `compute_raw`.
    /// No `.await` must occur between acquiring the snapshot and using it in the
    /// scoring pipeline.
    pub async fn snapshot(&self, backend: &str, model: &str) -> Option<BackendModelStats> {
        let guard = self.inner.read().await;
        guard
            .map
            .get(&(backend.to_string(), model.to_string()))
            .cloned()
    }

    /// Take a point-in-time snapshot of ALL entries. Used by the scored router to
    /// avoid per-candidate lock acquisition: one read-lock, then O(candidates)
    /// BTreeMap lookups against the owned snapshot map — zero extra locking on
    /// the scoring path.
    pub async fn snapshot_all(&self) -> BTreeMap<(String, String), BackendModelStats> {
        self.inner.read().await.map.clone()
    }

    /// Number of tracked (backend, model) pairs. Primarily used in tests.
    pub async fn len(&self) -> usize {
        self.inner.read().await.map.len()
    }

    /// True if no (backend, model) pairs are tracked yet.
    pub async fn is_empty(&self) -> bool {
        self.inner.read().await.map.is_empty()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── EWMA math ────────────────────────────────────────────────────────────

    /// Verify EWMA latency folding: second sample = alpha*new + (1-alpha)*seed.
    #[tokio::test]
    async fn ewma_latency_folding() {
        let rs = RoutingStats::new();
        // First update seeds the EWMA directly (no alpha blend against zero).
        rs.update("b", "m", 1000, false, None).await;
        let s = rs.snapshot("b", "m").await.unwrap();
        assert!(
            (s.ewma_latency_ms - 1000.0).abs() < 1e-9,
            "first sample seeds EWMA directly"
        );

        // Second update: alpha*new + (1-alpha)*old = 0.2*500 + 0.8*1000 = 900.
        rs.update("b", "m", 500, false, None).await;
        let s = rs.snapshot("b", "m").await.unwrap();
        let expected = ALPHA * 500.0 + (1.0 - ALPHA) * 1000.0;
        assert!(
            (s.ewma_latency_ms - expected).abs() < 1e-9,
            "second sample: got {}, expected {}",
            s.ewma_latency_ms,
            expected
        );
    }

    // ── Error ring ────────────────────────────────────────────────────────────

    /// After 4 success + 4 error updates the error ring has exactly 4 set bits;
    /// error_rate() returns 4/8 = 0.5.
    #[tokio::test]
    async fn error_ring_shift_and_rate() {
        let rs = RoutingStats::new();
        // 4 successes then 4 errors.
        for _ in 0..4 {
            rs.update("b", "m", 100, false, None).await;
        }
        for _ in 0..4 {
            rs.update("b", "m", 100, true, None).await;
        }
        let s = rs.snapshot("b", "m").await.unwrap();
        assert_eq!(s.samples, 8);
        assert_eq!(s.err_window.count_ones(), 4, "4 error bits should be set");
        let rate = s.error_rate();
        assert!((rate - 0.5).abs() < 1e-9, "error_rate = 0.5, got {}", rate);
    }

    /// The ring holds exactly ERR_RING_WIDTH bits; after more than 32 updates only
    /// the most-recent 32 contribute to error_rate().
    #[tokio::test]
    async fn error_ring_caps_at_width() {
        let rs = RoutingStats::new();
        // 40 successes — after 40 updates the ring must be all-zero.
        for _ in 0..40 {
            rs.update("b", "m", 100, false, None).await;
        }
        let s = rs.snapshot("b", "m").await.unwrap();
        assert_eq!(
            s.err_window.count_ones(),
            0,
            "ring must be all-zero after 40 successes"
        );

        // Then 32 errors — ring must be all-one (rate = 1.0).
        for _ in 0..32 {
            rs.update("b", "m", 100, true, None).await;
        }
        let s = rs.snapshot("b", "m").await.unwrap();
        assert_eq!(
            s.err_window,
            u32::MAX,
            "ring must be all-ones after 32 consecutive errors"
        );
        let rate = s.error_rate();
        assert!((rate - 1.0).abs() < 1e-9, "error_rate should be 1.0");
    }

    // ── MIN_SAMPLES gating ────────────────────────────────────────────────────

    /// A key with fewer than MIN_SAMPLES updates must read as absent (None) when
    /// filtered at the scoring site. We test this indirectly: after 4 updates
    /// `samples` is 4 < MIN_SAMPLES=5, and directly verify the gating logic that
    /// `compute_raw` applies (samples < MIN_SAMPLES → treat as absent).
    #[tokio::test]
    async fn samples_below_min_gating() {
        let rs = RoutingStats::new();
        for _ in 0..(MIN_SAMPLES - 1) {
            rs.update("b", "m", 200, false, None).await;
        }
        let s = rs.snapshot("b", "m").await.unwrap();
        assert!(
            s.samples < MIN_SAMPLES,
            "should be under MIN_SAMPLES after {} updates",
            MIN_SAMPLES - 1
        );
        // The scoring site gates on samples < MIN_SAMPLES → dims absent. Confirm
        // the count is exactly MIN_SAMPLES - 1.
        assert_eq!(s.samples, MIN_SAMPLES - 1);
    }

    // ── LRU eviction at the ceiling ───────────────────────────────────────────

    /// At MAX_ROUTING_STATS entries a new key must evict the LRU entry (lowest
    /// tick), NOT silently drop the new entry.
    #[tokio::test]
    async fn lru_eviction_at_ceiling() {
        // Use a small ceiling via direct instantiation and saturate it.
        // We cannot change MAX_ROUTING_STATS without a const override, so we insert
        // MAX_ROUTING_STATS entries then insert one more and check that (a) len
        // stays at MAX_ROUTING_STATS and (b) the newest key IS present (not dropped)
        // and (c) the first-inserted (oldest tick) key is gone.
        //
        // To keep the test fast we use a separate RoutingStats with a tiny inner
        // map, replicating the eviction logic inline. The unit under test is the
        // `update()` implementation: after filling to MAX_ROUTING_STATS the next
        // new key causes an eviction.
        //
        // Strategy: insert MAX_ROUTING_STATS keys, then insert key "new".
        // Because keys are inserted sequentially, key ("b0", "m") has the
        // smallest last_update_tick and must be evicted.
        let rs = RoutingStats::new();
        for i in 0..MAX_ROUTING_STATS {
            rs.update(&format!("b{}", i), "m", 100, false, None).await;
        }
        assert_eq!(rs.len().await, MAX_ROUTING_STATS);

        // Insert a brand-new key — should trigger LRU eviction of ("b0","m").
        rs.update("brand-new", "m", 100, false, None).await;

        assert_eq!(
            rs.len().await,
            MAX_ROUTING_STATS,
            "len must stay at ceiling after eviction"
        );
        // The new key must be present.
        assert!(
            rs.snapshot("brand-new", "m").await.is_some(),
            "new key must be present after eviction"
        );
        // The first-inserted (LRU) key must be gone.
        assert!(
            rs.snapshot("b0", "m").await.is_none(),
            "LRU key (b0) must have been evicted"
        );
    }

    // ── snapshot_all / snapshot ───────────────────────────────────────────────

    /// snapshot_all returns an owned clone; mutations after the snapshot do not
    /// retroactively change the already-taken snapshot.
    #[tokio::test]
    async fn snapshot_all_is_isolated() {
        let rs = RoutingStats::new();
        rs.update("b", "m", 1000, false, None).await;

        let snap = rs.snapshot_all().await;
        // Mutate the store after taking the snapshot.
        rs.update("b", "m", 100, false, None).await;

        // The snapshot should still hold the original first-sample value.
        let s = snap.get(&("b".to_string(), "m".to_string())).unwrap();
        assert_eq!(
            s.samples, 1,
            "snapshot must not reflect post-snapshot writes"
        );
    }
}
