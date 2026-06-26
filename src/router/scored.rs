/// Scored router.
///
/// GATE → SCORE → SELECT pipeline over 23 dimensions. Active: dims 1–9 (Phase 1)
/// plus dims 10–13 (Phase 2 — live load & latency from agent telemetry; dim 11
/// `ttft_p50` reads agent caps but the agent does not yet measure it). Dims 14–17
/// (Phase 3) read per-(backend,model) EWMA history from `RoutingStats`.
/// Phase 4 (policy/affinity) is landing incrementally: dims 18 (`session_stickiness`,
/// from the `SessionAffinity` store), 19 (`network_locality`), 20 (`power_cost`,
/// config-only) and 23 (`warm_model_recency`, from per-(backend,model) last-served
/// time on `BackendState`) are active; dims 21, 22 remain absent until their slices.
use crate::backend::{pool::filter_healthy, BackendPool};
use crate::config::{BackendType, ModelGate, ScoredConfig, ScoredWeights};
use crate::router::routing_stats::{BackendModelStats, RoutingStats, MIN_SAMPLES};
use crate::router::session_affinity::SessionAffinity;
use crate::router::{RouteContext, RoutedBackend, Router};
use anyhow::Result;
use async_trait::async_trait;
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Instant;
use tracing::debug;

// ── Normalization constants ──────────────────────────────────────────────────

const TEMP_MIN: f64 = 40.0;
const TEMP_MAX: f64 = 85.0;
/// Soft score for a backend whose type doesn't match the preference (dim 9).
const BACKEND_NEUTRAL: f64 = 0.6;
/// Rough VRAM estimate: ~1 GB (1024 MB) per billion params (q8-ish; fp16 ≈ 2×).
/// Coarse fit heuristic for dim 2 (`model_fits_vram`) only.
const BYTES_PER_BILLION_MB: u64 = 1024;
/// Reference queue depth for dim 10 (`queue_depth`); a backend with this many
/// queued requests normalizes to 0.0, an empty queue to 1.0.
const QUEUE_REF: f64 = 8.0;
/// Reference TTFT (ms) for dim 11 (`ttft_p50`); at/above this a backend
/// normalizes to 0.0, instant first token to 1.0.
const TTFT_REF: f64 = 2000.0;
/// Reference cost/power weight for dim 20 (`power_cost`); a backend at/above
/// this normalizes to 0.0 (most expensive), a zero-cost backend to 1.0. Same
/// arbitrary unit the operator uses for `Backend.power_cost` (watts, $/1k tok…).
const COST_REF: f64 = 100.0;
/// Reference age (seconds) for dim 23 (`warm_model_recency`); a model served
/// just now normalizes to 1.0, one served `WARM_REF`+ seconds ago to 0.0.
/// ~5 min ≈ Ollama's default keep-alive window before a model is evicted.
const WARM_REF: f64 = 300.0;
/// Scale factor for quantizing scores to i64 comparison keys.
const SCORE_SCALE: f64 = 1_000_000.0;

// ── Phase-3 normalization re-exports (mirrors routing_stats constants) ───────
// Using the canonical values from routing_stats rather than duplicating them.
use crate::router::routing_stats::{FLAP_REF, LAT_REF, TPS_REF};

// ── Dimension catalog ────────────────────────────────────────────────────────

/// Fixed-order catalog of all 23 dimensions (catalog order = array index).
/// `#[repr(u8)]` so `as usize` gives the index directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Dimension {
    ModelResident = 0,
    ModelFitsVram = 1,
    PromptSizeVsCapacity = 2,
    GpuUtilization = 3,
    VramHeadroom = 4,
    GpuTemperature = 5,
    OperatorPriority = 6,
    TagAffinity = 7,
    BackendTypeAffinity = 8,
    // Phase 2 — always absent in Phase 1
    QueueDepth = 9,
    TtftP50 = 10,
    ConcurrencySaturation = 11,
    PreciseVramFree = 12,
    // Phase 3 — always absent in Phase 1
    EwmaLatency = 13,
    RecentErrorRate = 14,
    RecentSuccessThroughput = 15,
    FlapStability = 16,
    // Phase 4 — dims 18/19/20/23 active; 21, 22 absent until their slices
    SessionStickiness = 17,
    NetworkLocality = 18,
    PowerCost = 19,
    RpcShardCapability = 20,
    GpuClassAffinity = 21,
    WarmModelRecency = 22,
}

const DIM_COUNT: usize = 23;

// Ordered list for iteration in canonical catalog order.
const ALL_DIMS: [Dimension; DIM_COUNT] = [
    Dimension::ModelResident,
    Dimension::ModelFitsVram,
    Dimension::PromptSizeVsCapacity,
    Dimension::GpuUtilization,
    Dimension::VramHeadroom,
    Dimension::GpuTemperature,
    Dimension::OperatorPriority,
    Dimension::TagAffinity,
    Dimension::BackendTypeAffinity,
    Dimension::QueueDepth,
    Dimension::TtftP50,
    Dimension::ConcurrencySaturation,
    Dimension::PreciseVramFree,
    Dimension::EwmaLatency,
    Dimension::RecentErrorRate,
    Dimension::RecentSuccessThroughput,
    Dimension::FlapStability,
    Dimension::SessionStickiness,
    Dimension::NetworkLocality,
    Dimension::PowerCost,
    Dimension::RpcShardCapability,
    Dimension::GpuClassAffinity,
    Dimension::WarmModelRecency,
];

impl Dimension {
    #[inline]
    fn idx(self) -> usize {
        self as usize
    }

    fn name(self) -> &'static str {
        match self {
            Dimension::ModelResident => "model_resident",
            Dimension::ModelFitsVram => "model_fits_vram",
            Dimension::PromptSizeVsCapacity => "prompt_size_vs_capacity",
            Dimension::GpuUtilization => "gpu_utilization",
            Dimension::VramHeadroom => "vram_headroom",
            Dimension::GpuTemperature => "gpu_temperature",
            Dimension::OperatorPriority => "operator_priority",
            Dimension::TagAffinity => "tag_affinity",
            Dimension::BackendTypeAffinity => "backend_type_affinity",
            Dimension::QueueDepth => "queue_depth",
            Dimension::TtftP50 => "ttft_p50",
            Dimension::ConcurrencySaturation => "concurrency_saturation",
            Dimension::PreciseVramFree => "precise_vram_free",
            Dimension::EwmaLatency => "ewma_latency",
            Dimension::RecentErrorRate => "recent_error_rate",
            Dimension::RecentSuccessThroughput => "recent_success_throughput",
            Dimension::FlapStability => "flap_stability",
            Dimension::SessionStickiness => "session_stickiness",
            Dimension::NetworkLocality => "network_locality",
            Dimension::PowerCost => "power_cost",
            Dimension::RpcShardCapability => "rpc_shard_capability",
            Dimension::GpuClassAffinity => "gpu_class_affinity",
            Dimension::WarmModelRecency => "warm_model_recency",
        }
    }
}

// ── Router struct ────────────────────────────────────────────────────────────

/// Scored router: GATE → SCORE (dims 1–17 active where data available) → SELECT.
#[derive(Clone)]
pub struct ScoredRouter {
    pool: BackendPool,
    /// Fixed weight array in catalog order (index = Dimension as usize).
    weights: [f64; DIM_COUNT],
    model_gate: ModelGate,
    prefer_backend_type: Option<BackendType>,
    /// Shared Phase-3 EWMA history store. One read-lock snapshot is taken per
    /// routing call; no lock is held across the score/select steps.
    routing_stats: Arc<RoutingStats>,
    /// Shared Phase-4 session→backend affinity store (dim 18). Read once per
    /// routing call, only when the request carries a session id.
    session_affinity: Arc<SessionAffinity>,
}

impl ScoredRouter {
    pub fn new(
        pool: BackendPool,
        config: &ScoredConfig,
        routing_stats: Arc<RoutingStats>,
        session_affinity: Arc<SessionAffinity>,
    ) -> Self {
        let w = &config.weights;
        let weights = weights_array(w);
        Self {
            pool,
            weights,
            model_gate: config.model_gate,
            prefer_backend_type: config.prefer_backend_type,
            routing_stats,
            session_affinity,
        }
    }
}

/// Build the fixed-size weights array from a `ScoredWeights` struct.
fn weights_array(w: &ScoredWeights) -> [f64; DIM_COUNT] {
    [
        w.model_resident,            // 0
        w.model_fits_vram,           // 1
        w.prompt_size_vs_capacity,   // 2
        w.gpu_utilization,           // 3
        w.vram_headroom,             // 4
        w.gpu_temperature,           // 5
        w.operator_priority,         // 6
        w.tag_affinity,              // 7
        w.backend_type_affinity,     // 8
        w.queue_depth,               // 9
        w.ttft_p50,                  // 10
        w.concurrency_saturation,    // 11
        w.precise_vram_free,         // 12
        w.ewma_latency,              // 13
        w.recent_error_rate,         // 14
        w.recent_success_throughput, // 15
        w.flap_stability,            // 16
        w.session_stickiness,        // 17
        w.network_locality,          // 18
        w.power_cost,                // 19
        w.rpc_shard_capability,      // 20
        w.gpu_class_affinity,        // 21
        w.warm_model_recency,        // 22
    ]
}

/// Quantize a `[0,1]` score to a fixed-scale integer comparison key.
/// Same quantizer the pre-pass uniformity test uses — no new epsilon.
#[inline]
fn quantize(score: f64) -> i64 {
    (score * SCORE_SCALE).round() as i64
}

// ── Per-candidate score computation ─────────────────────────────────────────

/// Per-candidate raw norm values and presence flags (before pre-pass).
struct CandidateRaw {
    /// Normalised value for each dimension. Only meaningful when `present0[i]` is true.
    norms: [f64; DIM_COUNT],
    /// Whether the dimension's source datum is present for this candidate.
    present0: [bool; DIM_COUNT],
}

