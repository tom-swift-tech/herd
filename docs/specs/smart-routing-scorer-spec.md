# Smart-Routing Scorer: Weighted Multi-Dimensional Routing for Herd

## Overview

Herd ships four routing strategies today (`priority`, `model_aware`, `least_busy`,
`weighted_round_robin`). Each optimizes a **single axis**. `least_busy` looks only
at GPU utilization; `priority` looks only at operator-assigned rank; `model_aware`
prefers a model-resident node then falls back to one axis. None can express "prefer
the node that has the model loaded **and** is cool **and** has VRAM headroom **and**
is the operator's preferred tier" in one decision.

The **Scored** strategy replaces the single-axis decision with a weighted sum over
many normalized dimensions:

```
score(backend) = Σ wᵢ · normᵢ(backend)     over dimensions i that are present
route          = argmax(score), with a total tie-break
```

This is strictly more expressive than the four existing strategies — each of them is
a degenerate Scored configuration (`least_busy` ≈ Scored with all weight on GPU
utilization). It is also **auditable**: the per-candidate score breakdown is logged,
so an operator can answer "why did this request go *there*?" — something the existing
strategies cannot do.

Scored is **opt-in**. You only get it by setting `routing.strategy: scored`. With no
`routing.scored` block, sane built-in weights apply. Existing `herd.yaml` files are
unaffected — adding the variant and config block is purely additive.

### Design constraints (non-negotiable)

These are the properties the implementation MUST hold; the reviewer hunts for
violations of each:

1. **Three-stage pipeline: GATE → SCORE → SELECT.** Ineligible backends are
   *eliminated* in the gate, never scored-to-zero. Gate runs strictly before scoring.
2. **Determinism.** No RNG, no wall-clock reads in the scoring path. Fixed dimension
   iteration order. A total tie-break so two backends never order ambiguously. Same
   pool snapshot + same request ⇒ identical route, every time.
3. **Missing-value neutrality.** A dimension a backend cannot report degrades to a
   neutral `0.5` and that dimension's weight is dropped from *that backend's*
   active-weight denominator — a backend is never pushed below a peer merely for
   lacking telemetry it has no way to produce.
4. **No `unwrap`/`expect` in library code.** Backend-agnostic: Ollama and llama-server
   nodes are treated identically as OpenAI-compatible HTTP endpoints; the only
   backend-type *input* is the `backend-type affinity` dimension, which is a soft
   score nudge, never a gate.
5. **Backward-compatible config.** Omitted weights fall to defaults; an omitted block
   means all defaults; the variant defaults to disabled (you must explicitly select it).

---

## Architecture: where Scored fits

Scored is a fifth `Router` implementation. It changes **nothing** above or below the
router boundary:

- It implements the existing `Router` trait
  (`src/router/mod.rs`): `route_excluding(model, tags, excluded) -> Result<RoutedBackend>`.
- It returns the existing `RoutedBackend { name, url }` — name + URL only. The proxy
  handler (`src/api/openai.rs:359`), retry loop, exclusion accumulation, and
  503-on-empty all live **above** the router and stay byte-for-byte unchanged. When
  the gate eliminates every candidate, Scored returns `Err(anyhow!("No healthy
  backends available"))`, exactly as `model_aware`/`least_busy` do today — the caller
  maps that to 503.
- It reads the same `BackendPool` snapshot the other routers read.
- It is wired through `RouterEnum` and `create_router` (a new `RouterEnum::Scored` arm
  and `RoutingStrategy::Scored => …`).

```
                        proxy_handler (openai.rs)         ← unchanged
                                │  route_excluding(model, tags, excluded)
                                ▼
        ┌──────────────────────────────────────────────────┐
        │ ScoredRouter                                       │
        │                                                    │
        │  snapshot = pool.backends.read().await   (one read)│
        │                                                    │
        │  ┌── GATE ──────────────────────────────────────┐ │
        │  │ healthy ∧ ¬excluded ∧ tags⊆ ∧ model-resident │ │
        │  │ ∧ circuit-closed                             │ │
        │  └──────────────────────────────────────────────┘ │
        │                  │ candidates: Vec<&BackendState>  │
        │  ┌── SCORE ─────────────────────────────────────┐ │
        │  │ for each candidate, for each active dim:     │ │
        │  │   norm ∈ [0,1] (or 0.5 if not reportable)    │ │
        │  │   score = Σ wᵢ·normᵢ / Σ wᵢ  (present dims)  │ │
        │  └──────────────────────────────────────────────┘ │
        │  ┌── SELECT ────────────────────────────────────┐ │
        │  │ argmax by (score desc, priority desc, name↑) │ │
        │  └──────────────────────────────────────────────┘ │
        └──────────────────────────────────────────────────┘
                                │  RoutedBackend { name, url }
                                ▼
                        retry / exclusion / 503        ← unchanged
```

### The request-context problem (and its additive fix)

Some Phase-1 dimensions (`prompt size vs capacity`, `model fits VRAM`) need request
facts the current trait signature does not carry — `route_excluding` only gets
`model`, `tags`, `excluded`. Today the prompt is parsed in `openai.rs` *after* routing.

We do **not** break the trait. Instead:

- Add a default-method overload on `Router`:
  `route_scored(model, tags, excluded, ctx: &RouteContext)` whose default body simply
  calls `route_excluding`. Only `ScoredRouter` overrides it to consume `ctx`.
- `RouteContext` is a small, owned, `Default`-able struct of optional request facts:
  `{ prompt_tokens: Option<u32>, requested_ctx_len: Option<u32> }`. The proxy fills
  what it cheaply knows (it already extracts `prompt_tokens` from usage paths — see
  `server.rs:88`, `openai.rs:561`); when it does not, the fields are `None`.
- Inside Scored, a `None` context field makes the dependent dimension **not present**
  for that request (neutral, weight dropped) — never a penalty.

This keeps the four legacy routers untouched (they ignore the context) and keeps the
503/retry/exclusion machinery above the router unaware that Scored exists.

> **Decision for the lead:** the request-context extension is the one place the prompt
> was silent on *how* prompt-size data reaches the router. The chosen path is an
> additive default-trait-method + optional context struct, so Phase 0/1 can ship
> without the proxy ever populating it — those dimensions simply stay neutral until a
> later, trivial proxy wiring change turns them on. No interface above the router
> changes shape.

---

## The Dimension Catalog (23 dimensions, 4 phases)

Every dimension declares: **source**, **normalization to [0,1]**, **direction**, and
**activation phase**. Direction is normalized so that **higher `norm` is always
better** — lower-better raw signals are inverted during normalization, so the weighted
sum is always a maximization.

Normalization constants (`*_REF`, `*_MAX`) are fixed compile-time constants chosen for
typical local-GPU fleets; they are *not* config in Phase 0/1 (keeps the surface small).
A dimension is **present** for a backend only when its source datum exists; otherwise
it contributes neutral `0.5` and its weight is removed from that backend's denominator
(see Scoring Math).

### Group A — Model & placement (Phase 1)

| # | Dimension | Source | Normalization → [0,1] | Direction | Phase |
|---|-----------|--------|------------------------|-----------|-------|
| 1 | `model_resident` | `BackendState.models` vs request `model` | `1.0` if model in `models`; `0.5` if no model requested; `0.0` if requested model absent **but** backend otherwise eligible* | higher-better | 1 |
| 2 | `model_fits_vram` | request model est. size vs `vram_free_mb`/`vram_total_mb` | `1.0` if est_size ≤ free; linear ramp to `0.0` as est_size → total; `0.5` if size unknown | higher-better | 1 |
| 3 | `prompt_size_vs_capacity` | `RouteContext.prompt_tokens` vs node `max_context_len` (registry/db) | `1.0` if prompt ≤ 50% ctx; linear down to `0.0` at 100% ctx; `0.5` if either unknown | higher-better | 1 |

\* Note on #1: when a model **is** requested, model-absent backends are normally
removed by the **gate** (see GATE). Dimension #1 only meaningfully varies in the
fallback mode where no model is requested (all `0.5`) or where the operator runs Scored
in "soft model preference" mode (gate relaxed — see GATE options). The catalog lists it
as a dimension so soft-preference mode has a knob.