/// Compute raw norms for all 23 dimensions for a single candidate.
///
/// Dims 10, 11, 13 (Phase 2 Slice 1) read agent live-load telemetry when present.
/// Dims 14–17 (Phase 3) read from the EWMA history snapshot when `stats` is
/// `Some` AND `stats.samples >= MIN_SAMPLES`.
/// Dims 12, 18–23: their sources are `None`/absent, so `present0` is `false` →
/// neutral/weight-dropped.
// Each parameter is a distinct, independent scoring input (candidate, request,
// fleet-relative bounds, history snapshot); grouping them into a struct would
// obscure more than it clarifies for this private pure fn.
#[allow(clippy::too_many_arguments)]
fn compute_raw(
    b: &crate::backend::pool::BackendState,
    model: Option<&str>,
    tags: Option<&[String]>,
    relaxed: bool,
    prefer_backend_type: Option<BackendType>,
    pmin: u32,
    pmax: u32,
    prompt_tokens: Option<u32>,
    stats: Option<&BackendModelStats>,
    now: Instant,
    sticky_backend: Option<&str>,
) -> CandidateRaw {
    let mut norms = [0.0f64; DIM_COUNT];
    let mut present0 = [false; DIM_COUNT];

    // ── Dim 1: model_resident ────────────────────────────────────────────────
    // NOTE: model_resident self-neutralizes via the Q6 pre-pass and does NOT
    // contribute to ranking. The GATE (step 2) already filters to residents-only
    // unless relaxed, so within the scored set this dim is uniform — all 1.0
    // (resident set) or all 0.0 (relaxed, none resident) — and Q6 drops it as
    // call-uniform. Its weight (default 5.0) is effectively the GATE, not a score
    // term: residency is enforced by elimination, not scored. A resident-vs-
    // cold-loadable distinction that would actually rank needs Phase 2.
    // present if model was requested AND (resident OR gate has relaxed).
    if let Some(m) = model {
        let resident = b.models.iter().any(|x| x == m);
        if resident || relaxed {
            present0[Dimension::ModelResident.idx()] = true;
            norms[Dimension::ModelResident.idx()] = if resident { 1.0 } else { 0.0 };
        }
    }

    // ── Dim 2: model_fits_vram ───────────────────────────────────────────────
    // est_mb = extract_param_billions(model).filter(>0) * 1024
    // free_mb = vram_free_mb if Some, else gpu_metrics.(total - used)
    if let Some(m) = model {
        if let Some(est_b) = crate::analytics::extract_param_billions(m).filter(|&b| b > 0) {
            let est_mb = est_b * BYTES_PER_BILLION_MB;
            let free_mb: Option<u64> = b.vram_free_mb.or_else(|| {
                b.gpu_metrics
                    .as_ref()
                    .map(|gm| gm.memory_total.saturating_sub(gm.memory_used))
            });
            if let Some(free) = free_mb {
                present0[Dimension::ModelFitsVram.idx()] = true;
                norms[Dimension::ModelFitsVram.idx()] =
                    (free as f64 / est_mb as f64).clamp(0.0, 1.0);
            }
        }
    }

    // ── Dim 3: prompt_size_vs_capacity ───────────────────────────────────────
    // present iff the proxy estimated prompt_tokens AND this backend declares a
    // ctx window (`config.max_context_len`, > 0). n = (1 - prompt/window) / 0.5,
    // clamped: 1.0 when the prompt is ≤ 50% of the window, linear down to 0.0 at
    // 100%, and 0.0 beyond. A backend without a configured window (most agent
    // nodes today) is simply absent here — neutral, never penalized.
    if let (Some(pt), Some(ctx)) = (prompt_tokens, b.config.max_context_len) {
        if ctx > 0 {
            present0[Dimension::PromptSizeVsCapacity.idx()] = true;
            let ratio = pt as f64 / ctx as f64;
            norms[Dimension::PromptSizeVsCapacity.idx()] = ((1.0 - ratio) / 0.5).clamp(0.0, 1.0);
        }
    }

    // ── Dim 4: gpu_utilization ───────────────────────────────────────────────
    if let Some(ref gm) = b.gpu_metrics {
        present0[Dimension::GpuUtilization.idx()] = true;
        norms[Dimension::GpuUtilization.idx()] =
            (1.0 - gm.utilization as f64 / 100.0).clamp(0.0, 1.0);
    }

    // ── Dim 5: vram_headroom ─────────────────────────────────────────────────
    // Phase 1: use gpu_metrics (memory_total - memory_used) / memory_total.
    // Phase 2 will flip to vram_free_mb / vram_total_mb when both are Some.
    if let Some(ref gm) = b.gpu_metrics {
        if gm.memory_total > 0 {
            present0[Dimension::VramHeadroom.idx()] = true;
            let free = gm.memory_total.saturating_sub(gm.memory_used);
            norms[Dimension::VramHeadroom.idx()] =
                (free as f64 / gm.memory_total as f64).clamp(0.0, 1.0);
        }
    }

    // ── Dim 6: gpu_temperature ───────────────────────────────────────────────
    // A GPU under load is never ≤ 0 °C, so a non-positive reading is the
    // "sensor unavailable" sentinel (the agent reports the field but the driver
    // had no value). Treat it as source-absent — neutral and weight-dropped —
    // rather than scoring a perfect coolness norm (1.0) that would unfairly win
    // dim 6. NOTE: utilization == 0 (dim 4) is NOT a sentinel — an idle GPU is
    // genuinely the best routing target — so dim 4 keeps the zero.
    if let Some(ref gm) = b.gpu_metrics {
        if gm.temperature > 0.0 {
            present0[Dimension::GpuTemperature.idx()] = true;
            // Denominator is TEMP_MAX - TEMP_MIN = 45.0, a compile-time constant ≠ 0.
            norms[Dimension::GpuTemperature.idx()] =
                ((TEMP_MAX - gm.temperature as f64) / (TEMP_MAX - TEMP_MIN)).clamp(0.0, 1.0);
        }
    }

    // ── Dim 7: operator_priority ─────────────────────────────────────────────
    // Always present (priority is non-Option). Fleet-relative; pmin/pmax
    // already computed over the whole candidate set.
    present0[Dimension::OperatorPriority.idx()] = true;
    norms[Dimension::OperatorPriority.idx()] = if pmax == pmin {
        // all-equal → 0.5 (will be dropped by pre-pass as call-uniform)
        0.5
    } else {
        let p = b.config.priority;
        ((p - pmin) as f64 / (pmax - pmin) as f64).clamp(0.0, 1.0)
    };

    // ── Dim 8: tag_affinity ──────────────────────────────────────────────────
    // present if the request supplied a non-empty tag set.
    if let Some(req_tags) = tags {
        if !req_tags.is_empty() {
            present0[Dimension::TagAffinity.idx()] = true;
            let matched = req_tags
                .iter()
                .filter(|t| b.config.tags.contains(t))
                .count();
            norms[Dimension::TagAffinity.idx()] = matched as f64 / req_tags.len() as f64;
        }
    }

    // ── Dim 9: backend_type_affinity ─────────────────────────────────────────
    if let Some(pref) = prefer_backend_type {
        present0[Dimension::BackendTypeAffinity.idx()] = true;
        norms[Dimension::BackendTypeAffinity.idx()] = if b.config.backend == pref {
            1.0
        } else {
            BACKEND_NEUTRAL
        };
    }

    // ── Dim 10: queue_depth (Phase 2 Slice 1) ───────────────────────────────
    // Presence predicate is `is_some()`, NOT `unwrap_or(0) > 0`: Some(0) is a
    // REAL signal (an empty queue, the best possible target → 1.0), not absent.
    // Only None (agent never reported it) → not-present → neutral, weight-dropped.
    if let Some(qd) = b.queue_depth {
        present0[Dimension::QueueDepth.idx()] = true;
        norms[Dimension::QueueDepth.idx()] = (1.0 - qd as f64 / QUEUE_REF).clamp(0.0, 1.0);
    }

    // ── Dim 11: ttft_p50 (Phase 2 Slice 1) ───────────────────────────────────
    if let Some(ttft) = b.ttft_p50_ms {
        present0[Dimension::TtftP50.idx()] = true;
        norms[Dimension::TtftP50.idx()] = (1.0 - ttft as f64 / TTFT_REF).clamp(0.0, 1.0);
    }

    // ── Dim 12: concurrency_saturation (Phase 2 Slice 2) ─────────────────────
    // How full the backend's concurrency slots are: queue_depth / max_concurrent.
    // Present only when BOTH are known and max_concurrent > 0 (the `> 0` is the
    // div-by-zero guard; a degenerate Some(0) capacity is treated as unmeasured).
    // Coexists with dim 10 (no supersession): dim 10 is absolute queue pressure,
    // dim 12 is capacity-relative; per-backend renorm tolerates the correlation.
    if let (Some(depth), Some(max)) = (b.queue_depth, b.max_concurrent) {
        if max > 0 {
            present0[Dimension::ConcurrencySaturation.idx()] = true;
            norms[Dimension::ConcurrencySaturation.idx()] =
                (1.0 - depth as f64 / max as f64).clamp(0.0, 1.0);
        }
    }

    // ── Dim 13: precise_vram_free (Phase 2 Slice 1) ──────────────────────────
    // Measured free VRAM from agent telemetry. When present it SUPERSEDES dim 5
    // (vram_headroom, gpu_metrics-derived) for this candidate to avoid double-
    // counting the same VRAM-pressure signal — clearing dim 5's present flag
    // drops its weight from this backend's score denominator. Supersession is
    // per-candidate: a static backend with no agent telemetry keeps its dim-5.
    // Some(0) free VRAM is a REAL signal (worst, → 0.0), not absent.
    if let Some(free) = b.vram_free_mb {
        if let Some(total) = b.vram_total_mb {
            if total > 0 {
                present0[Dimension::PreciseVramFree.idx()] = true;
                norms[Dimension::PreciseVramFree.idx()] =
                    (free as f64 / total as f64).clamp(0.0, 1.0);
                // Supersede dim 5 — the precise signal wins.
                present0[Dimension::VramHeadroom.idx()] = false;
            }
        }
    }

    // ── Dims 14–17: Phase 3 — per-(backend,model) EWMA history ──────────────
    // Present only when stats is Some AND samples >= MIN_SAMPLES (cold-start
    // gating). Under-sampled or missing history → not present → neutral 0.5,
    // weight-dropped — never a penalty for new/restarted backends.
    if let Some(s) = stats.filter(|s| s.samples >= MIN_SAMPLES) {
        // Dim 14: ewma_latency — lower latency is better.
        // n = clamp(1 - ewma_ms / LAT_REF, 0, 1)
        present0[Dimension::EwmaLatency.idx()] = true;
        norms[Dimension::EwmaLatency.idx()] = (1.0 - s.ewma_latency_ms / LAT_REF).clamp(0.0, 1.0);

        // Dim 15: recent_error_rate — lower error rate is better.
        // n = 1 - error_rate  (error_rate ∈ [0,1])
        present0[Dimension::RecentErrorRate.idx()] = true;
        norms[Dimension::RecentErrorRate.idx()] = 1.0 - s.error_rate();

        // Dim 16: recent_success_throughput — higher tps is better.
        // n = clamp(ewma_tps / TPS_REF, 0, 1)
        present0[Dimension::RecentSuccessThroughput.idx()] = true;
        norms[Dimension::RecentSuccessThroughput.idx()] = (s.ewma_tps / TPS_REF).clamp(0.0, 1.0);

        // Dim 17: flap_stability — fewer transitions is better.
        // n = 1 - clamp(health_transitions / FLAP_REF, 0, 1)
        present0[Dimension::FlapStability.idx()] = true;
        norms[Dimension::FlapStability.idx()] =
            1.0 - (s.health_transitions as f64 / FLAP_REF).clamp(0.0, 1.0);
    }

    // ── Dim 19: network_locality — Phase 4 (config-only) ─────────────────────
    // Closer backends are preferred (less proxy-hop latency). Present iff the
    // operator configured a tier; absent → neutral 0.5, weight-dropped.
    if let Some(tier) = b.config.locality {
        present0[Dimension::NetworkLocality.idx()] = true;
        norms[Dimension::NetworkLocality.idx()] = match tier {
            crate::config::LocalityTier::Local => 1.0,
            crate::config::LocalityTier::Lan => 0.8,
            crate::config::LocalityTier::Tailnet => 0.6,
            crate::config::LocalityTier::Wan => 0.4,
        };
    }

    // ── Dim 20: power_cost — Phase 4 (config-only) ───────────────────────────
    // Cheaper/lower-power backends preferred. n = clamp(1 - cost/COST_REF, 0, 1):
    // a zero-cost backend → 1.0, one at/above COST_REF → 0.0. Present iff the
    // operator configured a cost AND COST_REF > 0 (guard div-by-zero).
    if let Some(cost) = b.config.power_cost {
        if COST_REF > 0.0 {
            present0[Dimension::PowerCost.idx()] = true;
            norms[Dimension::PowerCost.idx()] = (1.0 - cost / COST_REF).clamp(0.0, 1.0);
        }
    }

    // ── Dim 23: warm_model_recency — Phase 4 (stateful) ──────────────────────
    // How recently THIS backend last served the requested model (prompt/KV-cache
    // warmth). Present iff a model is requested. Served just now → 1.0, ≥WARM_REF
    // seconds ago → 0.0; a model never served here reads neutral 0.5 (unknown, not
    // a penalty). When no model is requested the dimension is absent. `now` is
    // captured once per routing call so every candidate is aged against the same
    // instant (fair + deterministic within the call).
    if let Some(m) = model {
        let dim = Dimension::WarmModelRecency.idx();
        present0[dim] = true;
        norms[dim] = match b.last_served.get(m) {
            Some(&t) => {
                let age = now.saturating_duration_since(t).as_secs_f64();
                (1.0 - age / WARM_REF).clamp(0.0, 1.0)
            }
            None => 0.5,
        };
    }

    // ── Dim 18: session_stickiness — Phase 4 (affinity) ──────────────────────
    // Prefer the backend that served this session's last turn (prompt/KV-cache
    // warmth). Present iff a prior backend is known for the session (the request
    // carried a session id AND it has history): the matching candidate scores
    // 1.0, all others 0.5. A new/unknown session → `sticky_backend` is None → dim
    // absent (a brand-new session would make every candidate uniform-0.5 anyway).
    if let Some(last) = sticky_backend {
        let dim = Dimension::SessionStickiness.idx();
        present0[dim] = true;
        norms[dim] = if last == b.config.name { 1.0 } else { 0.5 };
    }

    // ── Dims 21, 22: Phase 4 — absent until their slices ─────────────────────
    // rpc_shard_capability (21), gpu_class_affinity (22): sources None/absent →
    // present0 stays false.

    CandidateRaw { norms, present0 }
}

// ── GATE → SCORE → SELECT ────────────────────────────────────────────────────

#[async_trait]
impl Router for ScoredRouter {
    /// Delegates to `route_scored` with a default (empty) context.
    /// Legacy callers using `route_excluding` get correct scored routing;
    /// dims 2–3 stay dormant-neutral (no prompt_tokens supplied).
    async fn route_excluding(
        &self,
        model: Option<&str>,
        tags: Option<&[String]>,
        excluded: &HashSet<String>,
    ) -> Result<RoutedBackend> {
        self.route_scored(model, tags, excluded, &RouteContext::default())
            .await
    }