### Group B — GPU pressure (Phase 1)

| # | Dimension | Source | Normalization → [0,1] | Direction | Phase |
|---|-----------|--------|------------------------|-----------|-------|
| 4 | `gpu_utilization` | `GpuMetrics.utilization` (0–100) | `1.0 - util/100` | lower-better | 1 |
| 5 | `vram_headroom` | `GpuMetrics.memory_used`/`memory_total` (or `vram_free_mb`/`vram_total_mb`) | `free / total`, clamped [0,1] | higher-better | 1 |
| 6 | `gpu_temperature` | `GpuMetrics.temperature` (°C) | `clamp((TEMP_MAX - temp) / (TEMP_MAX - TEMP_MIN), 0, 1)`, `TEMP_MIN=40`, `TEMP_MAX=85` | lower-better | 1 |

### Group C — Operator intent & affinity (Phase 1)

| # | Dimension | Source | Normalization → [0,1] | Direction | Phase |
|---|-----------|--------|------------------------|-----------|-------|
| 7 | `operator_priority` | `Backend.priority` (u32) | `priority / max_priority_in_candidate_set`; if all equal → `1.0` | higher-better | 1 |
| 8 | `tag_affinity` | request `tags` ∩ `Backend.tags` | `matched_tags / requested_tags`; `0.5` if no tags requested | higher-better | 1 |
| 9 | `backend_type_affinity` | `Backend.backend` vs configured preference | `1.0` if matches preferred type, else `BACKEND_NEUTRAL=0.6`; `0.5` if no preference set | higher-better | 1 |

> Dimensions 1–9 are the **Phase-1 set**: they read only the pool snapshot, the request
> (model/tags/context), and config. **No agent telemetry is required** — a static
> `[[backends]]`-only fleet routes fully on these. Backends lacking `GpuMetrics`
> (common for static Ollama nodes) contribute neutral `0.5` for dims 4–6.

### Group D — Live load & latency (Phase 2 — agent telemetry)

| # | Dimension | Source | Normalization → [0,1] | Direction | Phase |
|---|-----------|--------|------------------------|-----------|-------|
| 10 | `queue_depth` | `BackendState.queue_depth: Option<u32>` (Phase 0) ← agent caps | `clamp(1 - depth/QUEUE_REF, 0, 1)`, `QUEUE_REF=8` | lower-better | 2 |
| 11 | `ttft_p50` | `BackendState.ttft_p50_ms: Option<u32>` (Phase 0) ← agent caps | `clamp(1 - ttft/TTFT_REF, 0, 1)`, `TTFT_REF=2000ms` | lower-better | 2 |
| 12 | `concurrency_saturation` | `queue_depth` vs `BackendState.max_concurrent: Option<u32>` (Phase 0; agent field future) | `clamp(1 - depth/max_concurrent, 0, 1)` | lower-better | 2 |
| 13 | `precise_vram_free` | `BackendState.vram_free_mb: Option<u64>` (Phase 0) ← agent caps | `vram_free_mb / vram_total_mb`, clamped | higher-better | 2 |

> Phase 2 sharpens Group B/C using *measured* agent telemetry instead of passively
> sampled `GpuMetrics`. Dim 13 supersedes dim 5 when present (precise free-VRAM beats
> derived). Dim 12's `max_concurrent` has **no field in `AgentCapabilities` yet**
> (registry.rs) — it is a Phase-0 `Option` on `BackendState` that stays `None` (→
> neutral) until the agent protocol reports it. Documented as future-populated.

### Group E — History & stability (Phase 3 — derived/EWMA)

| # | Dimension | Source | Normalization → [0,1] | Direction | Phase |
|---|-----------|--------|------------------------|-----------|-------|
| 14 | `ewma_latency` | rolling EWMA of observed completion latency per backend (new derived store) | `clamp(1 - ewma/LAT_REF, 0, 1)` | lower-better | 3 |
| 15 | `recent_error_rate` | EWMA of 5xx/timeout rate per backend (derive from `failure_count` history) | `1 - error_rate` | lower-better | 3 |
| 16 | `recent_success_throughput` | EWMA tokens/sec per backend | `clamp(tps / TPS_REF, 0, 1)` | higher-better | 3 |
| 17 | `flap_stability` | health-transition frequency (penalize recently-flapping nodes) | `1 - clamp(transitions/FLAP_REF, 0, 1)` | lower-better | 3 |

> Phase 3 introduces a **per-backend derived-stats store** updated from request
> outcomes. Determinism is preserved: the *scoring path* reads an immutable snapshot of
> the EWMA values; updates happen out-of-band after a request completes, never during
> scoring. EWMA decay uses request-count, not wall-clock, to keep the score path free of
> time reads.

### Group F — Locality, cost & capability (Phase 4 — policy/affinity)

| # | Dimension | Source | Normalization → [0,1] | Direction | Phase |
|---|-----------|--------|------------------------|-----------|-------|
| 18 | `session_stickiness` | session→backend affinity hint (agent sessions) | `1.0` if backend served this session's last turn (prompt-cache warm), else `0.5` | higher-better | 4 |
| 19 | `network_locality` | configured locality tier (same-host / LAN / Tailscale / WAN) per backend | tiered map: local `1.0`, LAN `0.8`, tailnet `0.6`, WAN `0.4` | higher-better | 4 |
| 20 | `power_cost` | per-backend cost/power weight (config, e.g. watts or $/1k tok) | `clamp(1 - cost/COST_REF, 0, 1)` | lower-better | 4 |
| 21 | `rpc_shard_capability` | `AgentCapabilities.rpc_capable` for tensor-parallel-eligible large models | `1.0` if request needs sharding ∧ `rpc_capable`; `0.5` if request doesn't need it; `0.0` if needed ∧ not capable | higher-better | 4 |
| 22 | `gpu_class_affinity` | `AgentCapabilities.gpu_model` vs model's preferred GPU class | `1.0` exact-class match, `0.7` same-vendor, `0.5` unknown | higher-better | 4 |
| 23 | `warm_model_recency` | how recently the requested model was served (prompt/KV cache warmth) | `clamp(1 - turns_since/WARM_REF, 0, 1)`; `0.5` if never/unknown | higher-better | 4 |

**Phase totals:** Phase 1 = **9** dims (1–9), Phase 2 = **4** (10–13), Phase 3 = **4**
(14–17), Phase 4 = **6** (18–23). **Total = 23.**

---

## 4-Phase Rollout

This sprint ships **Phase 0 + Phase 1 only.** Phases 2–4 are designed here so the
weight schema and scoring engine are forward-stable, but are out of scope for the
sprint.

### Phase 0 — Telemetry to the pool boundary (additive, no router yet)

Carry agent telemetry all the way to `BackendState` so it is *available* to scorers and
already improves `least_busy`. No new routing behavior.

- **`BackendState` gains four `Option` fields** (`src/backend/pool.rs`):
  `queue_depth: Option<u32>`, `ttft_p50_ms: Option<u32>`, `vram_free_mb: Option<u64>`,
  `max_concurrent: Option<u32>`. All default `None` in `new()`, `add()`, and the test
  helpers — existing constructors and static/enrolled backends are unaffected.
- **`pool_sync.rs::reconcile` populates them** from `AgentCapabilities` for `agent:`
  entries only (it already owns that prefix exclusively): `queue_depth =
  Some(caps.queue_depth)`, `ttft_p50_ms = caps.ttft_p50_ms`, `vram_free_mb =
  Some(caps.vram_free_mb)`, `max_concurrent = None` (no caps field yet). Apply on both
  the add and update branches.
- **Additive `least_busy` win (optional, low-risk):** `least_busy_cmp` may prefer
  `queue_depth` when present, falling back to `utilization`. Keep behind the existing
  comparator so the change is isolated and its tests are local.
- **Acceptance:** existing 321 tests pass; a new test asserts a reconciled `agent:`
  entry carries `queue_depth`/`ttft_p50_ms`/`vram_free_mb`, and a static entry leaves
  them `None`.

### Phase 1 — The Scored router (pool + request dimensions)

- `RoutingStrategy::Scored` (`#[serde(rename = "scored")]`) added to the enum and to its
  `Display` impl (`"scored"`).