    /// GATE → SCORE → SELECT.  Terminal — never calls back into `route_excluding`.
    async fn route_scored(
        &self,
        model: Option<&str>,
        tags: Option<&[String]>,
        excluded: &HashSet<String>,
        ctx: &RouteContext,
    ) -> Result<RoutedBackend> {
        // ── Single snapshot — ONE read, no .await between here and the return. ──
        let backends_guard = self.pool.backends.read().await;
        let backends: &[crate::backend::pool::BackendState] = &backends_guard;

        let tag_slice: &[String] = tags.unwrap_or(&[]);

        // ── GATE ──────────────────────────────────────────────────────────────
        // Step 1: healthy ∧ ¬excluded ∧ tags⊆
        let healthy: Vec<&crate::backend::pool::BackendState> =
            filter_healthy(backends, excluded, tag_slice);

        if healthy.is_empty() {
            return Err(anyhow::anyhow!("No healthy backends available"));
        }

        // Step 2: model-residency filter.
        let (candidates, relaxed): (Vec<&crate::backend::pool::BackendState>, bool) =
            if let Some(m) = model {
                let residents: Vec<_> = healthy
                    .iter()
                    .copied()
                    .filter(|b| b.models.iter().any(|x| x == m))
                    .collect();

                if !residents.is_empty() {
                    (residents, false)
                } else {
                    // Relaxed mode: fall back to model-unaware healthy set.
                    // Strict mode: no residents → Err.
                    match self.model_gate {
                        ModelGate::Relaxed => (healthy, true),
                        ModelGate::Strict => {
                            return Err(anyhow::anyhow!("No healthy backends available"));
                        }
                    }
                }
            } else {
                // No model requested — use the full healthy set.
                (healthy, false)
            };

        // candidates is never empty here (healthy was non-empty; residents
        // branch returned early if empty+strict; relaxed keeps healthy).
        let n_total = backends.len();
        let n_gated = n_total - candidates.len();

        // ── SCORE: compute raw norms (present₀) ──────────────────────────────

        // Phase-3: take ONE snapshot of the EWMA history store before scoring.
        // This is the only lock acquisition on the Phase-3 path; no lock is held
        // across the score/select loop. The snapshot is an owned BTreeMap so the
        // read guard is released immediately after the clone.
        // No .await occurs between here and the returned RoutedBackend.
        let stats_snap: BTreeMap<(String, String), BackendModelStats> =
            self.routing_stats.snapshot_all().await;

        // Dim 7 (operator_priority) is fleet-relative: need pmin/pmax first.
        let (pmin, pmax) = candidates.iter().fold((u32::MAX, u32::MIN), |(lo, hi), b| {
            (lo.min(b.config.priority), hi.max(b.config.priority))
        });

        // Dim 23 (warm_model_recency) ages every candidate against ONE instant,
        // captured here so the comparison is fair and deterministic within the call.
        let now = Instant::now();

        // Dim 18 (session_stickiness): resolve the session's last backend ONCE,
        // only when the request carries a session id. Same value for every
        // candidate; the matching one scores 1.0. None → dim absent.
        let sticky_backend: Option<String> = match ctx.session_id.as_deref() {
            Some(sid) => self.session_affinity.get(sid).await,
            None => None,
        };
        let sticky_backend = sticky_backend.as_deref();

        // Per-candidate raw norms + present₀ flags.
        let raws: Vec<CandidateRaw> = candidates
            .iter()
            .map(|b| {
                // Look up this candidate's Phase-3 stats from the snapshot.
                // model_key is the originally requested model string — the same key
                // used when recording outcomes on the post-request hook.
                let model_key = model.unwrap_or("");
                let candidate_stats = stats_snap
                    .get(&(b.config.name.clone(), model_key.to_string()))
                    .filter(|s| s.samples >= MIN_SAMPLES);
                compute_raw(
                    b,
                    model,
                    tags,
                    relaxed,
                    self.prefer_backend_type,
                    pmin,
                    pmax,
                    ctx.prompt_tokens,
                    candidate_stats,
                    now,
                    sticky_backend,
                )
            })
            .collect();

        // ── Q6 call-uniform pre-pass ──────────────────────────────────────────
        // For each dimension i, if Pᵢ (candidates where present₀) is non-empty
        // AND every norm in Pᵢ maps to the same quantized value → drop dim i
        // from ALL candidates' present sets for this call.
        let mut uniform = [false; DIM_COUNT];
        for dim in ALL_DIMS {
            let i = dim.idx();
            // Collect quantized values for present₀ candidates.
            let mut q_vals: Option<i64> = None;
            let mut any_present = false;
            let mut all_same = true;
            for raw in &raws {
                if raw.present0[i] {
                    let qv = quantize(raw.norms[i]);
                    any_present = true;
                    match q_vals {
                        None => q_vals = Some(qv),
                        Some(prev) => {
                            if prev != qv {
                                all_same = false;
                                break;
                            }
                        }
                    }
                }
            }
            if any_present && all_same {
                uniform[i] = true;
            }
        }

        // ── Per-backend final score ───────────────────────────────────────────
        // present(b,i) = present₀(b,i) AND NOT uniform[i]
        // score(b) = Σ(w·n for surviving present dims) / Σ(w for those dims)
        //            or 0.5 if denom == 0.
        struct Scored<'a> {
            backend: &'a crate::backend::pool::BackendState,
            score_q: i64,
        }

        let scored: Vec<Scored> = candidates
            .iter()
            .zip(raws.iter())
            .map(|(b, raw)| {
                let mut numerator = 0.0_f64;
                let mut denom = 0.0_f64;
                for dim in ALL_DIMS {
                    let i = dim.idx();
                    let w = self.weights[i];
                    if w > 0.0 && raw.present0[i] && !uniform[i] {
                        numerator += w * raw.norms[i];
                        denom += w;
                    }
                }
                let score = if denom > 0.0 { numerator / denom } else { 0.5 };
                Scored {
                    backend: b,
                    score_q: quantize(score),
                }
            })
            .collect();

        // ── SELECT — total tie-break (score_q↓, priority↓, name↑) ────────────
        let winner = scored
            .iter()
            .max_by(|a, b| {
                // score_q descending
                let cmp = a.score_q.cmp(&b.score_q);
                if cmp != std::cmp::Ordering::Equal {
                    return cmp;
                }
                // priority descending
                let cmp2 = a.backend.config.priority.cmp(&b.backend.config.priority);
                if cmp2 != std::cmp::Ordering::Equal {
                    return cmp2;
                }
                // name ascending (guaranteed unique by validate())
                b.backend.config.name.cmp(&a.backend.config.name)
            })
            .ok_or_else(|| anyhow::anyhow!("No healthy backends available"))?;

        // ── Debug audit log ───────────────────────────────────────────────────
        debug!(
            "scored route → {}  score_q={}  (candidates={}, gated_out={}, gate={:?}, relaxed={})",
            winner.backend.config.name,
            winner.score_q,
            candidates.len(),
            n_gated,
            self.model_gate,
            relaxed,
        );

        if tracing::enabled!(tracing::Level::DEBUG) {
            // Per-candidate breakdown (only emitted when debug is enabled).
            for (s, raw) in scored.iter().zip(raws.iter()) {
                let mut surviving_parts = Vec::new();
                let mut dropped_parts = Vec::new();
                let mut absent_parts = Vec::new();
                let mut denom_val = 0.0_f64;

                for dim in ALL_DIMS {
                    let i = dim.idx();
                    let w = self.weights[i];
                    if w <= 0.0 {
                        continue; // weight 0 → skip entirely from breakdown
                    }
                    if uniform[i] {
                        dropped_parts.push(dim.name());
                    } else if raw.present0[i] {
                        surviving_parts.push(format!(
                            "{}={:.4}·{:.1}",
                            dim.name(),
                            raw.norms[i],
                            w
                        ));
                        denom_val += w;
                    } else {
                        absent_parts.push(dim.name());
                    }
                }

                debug!(
                    "  scored: {}  score_q={}  denom={:.1}  [{}]  [dropped(call-uniform): {}]  [absent: {}]",
                    s.backend.config.name,
                    s.score_q,
                    denom_val,
                    surviving_parts.join("  "),
                    dropped_parts.join(", "),
                    absent_parts.join(", "),
                );
            }
        }

        Ok(RoutedBackend {
            name: winner.backend.config.name.clone(),
            url: winner.backend.config.url.clone(),
        })
    }
}

// ── T6: Config sanitize helpers ──────────────────────────────────────────────