- `routing.scored` config block with per-dimension `weights` (schema below).
- New `src/router/scored.rs`: `ScoredRouter { pool, weights }` implementing `Router`.
  Implements GATE → SCORE → SELECT over **dims 1–9 only**. Dims 10–23 are wired into the
  weight table as recognized keys but always evaluate **not present** in Phase 1 (their
  source `Option`s are `None`/absent), so they cleanly no-op until later phases populate
  them.
- `RouterEnum::Scored` arm + `create_router` mapping.
- **Acceptance:** determinism test, missing-value-neutrality test, gate-before-score
  test, tie-break test, 503-on-empty test (all below); `cargo build` clean, no
  `unwrap`/`expect` in `scored.rs`.

### Phase 2 — Live load & latency (dims 10–13)

Flip dims 10–13 from "always absent" to reading their Phase-0 `BackendState` fields.
Add `max_concurrent` to `AgentCapabilities` + agent protocol to light up dim 12. No
scoring-engine change — purely turning on source reads + default weights.

### Phase 3 — History & stability (dims 14–17)

Add the per-backend derived-stats store (EWMA, request-count decay) updated post-request
above the router and snapshotted into scoring. New store, no engine reshape.

### Phase 4 — Locality, cost & capability (dims 18–23)

Policy/affinity layer: session stickiness (agent sessions), locality tiers, power/cost
weights, RPC-shard capability, GPU-class affinity, warm-model recency.

---

## Scoring Math

For a single candidate backend `b` and the active weight set `W` (the configured,
post-default weights for every dimension):

```
Let D       = the fixed-order list of all 23 dimensions.
Let present(b, i) = true iff dimension i's source datum exists for b in this request.

normᵢ(b) =
    nᵢ(b)   if present(b, i)         // dimension's [0,1] formula
    0.5     otherwise                // neutral

activeᵢ(b) = wᵢ   if present(b, i) AND wᵢ > 0
             0    otherwise          // weight dropped for absent dims

denom(b)  = Σ over i of activeᵢ(b)

score(b)  = ( Σ over i of activeᵢ(b) · nᵢ(b) ) / denom(b)      if denom(b) > 0
            0.5                                                  if denom(b) == 0
```

Key consequences:

- **Active-weight renormalization.** The denominator sums only the weights of
  dimensions actually present for *this* backend. A backend that cannot report
  `gpu_temperature` is scored on the *remaining* dimensions at full strength — it is
  **not** dragged toward 0 by a missing axis, and its present dimensions are not diluted
  by a phantom 0.5 term. This is the formal statement of **missing-value neutrality**
  (constraint 3): two backends, one reporting a dimension at exactly `0.5` and one not
  reporting it at all, end up identical, never with the non-reporter behind.
- **Bounded output.** `score(b) ∈ [0,1]` always (weighted mean of values in `[0,1]`).
- **All-absent guard.** If a backend has zero active weight (every weighted dimension
  absent), it scores neutral `0.5` rather than dividing by zero — pure integer/float
  guard, no `unwrap`.
- **Weights are pre-normalized at config load**, not per request: the scorer stores
  weights as given; the per-backend denominator does the only normalization needed.
  (We do not globally renormalize configured weights to sum to 1 — the per-backend
  denominator makes the absolute scale irrelevant. This keeps config readable: an
  operator writes `gpu_utilization: 3.0` and reasons about it relative to other
  weights, not as a fraction.)

### SELECT — total tie-break ordering

`argmax` is defined by a **total** comparator so two distinct backends can never be
equal-ordered:

```
1. score        — descending   (compared as fixed-scale integers, see below)
2. priority     — descending   (Backend.priority, u32)
3. name         — ascending    (lexicographic, &str)  ← guarantees totality
```

`name` is unique per pool entry (it is the pool key), so step 3 always breaks any
remaining tie. The result is one unambiguous winner for any candidate set.

**Float-comparison determinism.** Scores are `f32` weighted means. To keep ordering
deterministic and avoid `partial_cmp` `NaN` ambiguity (the existing `least_busy_cmp`
falls back to `Ordering::Equal` on `NaN` — acceptable there, not here), Scored compares
scores as **quantized integers**: `score_q = (score * SCORE_SCALE).round() as i64` with
`SCORE_SCALE = 1_000_000`. Equal `score_q` falls through to priority then name. No
`NaN` can arise (inputs are clamped to `[0,1]`, denom guarded `> 0`), but quantization
makes "effectively equal" scores tie-break by the stable secondary keys rather than by
float noise.

### Determinism rules the scoring path obeys

- **No RNG** anywhere in gate/score/select.
- **No wall-clock reads.** Scored reads `BackendState` fields only; it never calls
  `Instant::now()`. (Phases 3/4 EWMA decay is request-count-based, computed off-path.)
- **Fixed dimension iteration order** — `D` is a `const` ordered list; iteration is over
  that list, never a `HashMap`. Weights are held in a fixed-order structure (array
  indexed by a `Dimension` enum, or a `BTreeMap<Dimension, f32>`), per the project's
  "`BTreeMap` over `HashMap` where determinism matters" rule.
- **Single snapshot.** One `pool.backends.read().await`, then all gate/score/select work
  on that borrowed slice — no interleaved awaits that could observe a mutated pool
  mid-decision.

---

## GATE Stage

The gate hard-eliminates ineligible backends **before** any score is computed. The
predicate is the conjunction (a backend passes only if **all** hold):

| Predicate | Maps to | Source |
|-----------|---------|--------|
| **healthy** | `b.healthy` | circuit breaker (`mark_unhealthy` past `failure_threshold`) |
| **not excluded** | `!excluded.contains(&b.config.name)` | retry-loop exclusion set |
| **tags satisfied** | `tags.iter().all(\|t\| b.config.tags.contains(t))` | request tags |
| **model resident** (when model specified, strict mode) | `b.models.contains(model)` | `BackendState.models` |
| **circuit closed** | subsumed by `healthy` (an open breaker sets `healthy = false`) | pool |

The first three predicates are **exactly** the existing `filter_healthy` helper in
`pool.rs`. Scored reuses that filter's logic (or calls a pool method that exposes the
filtered candidate slice) so gate semantics are identical to every other router — no new
eligibility behavior is invented. The model-resident predicate mirrors the
`.filter(|b| b.models.contains(...))` in `get_by_model_tagged_excluding`.

**Gate vs score — the blocking invariant:** model-absent / unhealthy / excluded
backends are *removed from the candidate set*, not given a low score. A test asserts
that a backend missing the requested model never appears in the scored set and never
wins, even if every other dimension would make it the top scorer.

**Strict vs soft model mode (config switch, defaults strict):**

- `model_gate: strict` (default) — model-absent backends are gated out. Matches
  `model_aware`'s "route to a node that has the model" intent. If this empties the set
  *and* a model was requested, the gate **relaxes once**: it falls back to the
  model-unaware candidate set (healthy ∧ tags ∧ ¬excluded) and lets dimension #1 carry
  the (now uniform `0.5`) model signal — this preserves `model_aware`'s existing
  fallback-to-priority behavior rather than 503-ing when a model is simply not loaded
  anywhere.
- `model_gate: soft` — model is never gated; dimension #1 expresses model residency as a
  strong score term instead. For operators who want "prefer the model-resident node but
  consider a fast idle node that would load it."

> **Decision for the lead:** the prompt specified "model-absent when model specified" as
> a gate. I kept that as the **default (strict)** but added the single-relax fallback so
> Scored does not regress `model_aware`'s "no node has the model → fall back to best
> available, don't 503" behavior. The `soft` mode is the documented escape hatch. If the
> lead wants pure-strict-no-relax (503 when model absent everywhere), that's a one-line
> change — flagged because it's a behavior choice, not a mechanical one.

---

## Config Schema