/// Sanitize a `ScoredWeights` struct in-place:
/// - Negative or non-finite weight → warn + reset to default.
/// - All Phase-1-active dims (1–9) zero → warn + restore all defaults.
pub fn sanitize_weights(weights: &mut ScoredWeights) {
    use crate::config::ScoredWeights as SW;

    macro_rules! fix_weight {
        ($field:ident, $default_fn:expr) => {
            if weights.$field < 0.0 || !weights.$field.is_finite() {
                tracing::warn!(
                    "scored weight '{}' = {} is negative or non-finite — using default {}",
                    stringify!($field),
                    weights.$field,
                    $default_fn,
                );
                weights.$field = $default_fn;
            }
        };
    }

    fix_weight!(model_resident, 5.0);
    fix_weight!(model_fits_vram, 2.0);
    fix_weight!(prompt_size_vs_capacity, 1.0);
    fix_weight!(gpu_utilization, 3.0);
    fix_weight!(vram_headroom, 2.0);
    fix_weight!(gpu_temperature, 1.0);
    fix_weight!(operator_priority, 2.0);
    fix_weight!(tag_affinity, 1.0);
    fix_weight!(backend_type_affinity, 0.0);
    fix_weight!(queue_depth, 2.0);
    fix_weight!(ttft_p50, 3.0);
    fix_weight!(concurrency_saturation, 1.0);
    fix_weight!(precise_vram_free, 2.0);
    fix_weight!(ewma_latency, 0.0);
    fix_weight!(recent_error_rate, 0.0);
    fix_weight!(recent_success_throughput, 0.0);
    fix_weight!(flap_stability, 0.0);
    fix_weight!(session_stickiness, 0.0);
    fix_weight!(network_locality, 0.0);
    fix_weight!(power_cost, 0.0);
    fix_weight!(rpc_shard_capability, 0.0);
    fix_weight!(gpu_class_affinity, 0.0);
    fix_weight!(warm_model_recency, 0.0);

    // Check if all Phase-1-active dims (1–9) are zero → restore defaults.
    let phase1_active_all_zero = weights.model_resident == 0.0
        && weights.model_fits_vram == 0.0
        && weights.prompt_size_vs_capacity == 0.0
        && weights.gpu_utilization == 0.0
        && weights.vram_headroom == 0.0
        && weights.gpu_temperature == 0.0
        && weights.operator_priority == 0.0
        && weights.tag_affinity == 0.0
        && weights.backend_type_affinity == 0.0;

    if phase1_active_all_zero {
        tracing::warn!("all Phase-1 scored weights are zero — falling back to default weight set");
        *weights = SW::default();
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{pool::BackendState, BackendPool, GpuMetrics};
    use crate::config::{Backend, ModelGate, ScoredConfig, ScoredWeights};
    use crate::router::routing_stats::RoutingStats;
    use std::collections::HashSet;
    use std::time::{Duration, Instant};

    // ── Pool builder helpers ─────────────────────────────────────────────────

    fn make_backend(name: &str, priority: u32) -> Backend {
        Backend {
            name: name.to_string(),
            url: format!("http://{}.local", name),
            priority,
            ..Backend::default()
        }
    }

    fn make_state(name: &str, priority: u32, healthy: bool) -> BackendState {
        let now = Instant::now();
        BackendState {
            config: make_backend(name, priority),
            healthy,
            models: Vec::new(),
            current_model: None,
            gpu_metrics: None,
            failure_count: 0,
            last_check: now,
            last_request: now,
            vram_total_mb: None,
            vram_populated: false,
            queue_depth: None,
            ttft_p50_ms: None,
            vram_free_mb: None,
            max_concurrent: None,
            last_served: std::collections::BTreeMap::new(),
        }
    }

    fn pool_from_states(states: Vec<BackendState>) -> BackendPool {
        // Build via the public constructor, then swap in our pre-built states.
        // BackendPool::new initialises from Backend configs; we need full BackendState
        // control (health, models, gpu_metrics, etc.) so we replace directly.
        let pool = BackendPool::new(vec![], 5, Duration::from_secs(30));
        // SAFETY: we own this pool, just constructed — no concurrent access.
        *pool.backends.try_write().unwrap() = states;
        pool
    }

    fn default_scored_config() -> ScoredConfig {
        ScoredConfig::default()
    }

    /// Build a `ScoredRouter` with an empty (cold-start) `RoutingStats`.
    /// Existing tests that don't exercise Phase-3 dims use this helper.
    fn make_router(pool: BackendPool, cfg: &ScoredConfig) -> ScoredRouter {
        ScoredRouter::new(
            pool,
            cfg,
            Arc::new(RoutingStats::new()),
            Arc::new(SessionAffinity::new()),
        )
    }

    // ── Test 1 & determinism: same pool + request → same result every time ──

    #[tokio::test]
    async fn scored_picks_right_backend() {
        // B has better GPU (lower utilization) → should win.
        let mut state_a = make_state("alpha", 50, true);
        let mut state_b = make_state("beta", 50, true);
        state_a.models = vec!["llama3:8b".to_string()];
        state_b.models = vec!["llama3:8b".to_string()];
        state_a.gpu_metrics = Some(GpuMetrics {
            utilization: 80.0,
            memory_used: 4096,
            memory_total: 8192,
            temperature: 60.0,
        });
        state_b.gpu_metrics = Some(GpuMetrics {
            utilization: 10.0,
            memory_used: 1024,
            memory_total: 8192,
            temperature: 50.0,
        });

        let pool = pool_from_states(vec![state_a, state_b]);
        let router = make_router(pool, &default_scored_config());

        let result = router
            .route_scored(
                Some("llama3:8b"),
                None,
                &HashSet::new(),
                &RouteContext::default(),
            )
            .await
            .unwrap();
        assert_eq!(result.name, "beta");
    }

    #[tokio::test]
    async fn determinism_run_twice() {
        let mut state_a = make_state("node-a", 50, true);
        let mut state_b = make_state("node-b", 50, true);
        state_a.models = vec!["m".to_string()];
        state_b.models = vec!["m".to_string()];
        state_a.gpu_metrics = Some(GpuMetrics {
            utilization: 70.0,
            memory_used: 6000,
            memory_total: 8000,
            temperature: 75.0,
        });
        state_b.gpu_metrics = Some(GpuMetrics {
            utilization: 20.0,
            memory_used: 2000,
            memory_total: 8000,
            temperature: 50.0,
        });

        let pool = pool_from_states(vec![state_a, state_b]);
        let router = make_router(pool, &default_scored_config());
        let exc = HashSet::new();
        let ctx = RouteContext::default();

        let r1 = router
            .route_scored(Some("m"), None, &exc, &ctx)
            .await
            .unwrap();
        let r2 = router
            .route_scored(Some("m"), None, &exc, &ctx)
            .await
            .unwrap();
        assert_eq!(r1.name, r2.name);
    }

    #[tokio::test]
    async fn determinism_order_invariant() {
        // Shuffled input order must produce the same winner.
        let make = |name: &str, util: f32| {
            let mut s = make_state(name, 50, true);
            s.models = vec!["m".to_string()];
            s.gpu_metrics = Some(GpuMetrics {
                utilization: util,
                memory_used: 2000,
                memory_total: 8000,
                temperature: 55.0,
            });
            s
        };
        let sa = make("aaa", 80.0);
        let sb = make("bbb", 20.0); // best util → should win

        let pool1 = pool_from_states(vec![sa.clone(), sb.clone()]);
        let pool2 = pool_from_states(vec![sb, sa]);

        let cfg = default_scored_config();
        let r1 = make_router(pool1, &cfg)
            .route_scored(Some("m"), None, &HashSet::new(), &RouteContext::default())
            .await
            .unwrap();
        let r2 = make_router(pool2, &cfg)
            .route_scored(Some("m"), None, &HashSet::new(), &RouteContext::default())
            .await
            .unwrap();
        assert_eq!(r1.name, r2.name);
        assert_eq!(r1.name, "bbb");
    }

    // ── Test 2: gate-before-score ────────────────────────────────────────────

    #[tokio::test]
    async fn gate_before_score_model_absent_not_returned() {
        // X does NOT have the model but would score highest on GPU.
        // Y has the model and must win.
        let mut state_x = make_state("x-no-model", 50, true);
        let mut state_y = make_state("y-has-model", 50, true);
        // X has great GPU but lacks the model
        state_x.gpu_metrics = Some(GpuMetrics {
            utilization: 0.0,
            memory_used: 0,
            memory_total: 32768,
            temperature: 40.0,
        });
        state_y.models = vec!["llama3:8b".to_string()];
        state_y.gpu_metrics = Some(GpuMetrics {
            utilization: 90.0,
            memory_used: 7000,
            memory_total: 8000,
            temperature: 80.0,
        });

        // Strict gate: X has no model → must never be chosen.
        let mut cfg = default_scored_config();
        cfg.model_gate = ModelGate::Strict;
        let pool = pool_from_states(vec![state_x, state_y]);
        let router = make_router(pool, &cfg);

        let result = router
            .route_scored(
                Some("llama3:8b"),
                None,
                &HashSet::new(),
                &RouteContext::default(),
            )
            .await
            .unwrap();
        assert_eq!(result.name, "y-has-model");
    }

    #[tokio::test]
    async fn gate_before_score_unhealthy_not_returned() {
        let mut state_good = make_state("good", 50, true);
        let mut state_bad = make_state("bad", 100, false); // higher priority but unhealthy
        state_good.models = vec!["m".to_string()];
        state_bad.models = vec!["m".to_string()];

        let pool = pool_from_states(vec![state_good, state_bad]);
        let router = make_router(pool, &default_scored_config());

        let result = router
            .route_scored(Some("m"), None, &HashSet::new(), &RouteContext::default())
            .await
            .unwrap();
        assert_eq!(result.name, "good");
    }

    // ── Test 3: missing-value neutrality ────────────────────────────────────
    //
    // Q6=(b) semantics: Pᵢ = set of candidates for which dim i is present₀.
    // If Pᵢ has only one member, the dim is trivially uniform → dropped by
    // the pre-pass. The non-reporter is NOT penalized.
    // A genuinely discriminating signal (|Pᵢ| ≥ 2, values differ) DOES sort
    // candidates: good beats bad; a non-reporter sits at neutral (weight-dropped).

    #[tokio::test]
    async fn missing_value_neutrality_two_reporters_good_wins_over_bad() {
        // Three candidates: A (good temp 45°C), B (bad temp 84°C), C (no metrics).
        // Pᵢ = {A, B} for gpu_temperature → NOT uniform (different quantized norms)
        // → dim survives pre-pass → A wins.  C is neutral (weight-dropped).
        let cfg = ScoredConfig {
            weights: ScoredWeights {
                model_resident: 0.0,
                model_fits_vram: 0.0,
                prompt_size_vs_capacity: 0.0,
                gpu_utilization: 0.0,
                vram_headroom: 0.0,
                gpu_temperature: 5.0,
                operator_priority: 0.0,
                tag_affinity: 0.0,
                backend_type_affinity: 0.0,
                ..ScoredWeights::default()
            },
            ..default_scored_config()
        };

        let mut sa = make_state("a-good-temp", 50, true);
        let mut sb = make_state("b-bad-temp", 50, true);
        let mut sc = make_state("c-no-metrics", 50, true);
        for s in [&mut sa, &mut sb, &mut sc] {
            s.models = vec!["m".to_string()];
        }
        sa.gpu_metrics = Some(GpuMetrics {
            utilization: 50.0,
            memory_used: 4000,
            memory_total: 8000,
            temperature: 45.0,
        });
        sb.gpu_metrics = Some(GpuMetrics {
            utilization: 50.0,
            memory_used: 4000,
            memory_total: 8000,
            temperature: 84.0,
        });
        // C has no gpu_metrics.

        let pool = pool_from_states(vec![sa, sb, sc]);
        let result = make_router(pool, &cfg)
            .route_scored(Some("m"), None, &HashSet::new(), &RouteContext::default())
            .await
            .unwrap();
        // A has genuinely good temp; the dim discriminates → A wins.
        assert_eq!(result.name, "a-good-temp");
    }

    #[tokio::test]
    async fn missing_value_neutrality_single_reporter_dim_dropped() {
        // A (bad temp 84°C), B (no metrics). Pᵢ = {A} → single-member → trivially
        // uniform → dropped. Both score 0.5 → tie-break by name asc.
        // This proves the non-reporter is NOT penalized for not reporting.
        let cfg = ScoredConfig {
            weights: ScoredWeights {
                model_resident: 0.0,
                model_fits_vram: 0.0,
                prompt_size_vs_capacity: 0.0,
                gpu_utilization: 0.0,
                vram_headroom: 0.0,
                gpu_temperature: 5.0,
                operator_priority: 0.0,
                tag_affinity: 0.0,
                backend_type_affinity: 0.0,
                ..ScoredWeights::default()
            },
            ..default_scored_config()
        };

        let mut sa = make_state("a-bad-temp", 50, true);
        let mut sb = make_state("b-no-metrics", 50, true);
        sa.models = vec!["m".to_string()];
        sb.models = vec!["m".to_string()];
        sa.gpu_metrics = Some(GpuMetrics {
            utilization: 50.0,
            memory_used: 4000,
            memory_total: 8000,
            temperature: 84.0,
        });
        // B has no gpu_metrics. Pᵢ = {A} → single-member → trivially uniform → dropped.
        // Both score 0.5 → name asc: "a-bad-temp" < "b-no-metrics" → A wins on name.
        let pool = pool_from_states(vec![sa, sb]);
        let result = make_router(pool, &cfg)
            .route_scored(Some("m"), None, &HashSet::new(), &RouteContext::default())
            .await
            .unwrap();
        // Tie at 0.5; "a-bad-temp" < "b-no-metrics" lexicographically.
        assert_eq!(result.name, "a-bad-temp");
    }

    // ── Test 5: tie-break by name ────────────────────────────────────────────

    #[tokio::test]
    async fn tiebreak_name_asc() {
        // Two identical backends (same priority, no metrics) → lexicographically smaller name wins.
        let mut state_a = make_state("zz-last", 50, true);
        let mut state_b = make_state("aa-first", 50, true);
        state_a.models = vec!["m".to_string()];
        state_b.models = vec!["m".to_string()];

        let pool1 = pool_from_states(vec![state_a.clone(), state_b.clone()]);
        let pool2 = pool_from_states(vec![state_b, state_a]);
        let cfg = default_scored_config();

        let r1 = make_router(pool1, &cfg)
            .route_scored(Some("m"), None, &HashSet::new(), &RouteContext::default())
            .await
            .unwrap();
        let r2 = make_router(pool2, &cfg)
            .route_scored(Some("m"), None, &HashSet::new(), &RouteContext::default())
            .await
            .unwrap();
        assert_eq!(r1.name, "aa-first");
        assert_eq!(r2.name, "aa-first");
    }

    // ── Test 6: priority tie-break ───────────────────────────────────────────

    #[tokio::test]
    async fn tiebreak_priority_desc() {
        let mut lo = make_state("low-prio", 10, true);
        let mut hi = make_state("high-prio", 90, true);
        lo.models = vec!["m".to_string()];
        hi.models = vec!["m".to_string()];
        // No GPU metrics → all dims absent except operator_priority.

        // Use only operator_priority with weight (everything else 0).
        let cfg = ScoredConfig {
            weights: ScoredWeights {
                operator_priority: 5.0,
                model_resident: 0.0,
                model_fits_vram: 0.0,
                prompt_size_vs_capacity: 0.0,
                gpu_utilization: 0.0,
                vram_headroom: 0.0,
                gpu_temperature: 0.0,
                tag_affinity: 0.0,
                backend_type_affinity: 0.0,
                ..ScoredWeights::default()
            },
            ..default_scored_config()
        };

        let pool = pool_from_states(vec![lo, hi]);
        let result = make_router(pool, &cfg)
            .route_scored(Some("m"), None, &HashSet::new(), &RouteContext::default())
            .await
            .unwrap();
        assert_eq!(result.name, "high-prio");
    }

    // ── Test 7: 503-on-empty ─────────────────────────────────────────────────

    #[tokio::test]
    async fn returns_err_on_empty_pool() {
        let pool = pool_from_states(vec![]);
        let router = make_router(pool, &default_scored_config());
        let result = router
            .route_scored(Some("m"), None, &HashSet::new(), &RouteContext::default())
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn returns_err_when_all_unhealthy() {
        let mut s = make_state("sick", 50, false);
        s.models = vec!["m".to_string()];
        let pool = pool_from_states(vec![s]);
        let router = make_router(pool, &default_scored_config());
        let result = router
            .route_scored(Some("m"), None, &HashSet::new(), &RouteContext::default())
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn strict_gate_errors_when_no_model_resident() {
        let s = make_state("node", 50, true); // models is empty
        let pool = pool_from_states(vec![s]);
        let mut cfg = default_scored_config();
        cfg.model_gate = ModelGate::Strict;
        let result = make_router(pool, &cfg)
            .route_scored(Some("m"), None, &HashSet::new(), &RouteContext::default())
            .await;
        assert!(result.is_err());
    }

    // ── Test 8: relaxed fallback ─────────────────────────────────────────────

    #[tokio::test]
    async fn relaxed_gate_falls_back_when_no_resident() {
        // Model not resident anywhere; healthy backends exist → relaxed should not 503.
        let s = make_state("node", 50, true); // no models
        let pool = pool_from_states(vec![s]);
        let mut cfg = default_scored_config();
        cfg.model_gate = ModelGate::Relaxed;
        let result = make_router(pool, &cfg)
            .route_scored(Some("m"), None, &HashSet::new(), &RouteContext::default())
            .await;
        assert!(result.is_ok());
    }

    // ── Test 10: Phase 2+ inertness ──────────────────────────────────────────

    #[tokio::test]
    async fn phase2_dims_inert_in_phase1() {
        // Two backends with agent telemetry set; routing with high Phase-2 weights
        // must give the same result as routing with those weights at 0, because the
        // source Options are None for enrolled (non-agent) backends.
        let mut s1 = make_state("n1", 50, true);
        let mut s2 = make_state("n2", 50, true);
        s1.models = vec!["m".to_string()];
        s2.models = vec!["m".to_string()];
        s1.gpu_metrics = Some(GpuMetrics {
            utilization: 10.0,
            memory_used: 1000,
            memory_total: 8000,
            temperature: 50.0,
        });
        s2.gpu_metrics = Some(GpuMetrics {
            utilization: 80.0,
            memory_used: 7000,
            memory_total: 8000,
            temperature: 70.0,
        });
        // Phase-2 fields stay None (simulating non-agent backend).

        let cfg_no_phase2 = ScoredConfig::default();
        let mut cfg_with_phase2 = ScoredConfig::default();
        cfg_with_phase2.weights.queue_depth = 100.0;
        cfg_with_phase2.weights.ttft_p50 = 100.0;
        cfg_with_phase2.weights.precise_vram_free = 100.0;

        let r1 = make_router(
            pool_from_states(vec![s1.clone(), s2.clone()]),
            &cfg_no_phase2,
        )
        .route_scored(Some("m"), None, &HashSet::new(), &RouteContext::default())
        .await
        .unwrap();
        let r2 = make_router(pool_from_states(vec![s1, s2]), &cfg_with_phase2)
            .route_scored(Some("m"), None, &HashSet::new(), &RouteContext::default())
            .await
            .unwrap();
        assert_eq!(r1.name, r2.name);
    }

    // ── Test 12a: Q6 call-uniform drop ───────────────────────────────────────

    #[tokio::test]
    async fn q6_uniform_dim_dropped_discriminating_wins() {
        // All backends have same temperature (uniform → dropped).
        // Only vram_headroom differs → that dim decides.
        let temp = 60.0_f32;
        let mut s1 = make_state("low-vram", 50, true);
        let mut s2 = make_state("high-vram", 50, true);
        s1.models = vec!["m".to_string()];
        s2.models = vec!["m".to_string()];
        s1.gpu_metrics = Some(GpuMetrics {
            utilization: 50.0,
            memory_used: 7500,
            memory_total: 8000,
            temperature: temp,
        });
        s2.gpu_metrics = Some(GpuMetrics {
            utilization: 50.0,
            memory_used: 2000,
            memory_total: 8000,
            temperature: temp,
        });

        // Weight only temperature and vram_headroom.
        let cfg = ScoredConfig {
            weights: ScoredWeights {
                model_resident: 0.0,
                model_fits_vram: 0.0,
                prompt_size_vs_capacity: 0.0,
                gpu_utilization: 0.0,
                vram_headroom: 3.0,
                gpu_temperature: 3.0, // uniform → will be dropped
                operator_priority: 0.0,
                tag_affinity: 0.0,
                backend_type_affinity: 0.0,
                ..ScoredWeights::default()
            },
            ..default_scored_config()
        };

        let pool = pool_from_states(vec![s1, s2]);
        let result = make_router(pool, &cfg)
            .route_scored(Some("m"), None, &HashSet::new(), &RouteContext::default())
            .await
            .unwrap();
        // high-vram wins (vram_headroom discriminates); temperature dropped.
        assert_eq!(result.name, "high-vram");

        // Control: same result when temperature weight is 0.
        let cfg_no_temp = ScoredConfig {
            weights: ScoredWeights {
                model_resident: 0.0,
                model_fits_vram: 0.0,
                prompt_size_vs_capacity: 0.0,
                gpu_utilization: 0.0,
                vram_headroom: 3.0,
                gpu_temperature: 0.0,
                operator_priority: 0.0,
                tag_affinity: 0.0,
                backend_type_affinity: 0.0,
                ..ScoredWeights::default()
            },
            ..default_scored_config()
        };
        let mut sv1 = make_state("low-vram", 50, true);
        let mut sv2 = make_state("high-vram", 50, true);
        sv1.models = vec!["m".to_string()];
        sv2.models = vec!["m".to_string()];
        sv1.gpu_metrics = Some(GpuMetrics {
            utilization: 50.0,
            memory_used: 7500,
            memory_total: 8000,
            temperature: temp,
        });
        sv2.gpu_metrics = Some(GpuMetrics {
            utilization: 50.0,
            memory_used: 2000,
            memory_total: 8000,
            temperature: temp,
        });
        let pool2 = pool_from_states(vec![sv1, sv2]);
        let result2 = make_router(pool2, &cfg_no_temp)
            .route_scored(Some("m"), None, &HashSet::new(), &RouteContext::default())
            .await
            .unwrap();
        assert_eq!(result2.name, "high-vram");
    }

    #[tokio::test]
    async fn q6_all_identical_fleet_no_panic() {
        // All identical → all dims uniform → all score 0.5 → tie-break by name.
        let mut s1 = make_state("alpha", 50, true);
        let mut s2 = make_state("beta", 50, true);
        let mut s3 = make_state("gamma", 50, true);
        for s in [&mut s1, &mut s2, &mut s3] {
            s.models = vec!["m".to_string()];
            s.gpu_metrics = Some(GpuMetrics {
                utilization: 50.0,
                memory_used: 4000,
                memory_total: 8000,
                temperature: 60.0,
            });
        }

        let pool = pool_from_states(vec![s1, s2, s3]);
        let result = make_router(pool, &default_scored_config())
            .route_scored(Some("m"), None, &HashSet::new(), &RouteContext::default())
            .await;
        // Must not panic; winner is "alpha" (lexicographically smallest).
        assert!(result.is_ok());
        assert_eq!(result.unwrap().name, "alpha");
    }

    // ── Dim-6 temperature sentinel guard ─────────────────────────────────────

    #[tokio::test]
    async fn temperature_zero_sentinel_not_favored() {
        // A=50°C (cool, real), B=80°C (hot, real), C=0.0 (sensor unavailable).
        // Pre-guard, C's temp norm is (85-0)/45 → clamp 1.0 → C wins dim 6 with a
        // bogus "perfect coolness". With the guard, C's temperature is treated as
        // source-absent (neutral, weight-dropped); P_temp = {A, B} discriminates,
        // and the genuinely-coolest real reporter (A) wins. The sentinel node must
        // NOT be favored just because its driver reported 0 °C.
        let cfg = ScoredConfig {
            weights: ScoredWeights {
                model_resident: 0.0,
                model_fits_vram: 0.0,
                prompt_size_vs_capacity: 0.0,
                gpu_utilization: 0.0,
                vram_headroom: 0.0,
                gpu_temperature: 5.0,
                operator_priority: 0.0,
                tag_affinity: 0.0,
                backend_type_affinity: 0.0,
                ..ScoredWeights::default()
            },
            ..default_scored_config()
        };

        let mut sa = make_state("a-cool", 50, true);
        let mut sb = make_state("b-hot", 50, true);
        let mut sc = make_state("c-nosensor", 50, true);
        for s in [&mut sa, &mut sb, &mut sc] {
            s.models = vec!["m".to_string()];
        }
        // Same memory everywhere so vram_headroom (weight 0 anyway) can't sway it.
        sa.gpu_metrics = Some(GpuMetrics {
            utilization: 50.0,
            memory_used: 4000,
            memory_total: 8000,
            temperature: 50.0,
        });
        sb.gpu_metrics = Some(GpuMetrics {
            utilization: 50.0,
            memory_used: 4000,
            memory_total: 8000,
            temperature: 80.0,
        });
        sc.gpu_metrics = Some(GpuMetrics {
            utilization: 50.0,
            memory_used: 4000,
            memory_total: 8000,
            temperature: 0.0, // sensor unavailable — must not score a perfect norm
        });

        let pool = pool_from_states(vec![sa, sb, sc]);
        let result = make_router(pool, &cfg)
            .route_scored(Some("m"), None, &HashSet::new(), &RouteContext::default())
            .await
            .unwrap();
        assert_eq!(
            result.name, "a-cool",
            "coolest real reporter should win; the 0°C sentinel must not be favored"
        );
        assert_ne!(
            result.name, "c-nosensor",
            "a backend with an unavailable temp sensor must not win on temperature"
        );
    }

    // ── T6: sanitize_weights tests ───────────────────────────────────────────

    #[test]
    fn sanitize_negative_weight_reset_to_default() {
        let mut w = ScoredWeights {
            gpu_utilization: -1.0,
            ..ScoredWeights::default()
        };
        sanitize_weights(&mut w);
        assert_eq!(w.gpu_utilization, 3.0);
    }

    #[test]
    fn sanitize_nan_weight_reset_to_default() {
        let mut w = ScoredWeights {
            vram_headroom: f64::NAN,
            ..ScoredWeights::default()
        };
        sanitize_weights(&mut w);
        assert_eq!(w.vram_headroom, 2.0);
    }

    #[test]
    fn sanitize_all_zero_phase1_restores_defaults() {
        let mut w = ScoredWeights {
            model_resident: 0.0,
            model_fits_vram: 0.0,
            prompt_size_vs_capacity: 0.0,
            gpu_utilization: 0.0,
            vram_headroom: 0.0,
            gpu_temperature: 0.0,
            operator_priority: 0.0,
            tag_affinity: 0.0,
            backend_type_affinity: 0.0,
            ..ScoredWeights::default()
        };
        sanitize_weights(&mut w);
        // Should restore defaults.
        assert_eq!(w.gpu_utilization, 3.0);
        assert_eq!(w.model_resident, 5.0);
    }

    // ── Phase 2 Slice 1: live-load telemetry dims (10, 11, 13) ───────────────

    /// dim 10 — an idle backend (`queue_depth: Some(0)`) out-ranks a saturated
    /// one (`Some(8)`). All other dims are uniform → dropped, so queue_depth is
    /// the sole differentiator. Anti-trivial: with QueueDepth weight at 0 (or the
    /// dim removed) the two tie and "alpha" wins by name — so a "beta" win proves
    /// the dim is live.
    #[tokio::test]
    async fn queue_depth_idle_outranks_busy() {
        let mut busy = make_state("alpha", 50, true);
        let mut idle = make_state("beta", 50, true);
        busy.models = vec!["m".to_string()];
        idle.models = vec!["m".to_string()];
        busy.queue_depth = Some(8); // saturated → 0.0
        idle.queue_depth = Some(0); // empty → 1.0

        let pool = pool_from_states(vec![busy, idle]);
        let router = make_router(pool, &default_scored_config());
        let result = router
            .route_scored(Some("m"), None, &HashSet::new(), &RouteContext::default())
            .await
            .unwrap();
        assert_eq!(result.name, "beta", "idle backend (Some(0)) must win");
    }

    /// dim 10 — `Some(0)` is a REAL signal, scored (not absent). Three backends:
    /// idle=Some(0), busy=Some(8), silent=None. queue_depth is present on the two
    /// reporters with differing values, so it survives the call-uniform pre-pass
    /// and the idle reporter's 1.0 wins. Anti-trivial against the `Some(0)→None`
    /// bug: the idle node is named LAST alphabetically, so were `Some(0)` mistaken
    /// for absent, the dim would collapse to a single reporter → dropped → the
    /// name tie-break hands the win to "aaa-busy". Asserting "zzz-idle" proves
    /// `Some(0)` is matched on the `Option`, never via a `.unwrap_or(0)` proxy.
    #[tokio::test]
    async fn queue_depth_some_zero_scored_not_absent() {
        let mut busy = make_state("aaa-busy", 50, true);
        let mut silent = make_state("mmm-silent", 50, true);
        let mut idle = make_state("zzz-idle", 50, true);
        for s in [&mut busy, &mut silent, &mut idle] {
            s.models = vec!["m".to_string()];
        }
        busy.queue_depth = Some(8); // → 0.0
        idle.queue_depth = Some(0); // → 1.0 (the signal under test)
                                    // silent leaves queue_depth = None (genuinely absent)

        let pool = pool_from_states(vec![busy, silent, idle]);
        let router = make_router(pool, &default_scored_config());
        let result = router
            .route_scored(Some("m"), None, &HashSet::new(), &RouteContext::default())
            .await
            .unwrap();
        assert_eq!(
            result.name, "zzz-idle",
            "Some(0) must be scored as the best queue signal, not flattened to absent"
        );
    }

    /// dim 11 — a backend with low TTFT out-ranks a slow one. Other dims uniform
    /// → dropped, so ttft_p50 decides. Anti-trivial via name (slow = "alpha").
    #[tokio::test]
    async fn ttft_p50_fast_outranks_slow() {
        let mut slow = make_state("alpha", 50, true);
        let mut fast = make_state("beta", 50, true);
        slow.models = vec!["m".to_string()];
        fast.models = vec!["m".to_string()];
        slow.ttft_p50_ms = Some(1900); // near TTFT_REF → ~0.05
        fast.ttft_p50_ms = Some(100); // → 0.95

        let pool = pool_from_states(vec![slow, fast]);
        let router = make_router(pool, &default_scored_config());
        let result = router
            .route_scored(Some("m"), None, &HashSet::new(), &RouteContext::default())
            .await
            .unwrap();
        assert_eq!(result.name, "beta", "lower TTFT must win");
    }

    /// dim 13 SUPERSEDES dim 5. Construct the signals in OPPOSITE directions:
    /// "zeta" has poor gpu_metrics headroom (dim 5 low) but high precise free-VRAM
    /// (dim 13 high); "alpha" is the reverse. With supersession only dim 13 counts
    /// → "zeta" wins. WITHOUT supersession dim 5 also counts → the two tie at 0.5
    /// and "alpha" wins by name. Asserting "zeta" proves dim 5 is dropped.
    #[tokio::test]
    async fn precise_vram_free_supersedes_vram_headroom() {
        let mut alpha = make_state("alpha", 50, true);
        let mut zeta = make_state("zeta", 50, true);
        alpha.models = vec!["m".to_string()];
        zeta.models = vec!["m".to_string()];
        // Identical utilization + temperature → those dims are call-uniform → dropped.
        let gm = |used: u64| GpuMetrics {
            utilization: 50.0,
            memory_used: used,
            memory_total: 8000,
            temperature: 60.0,
        };
        // alpha: high gpu headroom (dim5 0.875), low precise free (dim13 0.125)
        alpha.gpu_metrics = Some(gm(1000));
        alpha.vram_total_mb = Some(8000);
        alpha.vram_free_mb = Some(1000);
        // zeta: low gpu headroom (dim5 0.125), high precise free (dim13 0.875)
        zeta.gpu_metrics = Some(gm(7000));
        zeta.vram_total_mb = Some(8000);
        zeta.vram_free_mb = Some(7000);

        let pool = pool_from_states(vec![alpha, zeta]);
        let router = make_router(pool, &default_scored_config());
        let result = router
            .route_scored(Some("m"), None, &HashSet::new(), &RouteContext::default())
            .await
            .unwrap();
        assert_eq!(
            result.name, "zeta",
            "precise free-VRAM must supersede gpu_metrics headroom (no double-count)"
        );
    }

    /// Supersession is PER-CANDIDATE, tested directly on `compute_raw` (free of
    /// the call-uniform pre-pass, which would drop a single-reporter dim). An
    /// agent node with precise telemetry has dim 13 present and dim 5 cleared; a
    /// static node with only gpu_metrics keeps dim 5 and has dim 13 absent — so a
    /// static node is never penalized for lacking agent telemetry.
    #[test]
    fn supersession_is_per_candidate_in_compute_raw() {
        let gm = GpuMetrics {
            utilization: 50.0,
            memory_used: 1000,
            memory_total: 8000,
            temperature: 60.0,
        };
        // Agent node: gpu_metrics + precise free/total VRAM.
        let mut agent = make_state("agent", 50, true);
        agent.gpu_metrics = Some(gm.clone());
        agent.vram_total_mb = Some(8000);
        agent.vram_free_mb = Some(4000);
        let raw_a = compute_raw(
            &agent,
            None,
            None,
            false,
            None,
            50,
            50,
            None,
            None,
            Instant::now(),
            None,
        );
        assert!(
            raw_a.present0[Dimension::PreciseVramFree.idx()],
            "agent dim 13 must be present"
        );
        assert!(
            !raw_a.present0[Dimension::VramHeadroom.idx()],
            "agent dim 5 must be superseded (cleared) when dim 13 is present"
        );

        // Static node: gpu_metrics only, no agent telemetry.
        let mut stat = make_state("static", 50, true);
        stat.gpu_metrics = Some(gm);
        let raw_s = compute_raw(
            &stat,
            None,
            None,
            false,
            None,
            50,
            50,
            None,
            None,
            Instant::now(),
            None,
        );
        assert!(
            !raw_s.present0[Dimension::PreciseVramFree.idx()],
            "static dim 13 must be absent"
        );
        assert!(
            raw_s.present0[Dimension::VramHeadroom.idx()],
            "static dim 5 must survive (not penalized for missing precise telemetry)"
        );
    }

    // ── Phase 2 Slice 2: concurrency_saturation (dim 12) ─────────────────────

    /// dim 12 — with equal absolute queue depth (dim 10 uniform → dropped), the
    /// backend with MORE concurrency headroom (higher max_concurrent → lower
    /// saturation) wins. Anti-trivial: remove dim 12 and the two tie → "alpha"
    /// wins by name; a "zeta" win proves dim 12 decided.
    #[tokio::test]
    async fn concurrency_saturation_more_headroom_wins() {
        let mut tight = make_state("alpha", 50, true);
        let mut roomy = make_state("zeta", 50, true);
        tight.models = vec!["m".to_string()];
        roomy.models = vec!["m".to_string()];
        // Equal queue_depth → dim 10 is call-uniform → dropped.
        tight.queue_depth = Some(2);
        roomy.queue_depth = Some(2);
        // Different capacity → dim 12 differs: tight 1-2/4=0.5, roomy 1-2/16=0.875.
        tight.max_concurrent = Some(4);
        roomy.max_concurrent = Some(16);

        let pool = pool_from_states(vec![tight, roomy]);
        let router = make_router(pool, &default_scored_config());
        let result = router
            .route_scored(Some("m"), None, &HashSet::new(), &RouteContext::default())
            .await
            .unwrap();
        assert_eq!(result.name, "zeta", "more concurrency headroom must win");
    }

    /// dim 12 div-by-zero guard + coexistence (no supersession of dim 10), tested
    /// directly on `compute_raw`. A degenerate `max_concurrent: Some(0)` or a
    /// missing `max_concurrent`/`queue_depth` → dim 12 absent. When both are
    /// present, dim 12 AND dim 10 are both present (coexist, unlike dim 13/5).
    #[test]
    fn concurrency_saturation_guard_and_coexistence_in_compute_raw() {
        let dim12 = Dimension::ConcurrencySaturation.idx();
        let dim10 = Dimension::QueueDepth.idx();

        // Valid: both present and max > 0 → dim 12 present, dim 10 still present.
        let mut ok = make_state("ok", 50, true);
        ok.queue_depth = Some(3);
        ok.max_concurrent = Some(8);
        let raw = compute_raw(
            &ok,
            None,
            None,
            false,
            None,
            50,
            50,
            None,
            None,
            Instant::now(),
            None,
        );
        assert!(
            raw.present0[dim12],
            "dim 12 present when both known and max>0"
        );
        assert!(
            raw.present0[dim10],
            "dim 10 must still be present — dim 12 does NOT supersede it"
        );

        // Guard: max_concurrent Some(0) → dim 12 absent (no div-by-zero).
        let mut zero_cap = make_state("zerocap", 50, true);
        zero_cap.queue_depth = Some(3);
        zero_cap.max_concurrent = Some(0);
        let raw = compute_raw(
            &zero_cap,
            None,
            None,
            false,
            None,
            50,
            50,
            None,
            None,
            Instant::now(),
            None,
        );
        assert!(
            !raw.present0[dim12],
            "Some(0) capacity → dim 12 absent (guard)"
        );

        // Missing max_concurrent → dim 12 absent (dim 10 unaffected).
        let mut no_cap = make_state("nocap", 50, true);
        no_cap.queue_depth = Some(3);
        let raw = compute_raw(
            &no_cap,
            None,
            None,
            false,
            None,
            50,
            50,
            None,
            None,
            Instant::now(),
            None,
        );
        assert!(!raw.present0[dim12], "no max_concurrent → dim 12 absent");
        assert!(
            raw.present0[dim10],
            "dim 10 still present without max_concurrent"
        );
    }

    // ── Phase 4 Slice A: network_locality (dim 19) & power_cost (dim 20) ──────

    /// dim 19 — exact tier map (local 1.0 / lan 0.8 / tailnet 0.6 / wan 0.4),
    /// present iff the operator configured a locality; absent (neutral, never a
    /// penalty) when None. Tested directly on `compute_raw`.
    #[test]
    fn network_locality_tier_map_in_compute_raw() {
        use crate::config::LocalityTier;
        let dim = Dimension::NetworkLocality.idx();
        for (tier, expected) in [
            (LocalityTier::Local, 1.0_f64),
            (LocalityTier::Lan, 0.8),
            (LocalityTier::Tailnet, 0.6),
            (LocalityTier::Wan, 0.4),
        ] {
            let mut s = make_state("n", 50, true);
            s.config.locality = Some(tier);
            let raw = compute_raw(
                &s,
                None,
                None,
                false,
                None,
                50,
                50,
                None,
                None,
                Instant::now(),
                None,
            );
            assert!(raw.present0[dim], "dim 19 present when locality configured");
            assert!(
                (raw.norms[dim] - expected).abs() < 1e-9,
                "tier {tier} → {expected}"
            );
        }
        // Unconfigured → absent (neutral, weight-dropped), never a penalty.
        let s = make_state("n", 50, true);
        let raw = compute_raw(
            &s,
            None,
            None,
            false,
            None,
            50,
            50,
            None,
            None,
            Instant::now(),
            None,
        );
        assert!(!raw.present0[dim], "dim 19 absent when locality is None");
    }

    /// dim 20 — n = clamp(1 - cost/COST_REF, 0, 1): zero-cost → 1.0, COST_REF/2 →
    /// 0.5, ≥COST_REF → 0.0 (clamped). Present iff `power_cost` configured;
    /// absent (neutral) when None. Tested directly on `compute_raw`.
    #[test]
    fn power_cost_formula_and_absence_in_compute_raw() {
        let dim = Dimension::PowerCost.idx();
        let with_cost = |c: f64| {
            let mut s = make_state("b", 50, true);
            s.config.power_cost = Some(c);
            s
        };

        let raw = compute_raw(
            &with_cost(0.0),
            None,
            None,
            false,
            None,
            50,
            50,
            None,
            None,
            Instant::now(),
            None,
        );
        assert!(raw.present0[dim], "dim 20 present when cost configured");
        assert!((raw.norms[dim] - 1.0).abs() < 1e-9, "zero cost → 1.0");

        let raw = compute_raw(
            &with_cost(COST_REF / 2.0),
            None,
            None,
            false,
            None,
            50,
            50,
            None,
            None,
            Instant::now(),
            None,
        );
        assert!((raw.norms[dim] - 0.5).abs() < 1e-9, "half COST_REF → 0.5");

        // Above COST_REF clamps to 0.0 (never negative).
        let raw = compute_raw(
            &with_cost(COST_REF * 2.0),
            None,
            None,
            false,
            None,
            50,
            50,
            None,
            None,
            Instant::now(),
            None,
        );
        assert!((raw.norms[dim]).abs() < 1e-9, "≥COST_REF clamps to 0.0");

        // Unconfigured → absent (neutral, weight-dropped).
        let s = make_state("b", 50, true);
        let raw = compute_raw(
            &s,
            None,
            None,
            false,
            None,
            50,
            50,
            None,
            None,
            Instant::now(),
            None,
        );
        assert!(!raw.present0[dim], "dim 20 absent when power_cost is None");
    }

    /// dim 19 end-to-end — the closer backend wins. Anti-trivial: the WAN node is
    /// named "alpha" (would win the name tie-break) and the local node "zeta"; a
    /// "zeta" win proves dim 19 decided, not the tie-break. Weight opt-in (default
    /// is `w_zero`), so the test sets it explicitly.
    #[tokio::test]
    async fn network_locality_closer_backend_wins() {
        use crate::config::LocalityTier;
        let mut far = make_state("alpha", 50, true);
        let mut near = make_state("zeta", 50, true);
        far.models = vec!["m".to_string()];
        near.models = vec!["m".to_string()];
        far.config.locality = Some(LocalityTier::Wan);
        near.config.locality = Some(LocalityTier::Local);

        let mut cfg = ScoredConfig::default();
        cfg.weights.network_locality = 100.0;
        let pool = pool_from_states(vec![far, near]);
        let router = make_router(pool, &cfg);
        let result = router
            .route_scored(Some("m"), None, &HashSet::new(), &RouteContext::default())
            .await
            .unwrap();
        assert_eq!(result.name, "zeta", "closer (local) backend must win");
    }

    /// dim 20 end-to-end — the cheaper backend wins. Anti-trivial: the expensive
    /// node is "alpha", the cheap node "zeta"; a "zeta" win proves dim 20 decided.
    #[tokio::test]
    async fn power_cost_cheaper_backend_wins() {
        let mut pricey = make_state("alpha", 50, true);
        let mut cheap = make_state("zeta", 50, true);
        pricey.models = vec!["m".to_string()];
        cheap.models = vec!["m".to_string()];
        pricey.config.power_cost = Some(90.0);
        cheap.config.power_cost = Some(10.0);

        let mut cfg = ScoredConfig::default();
        cfg.weights.power_cost = 100.0;
        let pool = pool_from_states(vec![pricey, cheap]);
        let router = make_router(pool, &cfg);
        let result = router
            .route_scored(Some("m"), None, &HashSet::new(), &RouteContext::default())
            .await
            .unwrap();
        assert_eq!(result.name, "zeta", "cheaper backend must win");
    }

    // ── Phase 4 Slice B: warm_model_recency (dim 23) ─────────────────────────

    /// dim 23 — recency norm from per-model last-served time. Served now → 1.0,
    /// half WARM_REF ago → 0.5, ≥WARM_REF ago → 0.0 (clamped); a model never
    /// served here reads neutral 0.5 (unknown, not a penalty); absent entirely
    /// when no model is requested. Direct on `compute_raw` with a fixed `now` so
    /// ages are deterministic.
    #[test]
    fn warm_model_recency_formula_and_absence_in_compute_raw() {
        let dim = Dimension::WarmModelRecency.idx();
        let now = Instant::now();
        let served_ago = |secs: u64| {
            let mut s = make_state("w", 50, true);
            s.last_served
                .insert("m".to_string(), now - Duration::from_secs(secs));
            s
        };

        // Served just now → 1.0
        let raw = compute_raw(
            &served_ago(0),
            Some("m"),
            None,
            false,
            None,
            50,
            50,
            None,
            None,
            now,
            None,
        );
        assert!(
            raw.present0[dim],
            "dim 23 present when a model is requested"
        );
        assert!((raw.norms[dim] - 1.0).abs() < 1e-9, "served now → 1.0");

        // Half WARM_REF ago → 0.5
        let raw = compute_raw(
            &served_ago((WARM_REF / 2.0) as u64),
            Some("m"),
            None,
            false,
            None,
            50,
            50,
            None,
            None,
            now,
            None,
        );
        assert!((raw.norms[dim] - 0.5).abs() < 1e-9, "half WARM_REF → 0.5");

        // ≥WARM_REF ago → clamps to 0.0
        let raw = compute_raw(
            &served_ago(WARM_REF as u64 * 2),
            Some("m"),
            None,
            false,
            None,
            50,
            50,
            None,
            None,
            now,
            None,
        );
        assert!((raw.norms[dim]).abs() < 1e-9, "≥WARM_REF → 0.0");

        // Model requested but never served here → neutral 0.5 (unknown).
        let never = make_state("w", 50, true);
        let raw = compute_raw(
            &never,
            Some("m"),
            None,
            false,
            None,
            50,
            50,
            None,
            None,
            now,
            None,
        );
        assert!(
            raw.present0[dim],
            "present with a requested model even if never served"
        );
        assert!((raw.norms[dim] - 0.5).abs() < 1e-9, "never served → 0.5");

        // A DIFFERENT model is warm, but the requested one isn't → still 0.5.
        let mut other = make_state("w", 50, true);
        other.last_served.insert("other".to_string(), now);
        let raw = compute_raw(
            &other,
            Some("m"),
            None,
            false,
            None,
            50,
            50,
            None,
            None,
            now,
            None,
        );
        assert!(
            (raw.norms[dim] - 0.5).abs() < 1e-9,
            "a different warm model leaves the requested one at 0.5"
        );

        // No model requested → dim 23 absent (neutral, weight-dropped).
        let raw = compute_raw(
            &served_ago(0),
            None,
            None,
            false,
            None,
            50,
            50,
            None,
            None,
            now,
            None,
        );
        assert!(
            !raw.present0[dim],
            "dim 23 absent when no model is requested"
        );
    }

    /// dim 23 end-to-end — the backend that served the model more recently wins.
    /// Anti-trivial: the stale node is "alpha", the freshly-served node "zeta"; a
    /// "zeta" win proves dim 23 decided. Weight opt-in, set explicitly.
    #[tokio::test]
    async fn warm_model_recency_recent_backend_wins() {
        let now = Instant::now();
        let mut stale = make_state("alpha", 50, true);
        let mut warm = make_state("zeta", 50, true);
        stale.models = vec!["m".to_string()];
        warm.models = vec!["m".to_string()];
        stale
            .last_served
            .insert("m".to_string(), now - Duration::from_secs(280));
        warm.last_served.insert("m".to_string(), now);

        let mut cfg = ScoredConfig::default();
        cfg.weights.warm_model_recency = 100.0;
        let pool = pool_from_states(vec![stale, warm]);
        let router = make_router(pool, &cfg);
        let result = router
            .route_scored(Some("m"), None, &HashSet::new(), &RouteContext::default())
            .await
            .unwrap();
        assert_eq!(result.name, "zeta", "more recently served backend must win");
    }

    // ── Phase 4 Slice C: session_stickiness (dim 18) ─────────────────────────

    /// dim 18 — present iff a prior backend is known for the session. The matching
    /// candidate scores 1.0, all others 0.5; an unknown session (None sticky) →
    /// absent (neutral). Tested directly on `compute_raw`.
    #[test]
    fn session_stickiness_norm_and_absence_in_compute_raw() {
        let dim = Dimension::SessionStickiness.idx();
        let now = Instant::now();
        let b = make_state("beta", 50, true);

        // Sticky to THIS backend → 1.0.
        let raw = compute_raw(
            &b,
            None,
            None,
            false,
            None,
            50,
            50,
            None,
            None,
            now,
            Some("beta"),
        );
        assert!(raw.present0[dim], "present when a sticky backend is known");
        assert!(
            (raw.norms[dim] - 1.0).abs() < 1e-9,
            "matching backend → 1.0"
        );

        // Sticky to ANOTHER backend → 0.5.
        let raw = compute_raw(
            &b,
            None,
            None,
            false,
            None,
            50,
            50,
            None,
            None,
            now,
            Some("other"),
        );
        assert!(raw.present0[dim]);
        assert!(
            (raw.norms[dim] - 0.5).abs() < 1e-9,
            "non-matching backend → 0.5"
        );

        // No sticky backend (new/unknown session) → absent.
        let raw = compute_raw(&b, None, None, false, None, 50, 50, None, None, now, None);
        assert!(!raw.present0[dim], "absent when no sticky backend is known");
    }

    /// dim 18 end-to-end — the session's last backend wins. Anti-trivial: the
    /// sticky backend is "zeta" (loses the name tie-break to "alpha"); a "zeta"
    /// win proves dim 18 decided. Affinity store pre-seeded; weight opt-in.
    #[tokio::test]
    async fn session_stickiness_sticky_backend_wins() {
        let mut a = make_state("alpha", 50, true);
        let mut z = make_state("zeta", 50, true);
        a.models = vec!["m".to_string()];
        z.models = vec!["m".to_string()];

        let affinity = Arc::new(SessionAffinity::new());
        affinity.record("sess-1", "zeta").await;

        let mut cfg = ScoredConfig::default();
        cfg.weights.session_stickiness = 100.0;
        let pool = pool_from_states(vec![a, z]);
        let router = ScoredRouter::new(pool, &cfg, Arc::new(RoutingStats::new()), affinity);

        let ctx = RouteContext {
            prompt_tokens: None,
            requested_ctx_len: None,
            session_id: Some("sess-1".to_string()),
        };
        let result = router
            .route_scored(Some("m"), None, &HashSet::new(), &ctx)
            .await
            .unwrap();
        assert_eq!(result.name, "zeta", "session's last backend must win");
    }

    // ── Phase 2: prompt_size_vs_capacity (dim 3) ─────────────────────────────

    /// dim 3 — given the same prompt, the backend with the LARGER context window
    /// (lower prompt/window ratio → higher norm) wins. Anti-trivial: remove dim 3
    /// and the two tie → "alpha" wins by name; a "zeta" win proves dim 3 decided.
    #[tokio::test]
    async fn prompt_size_larger_window_wins() {
        let mut small = make_state("alpha", 50, true);
        let mut large = make_state("zeta", 50, true);
        small.models = vec!["m".to_string()];
        large.models = vec!["m".to_string()];
        small.config.max_context_len = Some(4096);
        large.config.max_context_len = Some(32768);

        let pool = pool_from_states(vec![small, large]);
        let router = make_router(pool, &default_scored_config());
        let ctx = RouteContext {
            prompt_tokens: Some(4000),
            requested_ctx_len: None,
            session_id: None,
        };
        let result = router
            .route_scored(Some("m"), None, &HashSet::new(), &ctx)
            .await
            .unwrap();
        assert_eq!(result.name, "zeta", "larger context window must win");
    }

    /// dim 3 formula + presence, tested directly on `compute_raw`. n = (1-ratio)/0.5
    /// clamped: ≤50% of window → 1.0, 75% → 0.5, ≥100% → 0.0. Absent when either
    /// `prompt_tokens` or `max_context_len` is missing (neutral, not a penalty).
    #[test]
    fn prompt_size_formula_and_absence_in_compute_raw() {
        let dim3 = Dimension::PromptSizeVsCapacity.idx();
        let with_window = |ctx: u32| {
            let mut s = make_state("b", 50, true);
            s.config.max_context_len = Some(ctx);
            s
        };

        // 500/1000 = 50% → 1.0
        let raw = compute_raw(
            &with_window(1000),
            None,
            None,
            false,
            None,
            50,
            50,
            Some(500),
            None,
            Instant::now(),
            None,
        );
        assert!(raw.present0[dim3]);
        assert!((raw.norms[dim3] - 1.0).abs() < 1e-9, "≤50% window → 1.0");

        // 750/1000 = 75% → 0.5
        let raw = compute_raw(
            &with_window(1000),
            None,
            None,
            false,
            None,
            50,
            50,
            Some(750),
            None,
            Instant::now(),
            None,
        );
        assert!((raw.norms[dim3] - 0.5).abs() < 1e-9, "75% window → 0.5");

        // 1000/1000 = 100% → 0.0
        let raw = compute_raw(
            &with_window(1000),
            None,
            None,
            false,
            None,
            50,
            50,
            Some(1000),
            None,
            Instant::now(),
            None,
        );
        assert!((raw.norms[dim3]).abs() < 1e-9, "≥100% window → 0.0");

        // No prompt_tokens → absent even with a window configured.
        let raw = compute_raw(
            &with_window(1000),
            None,
            None,
            false,
            None,
            50,
            50,
            None,
            None,
            Instant::now(),
            None,
        );
        assert!(!raw.present0[dim3], "no prompt_tokens → dim 3 absent");

        // No max_context_len → absent even with a prompt size.
        let no_window = make_state("nw", 50, true);
        let raw = compute_raw(
            &no_window,
            None,
            None,
            false,
            None,
            50,
            50,
            Some(500),
            None,
            Instant::now(),
            None,
        );
        assert!(!raw.present0[dim3], "no max_context_len → dim 3 absent");
    }

    // ── Phase 3: dims 14–17 via compute_raw directly ─────────────────────────

    /// Helper: build a warm BackendModelStats (samples >= MIN_SAMPLES).
    fn warm_stats(
        ewma_latency_ms: f64,
        ewma_tps: f64,
        error_rate_bits: u32, // raw u32 packed into err_window
        health_transitions: u32,
    ) -> BackendModelStats {
        BackendModelStats {
            ewma_latency_ms,
            ewma_tps,
            err_window: error_rate_bits,
            samples: MIN_SAMPLES,
            health_transitions,
            last_update_tick: 1,
        }
    }

    /// Dims 14–17 are all present when stats is Some and samples >= MIN_SAMPLES,
    /// and absent (not present → neutral 0.5, weight-dropped) when None or cold.
    #[test]
    fn phase3_dims_present_when_warm_absent_when_cold() {
        let s = make_state("b", 50, true);

        // Warm stats: all four dims should be present.
        let st = warm_stats(1000.0, 50.0, 0, 0);
        let raw = compute_raw(
            &s,
            None,
            None,
            false,
            None,
            50,
            50,
            None,
            Some(&st),
            Instant::now(),
            None,
        );
        assert!(
            raw.present0[Dimension::EwmaLatency.idx()],
            "dim 14 must be present with warm stats"
        );
        assert!(
            raw.present0[Dimension::RecentErrorRate.idx()],
            "dim 15 must be present with warm stats"
        );
        assert!(
            raw.present0[Dimension::RecentSuccessThroughput.idx()],
            "dim 16 must be present with warm stats"
        );
        assert!(
            raw.present0[Dimension::FlapStability.idx()],
            "dim 17 must be present with warm stats"
        );

        // Cold (None): all four dims absent.
        let raw_cold = compute_raw(
            &s,
            None,
            None,
            false,
            None,
            50,
            50,
            None,
            None,
            Instant::now(),
            None,
        );
        assert!(
            !raw_cold.present0[Dimension::EwmaLatency.idx()],
            "dim 14 absent without stats"
        );
        assert!(
            !raw_cold.present0[Dimension::RecentErrorRate.idx()],
            "dim 15 absent without stats"
        );
        assert!(
            !raw_cold.present0[Dimension::RecentSuccessThroughput.idx()],
            "dim 16 absent without stats"
        );
        assert!(
            !raw_cold.present0[Dimension::FlapStability.idx()],
            "dim 17 absent without stats"
        );

        // Under-sampled (samples < MIN_SAMPLES): treated as cold → absent.
        let mut under = st.clone();
        under.samples = MIN_SAMPLES - 1;
        let raw_under = compute_raw(
            &s,
            None,
            None,
            false,
            None,
            50,
            50,
            None,
            Some(&under),
            Instant::now(),
            None,
        );
        assert!(
            !raw_under.present0[Dimension::EwmaLatency.idx()],
            "dim 14 absent when under-sampled"
        );
    }

    /// Dim 14 formula: n = clamp(1 - ewma_ms / LAT_REF, 0, 1).
    /// Low-latency backend out-scores high-latency one.
    #[test]
    fn dim14_ewma_latency_formula() {
        use crate::router::routing_stats::LAT_REF;
        let s = make_state("b", 50, true);

        // Half the reference latency → 0.5 norm.
        let st = warm_stats(LAT_REF / 2.0, 0.0, 0, 0);
        let raw = compute_raw(
            &s,
            None,
            None,
            false,
            None,
            50,
            50,
            None,
            Some(&st),
            Instant::now(),
            None,
        );
        let n14 = raw.norms[Dimension::EwmaLatency.idx()];
        assert!((n14 - 0.5).abs() < 1e-9, "LAT_REF/2 → 0.5, got {}", n14);

        // Near-zero latency → ~1.0.
        let st_fast = warm_stats(1.0, 0.0, 0, 0);
        let raw_fast = compute_raw(
            &s,
            None,
            None,
            false,
            None,
            50,
            50,
            None,
            Some(&st_fast),
            Instant::now(),
            None,
        );
        assert!(
            raw_fast.norms[Dimension::EwmaLatency.idx()] > 0.99,
            "1ms latency → near 1.0"
        );

        // Latency at or above LAT_REF → 0.0.
        let st_slow = warm_stats(LAT_REF, 0.0, 0, 0);
        let raw_slow = compute_raw(
            &s,
            None,
            None,
            false,
            None,
            50,
            50,
            None,
            Some(&st_slow),
            Instant::now(),
            None,
        );
        assert!(
            raw_slow.norms[Dimension::EwmaLatency.idx()] <= 0.0,
            "LAT_REF ms → 0.0"
        );
    }

    /// Dim 15 formula: n = 1 - error_rate. Low error rate → high norm.
    #[test]
    fn dim15_error_rate_formula() {
        let s = make_state("b", 50, true);

        // All successes (err_window = 0) → error_rate = 0 → n = 1.0.
        let st_clean = warm_stats(100.0, 0.0, 0b0000_0000, 0);
        let raw_clean = compute_raw(
            &s,
            None,
            None,
            false,
            None,
            50,
            50,
            None,
            Some(&st_clean),
            Instant::now(),
            None,
        );
        assert!(
            (raw_clean.norms[Dimension::RecentErrorRate.idx()] - 1.0).abs() < 1e-9,
            "all success → dim 15 = 1.0"
        );

        // Half errors (alternating bits for 32 samples) → rate 0.5 → n = 0.5.
        // 0xAAAAAAAA = 0b10101010... (16 of 32 bits set)
        let mut half = warm_stats(100.0, 0.0, 0xAAAA_AAAA, 0);
        half.samples = 32; // exactly 32 observations so denominator is 32.
        let raw_half = compute_raw(
            &s,
            None,
            None,
            false,
            None,
            50,
            50,
            None,
            Some(&half),
            Instant::now(),
            None,
        );
        assert!(
            (raw_half.norms[Dimension::RecentErrorRate.idx()] - 0.5).abs() < 1e-9,
            "50% errors → dim 15 = 0.5, got {}",
            raw_half.norms[Dimension::RecentErrorRate.idx()]
        );
    }

    /// Dim 16 formula: n = clamp(ewma_tps / TPS_REF, 0, 1).
    #[test]
    fn dim16_throughput_formula() {
        use crate::router::routing_stats::TPS_REF;
        let s = make_state("b", 50, true);

        // Half TPS_REF → 0.5.
        let st = warm_stats(100.0, TPS_REF / 2.0, 0, 0);
        let raw = compute_raw(
            &s,
            None,
            None,
            false,
            None,
            50,
            50,
            None,
            Some(&st),
            Instant::now(),
            None,
        );
        assert!(
            (raw.norms[Dimension::RecentSuccessThroughput.idx()] - 0.5).abs() < 1e-9,
            "TPS_REF/2 → 0.5"
        );

        // At or above TPS_REF → 1.0.
        let st_max = warm_stats(100.0, TPS_REF, 0, 0);
        let raw_max = compute_raw(
            &s,
            None,
            None,
            false,
            None,
            50,
            50,
            None,
            Some(&st_max),
            Instant::now(),
            None,
        );
        assert!(
            (raw_max.norms[Dimension::RecentSuccessThroughput.idx()] - 1.0).abs() < 1e-9,
            "TPS_REF → 1.0"
        );
    }

    /// Dim 17 formula: n = 1 - clamp(transitions / FLAP_REF, 0, 1).
    #[test]
    fn dim17_flap_stability_formula() {
        use crate::router::routing_stats::FLAP_REF;
        let s = make_state("b", 50, true);

        // 0 transitions → 1.0.
        let st_stable = warm_stats(100.0, 0.0, 0, 0);
        let raw_stable = compute_raw(
            &s,
            None,
            None,
            false,
            None,
            50,
            50,
            None,
            Some(&st_stable),
            Instant::now(),
            None,
        );
        assert!(
            (raw_stable.norms[Dimension::FlapStability.idx()] - 1.0).abs() < 1e-9,
            "0 transitions → 1.0"
        );

        // FLAP_REF transitions → 0.0.
        let st_flappy = warm_stats(100.0, 0.0, 0, FLAP_REF as u32);
        let raw_flappy = compute_raw(
            &s,
            None,
            None,
            false,
            None,
            50,
            50,
            None,
            Some(&st_flappy),
            Instant::now(),
            None,
        );
        assert!(
            raw_flappy.norms[Dimension::FlapStability.idx()] <= 0.0,
            "FLAP_REF transitions → 0.0"
        );
    }

    // ── Phase 3: integration — history dim decides the route ──────────────────

    /// Two warm candidates differ only on ewma_latency (dim 14); the lower-latency
    /// one wins. Anti-trivial: with equal stats or without history the two tie by
    /// name and "alpha" wins — so "zeta" winning proves dim 14 decided.
    #[tokio::test]
    async fn phase3_low_latency_backend_wins() {
        use crate::router::routing_stats::{RoutingStats, MIN_SAMPLES};

        let mut fast_node = make_state("zeta", 50, true);
        let mut slow_node = make_state("alpha", 50, true);
        fast_node.models = vec!["m".to_string()];
        slow_node.models = vec!["m".to_string()];

        let pool = pool_from_states(vec![slow_node, fast_node]);

        // Weight only ewma_latency so it's the sole discriminator.
        let cfg = ScoredConfig {
            weights: ScoredWeights {
                model_resident: 0.0,
                model_fits_vram: 0.0,
                prompt_size_vs_capacity: 0.0,
                gpu_utilization: 0.0,
                vram_headroom: 0.0,
                gpu_temperature: 0.0,
                operator_priority: 0.0,
                tag_affinity: 0.0,
                backend_type_affinity: 0.0,
                ewma_latency: 5.0,
                ..ScoredWeights::default()
            },
            ..default_scored_config()
        };

        // Seed the store: alpha is slow, zeta is fast.
        let rs = Arc::new(RoutingStats::new());
        // "alpha" backend: high latency (4500 ms → norm ≈ 0.1)
        // "zeta" backend: low latency (500 ms → norm = 0.9)
        for _ in 0..MIN_SAMPLES {
            rs.update("alpha", "m", 4500, false, None).await;
            rs.update("zeta", "m", 500, false, None).await;
        }

        let router = ScoredRouter::new(
            pool,
            &cfg,
            Arc::clone(&rs),
            Arc::new(SessionAffinity::new()),
        );
        let result = router
            .route_scored(Some("m"), None, &HashSet::new(), &RouteContext::default())
            .await
            .unwrap();
        assert_eq!(
            result.name, "zeta",
            "lower ewma_latency must win: fast zeta beats slow alpha"
        );
    }

    /// Cold-start neutrality: a backend with no history is not penalized to 0
    /// and remains selectable. With only ewma_latency weighted and one warm
    /// (medium-latency) candidate and one cold candidate, the two should not
    /// have the cold one excluded — it competes on name/priority tiebreak
    /// (not scored to 0). This test asserts the cold backend CAN win when it
    /// has the alphabetically smaller name (the tiebreak), proving neutrality.
    #[tokio::test]
    async fn phase3_cold_start_neutrality() {
        use crate::router::routing_stats::{RoutingStats, MIN_SAMPLES};

        let mut warm_node = make_state("zzz-warm", 50, true);
        let mut cold_node = make_state("aaa-cold", 50, true);
        warm_node.models = vec!["m".to_string()];
        cold_node.models = vec!["m".to_string()];

        let pool = pool_from_states(vec![warm_node, cold_node]);

        let cfg = ScoredConfig {
            weights: ScoredWeights {
                model_resident: 0.0,
                model_fits_vram: 0.0,
                prompt_size_vs_capacity: 0.0,
                gpu_utilization: 0.0,
                vram_headroom: 0.0,
                gpu_temperature: 0.0,
                operator_priority: 0.0,
                tag_affinity: 0.0,
                backend_type_affinity: 0.0,
                ewma_latency: 5.0,
                ..ScoredWeights::default()
            },
            ..default_scored_config()
        };

        let rs = Arc::new(RoutingStats::new());
        // Only "zzz-warm" has history; "aaa-cold" is cold (no entries).
        for _ in 0..MIN_SAMPLES {
            rs.update("zzz-warm", "m", 2000, false, None).await;
        }

        let router = ScoredRouter::new(
            pool,
            &cfg,
            Arc::clone(&rs),
            Arc::new(SessionAffinity::new()),
        );
        let result = router
            .route_scored(Some("m"), None, &HashSet::new(), &RouteContext::default())
            .await
            .unwrap();
        // Both candidates: "zzz-warm" has dim 14 present (n ≈ 0.6), "aaa-cold"
        // is cold so dim 14 absent → weight-dropped → effectively 0.5/0.
        // The dim is present on only ONE candidate → single-member Pᵢ →
        // call-uniform-drop pre-pass drops it → both score 0.5 → name tiebreak:
        // "aaa-cold" < "zzz-warm" → "aaa-cold" wins.
        // (This is exactly the missing-value-neutrality contract for Phase 3.)
        assert_eq!(
            result.name, "aaa-cold",
            "cold backend (no history) must not be penalized; name tiebreak decides"
        );
    }

    /// Determinism: same RoutingStats snapshot + same pool → same result twice.
    #[tokio::test]
    async fn phase3_determinism() {
        use crate::router::routing_stats::{RoutingStats, MIN_SAMPLES};

        let mut n1 = make_state("node-a", 50, true);
        let mut n2 = make_state("node-b", 50, true);
        n1.models = vec!["m".to_string()];
        n2.models = vec!["m".to_string()];

        let rs = Arc::new(RoutingStats::new());
        for _ in 0..MIN_SAMPLES {
            rs.update("node-a", "m", 4000, false, None).await;
            rs.update("node-b", "m", 500, false, None).await;
        }

        let cfg = ScoredConfig {
            weights: ScoredWeights {
                model_resident: 0.0,
                model_fits_vram: 0.0,
                prompt_size_vs_capacity: 0.0,
                gpu_utilization: 0.0,
                vram_headroom: 0.0,
                gpu_temperature: 0.0,
                operator_priority: 0.0,
                tag_affinity: 0.0,
                backend_type_affinity: 0.0,
                ewma_latency: 5.0,
                ..ScoredWeights::default()
            },
            ..default_scored_config()
        };

        let pool = pool_from_states(vec![n1, n2]);
        let router = ScoredRouter::new(
            pool,
            &cfg,
            Arc::clone(&rs),
            Arc::new(SessionAffinity::new()),
        );

        let r1 = router
            .route_scored(Some("m"), None, &HashSet::new(), &RouteContext::default())
            .await
            .unwrap();
        let r2 = router
            .route_scored(Some("m"), None, &HashSet::new(), &RouteContext::default())
            .await
            .unwrap();
        assert_eq!(r1.name, r2.name, "same stats + same pool → same route");
        assert_eq!(r1.name, "node-b", "lower latency backend must win");
    }
}