```yaml
routing:
  strategy: scored          # opt-in; anything else ⇒ Scored never constructed

  scored:
    model_gate: strict      # strict | soft   (default: strict)

    # Per-dimension weights. Any omitted key falls to its default below.
    # Absolute scale is irrelevant — only ratios matter (per-backend
    # renormalization). Weight 0 disables a dimension entirely.
    weights:
      # Group A — model & placement
      model_resident:          5.0
      model_fits_vram:         2.0
      prompt_size_vs_capacity: 1.0
      # Group B — GPU pressure
      gpu_utilization:         3.0
      vram_headroom:           2.0
      gpu_temperature:         1.0
      # Group C — operator intent & affinity
      operator_priority:       2.0
      tag_affinity:            1.0
      backend_type_affinity:   0.0
      # Group D — live load (Phase 2; recognized now, inert until populated)
      queue_depth:             0.0
      ttft_p50:                0.0
      concurrency_saturation:  0.0
      precise_vram_free:       0.0
      # Groups E/F (Phase 3–4) — recognized, default 0.0
      ewma_latency:            0.0
      recent_error_rate:       0.0
      recent_success_throughput: 0.0
      flap_stability:          0.0
      session_stickiness:      0.0
      network_locality:        0.0
      power_cost:              0.0
      rpc_shard_capability:    0.0
      gpu_class_affinity:      0.0
      warm_model_recency:      0.0
```

### Defaults table (Phase 1 active dims)

| Dimension | Default weight | Rationale |
|-----------|----------------|-----------|
| `model_resident` | 5.0 | Strongest soft signal; dominates when `model_gate: soft` |
| `gpu_utilization` | 3.0 | Primary live-load axis (matches `least_busy` instinct) |
| `model_fits_vram` | 2.0 | Avoid OOM / CPU-spill routing |
| `vram_headroom` | 2.0 | Headroom for concurrent loads |
| `operator_priority` | 2.0 | Operator intent matters but isn't absolute |
| `gpu_temperature` | 1.0 | Thermal fairness, low weight |
| `tag_affinity` | 1.0 | Already partly enforced by the tag gate |
| `prompt_size_vs_capacity` | 1.0 | Inert until proxy populates `RouteContext` |
| `backend_type_affinity` | 0.0 | Off by default — backend-agnostic principle |
| all Phase 2–4 dims | 0.0 | Recognized keys, inert until their phase |

### Backward-compatibility rules

- **No `routing.scored` block** ⇒ `ScoredConfig::default()` ⇒ the defaults table above.
  An existing `herd.yaml` with `strategy: model_aware` and no `scored` block is
  completely unaffected.
- **Partial `weights` block** ⇒ each missing key falls to its default (per-field
  `#[serde(default = ...)]` or a post-parse merge over a defaults map). Omitting a weight
  is **never** the same as setting it to 0.
- **Unknown weight key** ⇒ warn + ignore (per project rule "never bail! on config
  errors — degrade gracefully"). Never an error that fails config load.
- **`strategy` is the only switch.** There is no `scored.enabled` flag — selecting the
  strategy *is* the opt-in, consistent with how the other four strategies work (they
  have no `enabled` flag either).

---

## Determinism & Testability

Phase-1 test matrix (each a focused unit test in `scored.rs`, using the existing
`BackendPool::new` + `update_*` helpers):

1. **Determinism.** Build a fixed pool snapshot + fixed request; call the scorer N times;
   assert the identical `RoutedBackend.name` every time. Stronger: shuffle the *input
   order* of backends in the pool and assert the winner is invariant (the comparator's
   `name` key makes input order irrelevant).
2. **Gate-before-score.** Backend `X` has the requested model **absent** but is otherwise
   the obvious top scorer (idle, cool, high priority). Assert `X` is never returned in
   strict mode — it was gated, not scored-to-zero. A second assertion: an *unhealthy*
   top-scorer is also never returned.
3. **Missing-value neutrality.** Two backends identical except backend `A` reports
   `gpu_temperature` (a good 45 °C → high norm) and backend `B` reports **no**
   `GpuMetrics` at all. With temperature the only weighted dimension that differs, assert
   `A` wins (it has a genuinely good measured value) — but in a variant where `A`'s
   temperature is *bad* (84 °C → low norm), assert `B` (neutral, weight-dropped) wins.
   This proves the non-reporter sits at neutral, never penalized below a reporter and
   never advantaged over a genuinely-good reporter.
4. **Active-weight renormalization.** A backend reporting only `gpu_utilization` (all
   other dims absent) is scored purely on that dimension at full weight — assert its
   score equals its utilization norm, not a 0.5-diluted blend.
5. **Tie-break totality.** Two backends with identical scores and identical priority →
   the lexicographically smaller `name` wins, deterministically, across input
   reorderings.
6. **Priority tie-break.** Two backends with identical scores, different priority → higher
   priority wins.
7. **503-on-empty.** Empty pool, and separately a fully-gated pool (all unhealthy / all
   excluded / model absent everywhere in strict-no-relax), ⇒ `route_excluding` returns
   `Err` ⇒ caller 503s. (Mirrors `pool_sync`'s `stale_agent_is_drained_…` test shape.)
8. **Strict-relax fallback.** Model requested, resident on no backend, healthy backends
   exist ⇒ Scored relaxes the model gate once and returns the best-scored healthy backend
   (does **not** 503) — parity with `model_aware`.
9. **Config backward-compat.** Parse a `herd.yaml` with `strategy: scored` and **no**
   `scored` block ⇒ defaults table applied. Parse one with a partial `weights` block ⇒
   only the specified keys override; the rest default. Parse one with an unknown weight
   key ⇒ load succeeds with a warning.
10. **Phase-2+ inertness.** With Phase-1-only sources populated, assert dims 10–23
    contribute nothing (their `Option` sources are `None`), so routing decisions are
    identical whether their weights are `0.0` or unset.

---

## Auditability

Every routing decision emits one structured `tracing::debug!` line per *winner* and,
when the per-candidate detail is enabled, a breakdown line per candidate. This is the
feature the four legacy strategies cannot offer.

**Winner line (always at debug):**

```
scored route → agent:citadel  score=0.842  (candidates=4, gated_out=2, mode=strict)
```

**Per-candidate breakdown (debug, gated behind a cheap log-level check so it costs
nothing when off):**

```
scored: agent:citadel  score=0.842  denom=14.0
  model_resident=1.00·5.0  gpu_utilization=0.78·3.0  model_fits_vram=1.00·2.0
  vram_headroom=0.71·2.0   operator_priority=0.50·2.0  gpu_temperature=0.89·1.0
  tag_affinity=0.50·1.0    [absent: prompt_size_vs_capacity, queue_depth, ttft_p50, …]
scored: gpu-2          score=0.611  denom=11.0  [absent: gpu_temperature, vram_headroom, …]
```

Each candidate line lists, in **fixed dimension order**: `dim=norm·weight` for present
dimensions and a bracketed `[absent: …]` list for the rest, plus the `denom` so an
operator can hand-verify `score = Σ(norm·weight)/denom`. Because order is fixed and no
clock/RNG is involved, the same snapshot produces identical audit output — the log *is* a
reproducible explanation.

No new endpoint in Phase 1; the breakdown is log-only. (A future `GET
/admin/route-explain?model=…` that returns the breakdown as JSON is a natural Phase-3+
addition but is a non-goal here.)

---

## Non-Goals / Deferred

- **Phases 2–4 implementation.** Designed above for forward-stability of the weight
  schema and engine; not built this sprint. The sprint ships Phase 0 + Phase 1.
- **`max_concurrent` on `AgentCapabilities` / agent protocol.** The `BackendState` field
  exists (Phase 0, stays `None`); wiring the agent to report it is Phase 2.
- **Per-backend EWMA / derived-stats store** (dims 14–17) — Phase 3.
- **Session stickiness, locality, cost, RPC-shard, GPU-class, warm-recency** (dims
  18–23) — Phase 4.
- **Configurable normalization constants** (`TEMP_MAX`, `QUEUE_REF`, etc.) — fixed
  consts in Phase 0/1; promoting them to config is a later, additive change.
- **`/admin/route-explain` JSON endpoint** — log-only auditability in Phase 1.
- **Dashboard surfacing** of the score breakdown — follows the endpoint, later.
- **Proxy populating `RouteContext.prompt_tokens`** — the hook exists (default trait
  method + optional struct); actually filling it is a trivial later wiring step. Until
  then dims 2–3 stay neutral, by design.
```
