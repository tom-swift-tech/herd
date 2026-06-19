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

## Prerequisites & sprint sequencing (read before reviewing)

> This section exists so a reader does **not** mistake planned Phase-1 implementation deltas,
> or already-implemented Phase-0 work, for design gaps. A reviewer flagged
> "`BackendState` is missing the Phase-0 fields" and "`RouterEnum` has no `Scored` variant"
> as blockers — **neither is a defect**; both are explained here.

**Phase 0 is a separate, already-implemented change (PR #17, unmerged at time of writing).**
Phase 0 adds the four `Option` telemetry fields to `BackendState`
(`queue_depth: Option<u32>`, `ttft_p50_ms: Option<u32>`, `vram_free_mb: Option<u64>`,
`max_concurrent: Option<u32>`) and populates them in `pool_sync`. **Phase 1 (this sprint)
depends on PR #17 landing first.** This spec deliberately designs against those fields *as
specified*, not as they may or may not exist on any given branch — if they are absent in the
tree you are reading, Phase 0 has not merged yet, which is expected, not a missing-field bug.
Phase-1 dims 1–9 do **not** require the Phase-0 fields (they read `gpu_metrics`,
`models`, `config.*`, `RouteContext`); the Phase-0 fields only light up Phase-2 dims 10–13.

**Exact impl deltas Phase 1 introduces** (these are the work, not gaps — verified against
`src/router/mod.rs`):

1. **`RoutingStrategy::Scored`** — a new enum variant in `config.rs`
   (`#[serde(rename = "scored")]`) + its `Display` arm (`"scored"`). Currently the enum has
   four variants (`config.rs:178`).
2. **`RouteContext` struct + `route_scored` default trait method** on `Router`
   (`mod.rs:13`). The trait today has `route` (defaulted) and `route_excluding` (required).
   Adding a **defaulted** `route_scored` is backward-compatible: the four existing routers
   (`PriorityRouter`, `ModelAwareRouter`, `LeastBusyRouter`, `WeightedRoundRobinRouter`)
   inherit the default body and need **zero edits**.
3. **`RouterEnum::Scored(ScoredRouter)`** — a new arm in the `enum RouterEnum`
   (`mod.rs:33`) **and** a matching arm in `create_router` (`mod.rs:58`,
   `RoutingStrategy::Scored => RouterEnum::Scored(...)`).
4. **`RouterEnum` must add a `route_scored` method.** Subtle but important: `RouterEnum`'s
   `impl Router` (`mod.rs:41`) currently implements **only** `route_excluding` and matches
   all four variants. If `route_scored` is left to the trait default on `RouterEnum`, that
   default delegates to `RouterEnum::route_excluding` — which dispatches each variant's
   `route_excluding`, **bypassing `ScoredRouter::route_scored` entirely**. So `RouterEnum`
   must **override `route_scored`** with its own `match` that calls each variant's
   `route_scored` (the inherited default for the four legacy, the real override for
   `Scored`). This is the one place the trait default is *not* sufficient — call it out in
   review. Aside from this method, no other `RouterEnum` match changes are required.
5. **New module `src/router/scored.rs`** — `ScoredRouter { pool, weights, model_gate,
   prefer_backend_type }` implementing `route_excluding` (delegates to `route_scored` with
   `RouteContext::default()`) and `route_scored` (the real GATE→SCORE→SELECT).
6. **`ScoredConfig`/`ScoredWeights`/`ModelGate`** in `config.rs` (full shapes in Config
   Schema below) under `RoutingConfig` as `#[serde(default)] pub scored: ScoredConfig`.
7. **(Optional, to light up dims 2–3)** the call-site swap from `route_excluding` to
   `route_scored(.., &ctx)` on the `Scored` path. There are **two** routing call sites and
   both must be swapped to light up dims 2–3 (verified against source):
   - **`src/api/openai.rs:359`** — `chat_completions`' retry loop (the OpenAI-compat path).
   - **`src/server.rs:1439`** — `proxy_handler`'s retry loop (the generic/streaming proxy
     path, which pre-excludes profile-restricted backends then calls `route_excluding`).

   Without the swap, scored routing still works correctly with dims 2–3 dormant-neutral (see
   Prompt-Size section). *(Note: `src/server.rs:1597` is `record_request_labeled`, a
   post-request **metrics** hook — not a routing call site. It is referenced correctly as a
   metrics hook in the Phase-3 store section, but it is **not** where routing happens.)*

8. **Config-validation invariant: `backend.name` uniqueness + reserved prefixes** (required
   for the determinism proof — see SELECT and the proof's step 5). `config.rs::validate()`
   today does **not** check for duplicate `backend.name` nor reserve the `agent:` / `node:`
   prefixes (verified: `validate()` at `config.rs:937` only validates URLs and the warmer/CB
   knobs; `pool.add()` at `pool.rs:285` unconditionally pushes; `pool.update()` finds the
   *first* match by `config.name`). Three independent name namespaces share one `Vec`:
   static `[[backends]]`, enrolled `node:{id}` (owned by `NodeHealthPoller::sync_to_pool`),
   and agent `agent:{id}` (owned by `pool_sync.rs::reconcile`, which explicitly does **not**
   dedup vs static — `pool_sync.rs:259`, decision 14). A static entry named `agent:citadel`
   or `node:citadel` can therefore collide with a reconciler-owned key, producing **two
   distinct `Vec` elements with the same `name`** → the SELECT tie-break's `name asc` returns
   `Equal` for distinct candidates → `max_by` is no longer invariant under input reordering
   (and the pool `Vec` order *does* shift across reconcile cycles). Phase 1 closes this in
   `validate()` (house rule: **warn + degrade, never `bail!`**):
   - **Duplicate static `backend.name`:** keep the first, **drop** the later duplicate(s)
     with `warn!("duplicate backend name '{name}' — keeping first, dropping duplicate")`.
     (Dedup-with-warn fits the house "never bail!" rule better than rejecting the whole load;
     duplicate names already silently break `pool.update`/`pool.get`/metrics keyed by name,
     so dropping the duplicate is the strictly-safer existing-behavior-preserving choice.)
   - **Reserved prefixes:** a static `backend.name` beginning with `agent:` or `node:` is
     **renamed-rejected** — dropped with `warn!("backend name '{name}' uses reserved prefix
     'agent:'/'node:' — these namespaces are owned by the fleet reconcilers; skipping")`.
     This makes the three namespaces provably disjoint so a static entry can never collide
     with a reconciler key. (The reconcilers already own their prefixes exclusively; this
     just forbids static config from poaching them.)

Items 1–6 and **8** are required for Phase 1; item 7 is the prompt-size enablement step (can
lag). Item 8 is small but load-bearing: the determinism proof rests on `name` uniqueness,
so the invariant must be *enforced*, not assumed.

---

## Architecture: where Scored fits

Scored is a fifth `Router` implementation. It changes **nothing** above or below the
router boundary:

- It implements the existing `Router` trait
  (`src/router/mod.rs`): `route_excluding(model, tags, excluded) -> Result<RoutedBackend>`.
- It returns the existing `RoutedBackend { name, url }` — name + URL only. **Both** proxy
  retry loops (`src/api/openai.rs:359` in `chat_completions`, and `src/server.rs:1439` in
  `proxy_handler`), exclusion accumulation, and
  503-on-empty all live **above** the router and stay byte-for-byte unchanged (unless the
  optional dims-2–3 `route_scored` swap lands — see impl-delta item 7). When
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
        │  │ pre-pass: drop dims uniform across candidates│ │
        │  │   score = Σ wᵢ·normᵢ / Σ wᵢ  (surviving dims)│ │
        │  └──────────────────────────────────────────────┘ │
        │  ┌── SELECT ────────────────────────────────────┐ │
        │  │ argmax by (score desc, priority desc, name↑) │ │
        │  └──────────────────────────────────────────────┘ │
        └──────────────────────────────────────────────────┘
                                │  RoutedBackend { name, url }
                                ▼
                        retry / exclusion / 503        ← unchanged
```

### The request-context problem and its additive fix (item 4)

Some Phase-1 dimensions (`prompt_size_vs_capacity`, and the size half of `model_fits_vram`)
need request facts the current trait signature does not carry — `route_excluding` only
gets `model`, `tags`, `excluded`. Today the prompt is parsed in `openai.rs` *after* routing
has already chosen a backend. We do **not** widen `route_excluding` (that would touch all
four legacy routers and the call sites). Instead we add an **additive default trait method**:

```rust
// src/router/mod.rs — added to the Router trait

/// Optional request context for score-aware routing. All fields optional;
/// a None field makes its dependent dimension "not present" (neutral, weight-dropped).
#[derive(Clone, Debug, Default)]
pub struct RouteContext {
    /// Prompt size in tokens, if the proxy has cheaply estimated it pre-routing.
    pub prompt_tokens: Option<u32>,
    /// Requested context-window length, if the request carried one (e.g. num_ctx).
    pub requested_ctx_len: Option<u32>,
}

#[async_trait]
pub trait Router: Send + Sync {
    // ... existing route() and route_excluding() unchanged ...

    /// Score-aware route. DEFAULT body delegates to route_excluding, ignoring ctx,
    /// so the four legacy routers inherit it verbatim and are untouched.
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
```

- **Only `ScoredRouter` overrides `route_scored`** to consume `ctx`. `PriorityRouter`,
  `ModelAwareRouter`, `LeastBusyRouter`, `WeightedRoundRobinRouter` inherit the default body
  and are **not edited at all** — they keep ignoring context, exactly as today. `RouterEnum`
  gains a `route_scored` arm that dispatches to each variant's method (the default for the
  four legacy, the override for `Scored`).
- **Concrete call site (verified).** The proxy's retry loop calls the router at
  **`src/api/openai.rs:359`, inside `chat_completions` (fn at `openai.rs:104`)**:
  `self.router.read().await.route_excluding(model, tags, &excluded).await`. `ScoredRouter`'s
  own `route_excluding` impl **must delegate to `route_scored(model, tags, excluded,
  &RouteContext::default())`**, so this existing call site keeps working unchanged and yields
  *correct* scored routing — but with `prompt_tokens = None`, so **dims 2–3 stay dormant at
  neutral 0.5 forever**.
- **To light up the prompt-size dimensions, `chat_completions` must, on the `Scored` path,
  call `route_scored(model, tags, excluded, &ctx)` with a populated `RouteContext`** —
  estimating `prompt_tokens` from the parsed request body (the same extraction it already
  performs post-routing for usage logging, near `openai.rs:561`/`server.rs`). This is the
  load-bearing wiring: the *router* implementing `route_scored` is necessary but not
  sufficient; the **two** call sites must select it. **Until both call-site changes land,
  dims 2–3 are inert by construction — not broken, just neutral.** The two sites (verified
  against source):
  - **`src/api/openai.rs:359`** — `chat_completions`' retry loop.
  - **`src/server.rs:1439`** — `proxy_handler`'s retry loop (generic/streaming path). Each
    must swap `route_excluding(...)` → `route_scored(.., &ctx)` on the `Scored` path to light
    up dims 2–3 on that path; otherwise they stay neutral there. (`server.rs:1597` is
    `record_request_labeled`, a post-request metrics hook, **not** a routing call site.)
- Inside `ScoredRouter`, a `None` context field makes the dependent dimension **not
  present** for that request (neutral, weight dropped) — never a penalty. `prompt_tokens =
  None` ⇒ dim 3 not present; it never pushes a backend below a peer.

This keeps the four legacy routers byte-for-byte untouched, and the **503/retry/exclusion
machinery above the router stays unaware that Scored exists** — it still calls a `Router`
trait method, gets a `RoutedBackend { name, url }` or an `Err`, and maps `Err` to 503 as
today. The retry loop's exclusion accumulation flows through whichever method the proxy
calls; `route_scored`'s `excluded` parameter is the same `HashSet<String>` the loop already
maintains.

> **Two-method contract summary.** `ScoredRouter` implements **both**: `route_excluding`
> (delegates to `route_scored` with an empty ctx — so legacy callers are correct, dims 2–3
> dormant) and `route_scored` (the real entry point — dims 2–3 active iff the proxy fills
> ctx). The proxy chooses `route_scored` on the `Scored` strategy to turn prompt-size on.
> No interface above the router changes shape; this is purely additive.
>
> **Recursion-direction invariant (state once, here — the single source of truth).**
> Delegation flows in **exactly one direction**: `ScoredRouter::route_excluding` →
> `ScoredRouter::route_scored`. `route_scored` holds the real GATE→SCORE→SELECT body and
> **never** calls back into `route_excluding`. The reverse delegation (`route_scored`
> defaulting to `route_excluding`) is the **trait default body** for the *four legacy*
> routers only; `ScoredRouter` **overrides** `route_scored`, so on `ScoredRouter` the trait
> default is shadowed and the two methods do not form a cycle. Implementing both delegations
> on `ScoredRouter` (each calling the other) would be infinite recursion — the invariant is:
> **on `ScoredRouter`, only `route_excluding` delegates; `route_scored` is terminal.** This is
> the same constraint impl-delta item 4 raises for `RouterEnum` (its `route_scored` override
> must dispatch each variant's `route_scored`, never fall back through `route_excluding`).

---

## Normalization Strategy (absolute-threshold vs fleet-relative)

> **Decision (item 1).** Every Phase-1 dimension is classified as **absolute-threshold**
> or **fleet-relative**, and each has an exact, closed-form `normᵢ(b) ∈ [0,1]`. The rule
> is: **use absolute thresholds wherever a physical scale exists** (a percentage, a
> temperature, a byte count, a token count) and **fleet-relative only where the signal has
> no natural scale** (an operator-assigned rank). Absolute thresholds are more stable
> (a backend's norm does not move when its peers change), more auditable (the operator
> reads the constant and knows the curve), and free of the n=1 div-by-zero hazard.

### Why mostly absolute

Fleet-relative normalization (min/max across *this call's* candidate set) makes a
backend's score depend on its peers: the **same backend can score differently in two
different calls** because the candidate set differed. That is acceptable as a routing
property — see the Determinism Proof: routing only ever decides *among this call's
candidates*, and the contract we hold is **same snapshot + same request ⇒ same route**,
not cross-call score stability. But it is undesirable *gratuitously*: it makes the audit
log harder to reason about and reintroduces the n=1 / all-equal div-by-zero edge on every
relative dimension. So we confine fleet-relative to the one dimension that genuinely has
no fixed scale: `operator_priority` (a `u32` rank whose meaning is "relative to the other
nodes in this fleet" by construction).

### The n=1 / all-equal rule (a special case of the general call-uniform-drop rule)

For any fleet-relative dimension, if the candidate set has a single member **or** every
candidate reports the same raw value (`min == max`), the min/max denominator is zero. The
normalized value collapses to `0.5` (no division), **and then the general call-uniform
pre-pass (Scoring Math, Q6 = (b)) DROPS that dimension for the call** — since a value equal
across all candidates cannot discriminate, it is removed from every candidate's present-set
rather than scored. This is no longer a fleet-relative-only special case: it is the
**fleet-relative instance** of the rule that now applies to *every* dimension (Q6 resolved =
(b), 2026-06-14). Justification is unchanged and now general: a dimension that cannot
*discriminate* must not tilt the decision; dropping it leaves the remaining present
dimensions to decide. We deliberately do **not** map all-equal to `1.0` (that would have
inflated every candidate's mean) — but we also no longer leave it scored at `0.5`: an
undiscriminating dimension is **dropped**, so "can't tell them apart" is exactly equivalent to
"didn't report it," both weight-dropped. (The gate, not normalization, handles "no candidates
at all" — the scorer is never called with an empty set; the case where the pre-pass drops
*all* dimensions is handled by the `denom == 0 → 0.5` guard — see Determinism Proof.)

> **Absolute-threshold dims have no n=1 problem at all**: their norm is a function of the
> backend's own value and fixed constants, independent of peers.

---

## The Dimension Catalog (23 dimensions, 4 phases)

Every dimension declares: **source**, **normalization to [0,1]**, **direction**, and
**activation phase**. Direction is normalized so that **higher `norm` is always
better** — lower-better raw signals are inverted during normalization, so the weighted
sum is always a maximization. The **N-type** column marks each as **A** (absolute-
threshold, peer-independent) or **R** (fleet-relative, min/max over the candidate set).

Normalization constants (`*_REF`, `*_MAX`) are fixed compile-time constants chosen for
typical local-GPU fleets; they are *not* config in Phase 0/1 (keeps the surface small).
A dimension is **present** for a backend only when its source datum exists; otherwise
it contributes neutral `0.5` and its weight is removed from that backend's denominator
(see Scoring Math). All `nᵢ(b)` formulas below are **clamped to `[0,1]`** as their final
step — this is the second line of defence against NaN/out-of-range (see Determinism Proof).

### Group A — Model & placement (Phase 1)

| # | Dimension | N-type | Source | Normalization → [0,1] | Dir | Phase |
|---|-----------|:------:|--------|------------------------|-----|-------|
| 1 | `model_resident` | A | `BackendState.models` vs request `model` | see formula below | higher | 1 |
| 2 | `model_fits_vram` | A | est. model size vs `vram_free_mb` (else `gpu_metrics.memory_*`) | see formula below | higher | 1 |
| 3 | `prompt_size_vs_capacity` | A | `RouteContext.prompt_tokens` vs node ctx window | see formula below | higher | 1 |

**Dim 1 — `model_resident` (absolute).** *Enforced at the gate; self-neutralizes in scoring
(see `compute_raw` in `scored.rs`) — its weight is effectively the gate, not a score term.*
Presence depends on the gate's relax state, so the predicate is stated explicitly for both cases:
```
present₁(b) = (model requested) AND (b is model-resident OR the gate has relaxed)
            // ⇔ model requested AND (b.models.contains(model) OR relaxed == true)
            // if no model requested → NOT present → neutral 0.5

n₁(b) =  1.0   if b.models.contains(model)        // resident
         0.0   otherwise                          // absent (only reachable post-relax)
```
- **Pre-relax (or `strict`):** model-absent backends are removed by the GATE, so every
  scored candidate is resident and `n₁ = 1.0` uniformly. Because the value is identical across
  **all** candidates, the **call-uniform pre-pass (Scoring Math) DROPS dim 1 for that call** —
  it enters neither numerator nor denominator for any backend, exactly like an absent
  dimension. It therefore cannot shift the *level* or the ranking of any candidate; placement
  is decided entirely by the dimensions that actually differ (GPU pressure, priority, etc.).
- **Post-relax (`relaxed` mode, model resident nowhere):** the candidate set is the model-
  unaware healthy set; **every** candidate is model-absent, so `n₁ = 0.0` uniformly. Again the
  value is identical across all candidates, so the pre-pass **DROPS** dim 1 — its uniform `0.0`
  records "nobody had it" but contributes nothing to scoring (it cannot discriminate). This is
  the call-uniform-drop rule resolved under **Q6 = (b)**; see Scoring Math and open-questions.
- **Mixed sets never occur:** the gate either keeps only residents (pre-relax) or drops the
  model predicate entirely (post-relax). A candidate set is never part-resident/part-absent,
  so `n₁` is always uniform across the scored set — which means dim 1 is **always dropped by
  the pre-pass** in Phase 1 (it can never discriminate among candidates). It begins to matter
  only if a future gate mode admits a mixed resident/absent set, at which point it would no
  longer be call-uniform and would survive the pre-pass.

**Dim 2 — `model_fits_vram` (absolute).** Estimate model VRAM from the model name via the
existing `analytics::extract_param_billions(model)` helper (`Some(b)` → `est_mb ≈ b · 1024`
as a rough fp16-ish footprint; refine constant later). Free VRAM source priority:
`vram_free_mb` (Phase-2 agent field) → else derive from `gpu_metrics` as
`memory_total − memory_used`.
```
est_b   = extract_param_billions(model).filter(|b| *b > 0)   // Option<u64>, b > 0 only
est_mb  = est_b · 1024                                       // > 0 by construction
free_mb = vram_free_mb                                       // if Some
        | gpu_metrics.(memory_total - memory_used)           // else if Some
present(b) = est_b.is_some()  AND  free_mb known             // ⇒ est_mb > 0 AND free known
n₂(b) = clamp( free_mb / est_mb , 0.0 , 1.0 )  // ≥1.0 free⇒ fits comfortably ⇒ 1.0;
                                               // ramps to 0 as free shrinks below need
```
Rationale for the ratio form (not a hard step): a node with 1.2× headroom should outscore
one with exactly 1.0×; both clamp to `1.0` only once they comfortably fit.

> **Div-by-zero guard (corrected — was a real defect).** `extract_param_billions`
> (`analytics.rs:338`) parses via `parse::<f64>()` then `n as u64` (line 352–353), so it
> **truncates toward zero**: `"phi3:3.8b"` → `3` (confirmed by the existing test at
> `analytics.rs:619`), and a sub-billion tag like `"0.5b"` → `0.5_f64 as u64` → **`0`**. An
> earlier draft wrongly claimed the parser guarantees `b ≥ 1`; it does **not**. Dim 2's
> presence predicate therefore **filters `est_b` to `> 0`** (`.filter(|b| *b > 0)`): when the
> helper returns `Some(0)` or `None`, dim 2 is declared **NOT present → neutral `0.5`**,
> weight-dropped — never `0/0`, never a divide by `est_mb == 0`. Same neutrality mechanism as
> every absent dimension: a model whose size cannot be estimated to ≥1B simply does not
> participate in the fit dimension rather than crashing or scoring wrong.

**Dim 3 — `prompt_size_vs_capacity` (absolute).**
```
present(b) = RouteContext.prompt_tokens.is_some() AND ctx_window(b) known
ratio = prompt_tokens / ctx_window
n₃(b) = clamp( (1.0 - ratio) / 0.5 , 0.0 , 1.0 )
        // = 1.0 when prompt ≤ 50% of window; linear down to 0.0 at 100%; 0.0 past 100%
```
> **Source gap flagged.** There is **no `max_context_len`/ctx-window field on `Backend`
> today** (verified against `config.rs`). Until one is added to the node registry or
> `RouteContext.requested_ctx_len` is populated by the proxy, `ctx_window(b)` is unknown,
> so dim 3 is **never present in Phase 1** and sits neutral regardless of weight. This is
> consistent with the spec's "inert until wired" stance for prompt-size; the *exact* wiring
> requirement is in the Prompt-Size section. Do not assume `Backend` carries context length.

### Group B — GPU pressure (Phase 1) — all absolute-threshold

| # | Dimension | N-type | Source | Normalization → [0,1] | Dir | Phase |
|---|-----------|:------:|--------|------------------------|-----|-------|
| 4 | `gpu_utilization` | A | `GpuMetrics.utilization` (0–100) | see below | lower | 1 |
| 5 | `vram_headroom` | A | `GpuMetrics.memory_used/total` (or Phase-2 `vram_free_mb/vram_total_mb`) | see below | higher | 1 |
| 6 | `gpu_temperature` | A | `GpuMetrics.temperature` (°C) | see below | lower | 1 |

These are absolute by nature — utilization is already a 0–100 percentage, VRAM headroom a
0–1 fraction, temperature a physical °C scale. No peer comparison; each is the backend's
own value against fixed constants.

**Full presence predicates (stated explicitly, not just implied by the formula).** Every
Group-B dimension first requires `gpu_metrics.is_some()`; dim 5 additionally requires a
non-zero VRAM denominator. When a predicate is false the dimension is **NOT present →
neutral `0.5`, weight-dropped** — never `0.0`, never `0/0`:

| Dim | `present(b)` predicate | If false |
|-----|------------------------|----------|
| 4 `gpu_utilization` | `gpu_metrics.is_some()` | neutral 0.5 |
| 5 `vram_headroom` | `(vram_free_mb.is_some() AND vram_total_mb.is_some() AND vram_total_mb > 0)` **OR** `(gpu_metrics.is_some() AND memory_total > 0)` | neutral 0.5 |
| 6 `gpu_temperature` | `gpu_metrics.is_some()` | neutral 0.5 |

```
n₄(b) = clamp( 1.0 - util/100.0 , 0.0 , 1.0 )                 // util∈[0,100]
n₅(b) = clamp( free / total , 0.0 , 1.0 )
        // Phase 2 source: free=vram_free_mb, total=vram_total_mb (both Some, total>0).
        // Phase 1 source: free=memory_total-memory_used, total=memory_total (gpu_metrics, total>0).
        // total == 0 (degenerate telemetry) ⇒ dim NOT present (→0.5), never 0/0.
n₆(b) = clamp( (TEMP_MAX - temp) / (TEMP_MAX - TEMP_MIN) , 0.0 , 1.0 )
        // TEMP_MIN = 40.0, TEMP_MAX = 85.0  (consts; denominator 45.0 ≠ 0 by construction)
```

> **Static / enrolled backends degrade correctly.** A static `[[backends]]` Ollama node
> (or any backend) that never reports `GpuMetrics` has `gpu_metrics == None`, so dims 4–6
> are all **not present → 0.5**, weight-dropped. Via active-weight renormalization (Scoring
> Math) such a node is scored purely on its *present* dimensions (model, priority, …) at
> full strength — it is **never** pushed to `0.0` on GPU axes it cannot report, and never
> sits behind a peer that merely reports a mediocre value. This is the missing-value-
> neutrality contract (constraint 3) applied to the GPU group specifically.

### Group C — Operator intent & affinity (Phase 1)

| # | Dimension | N-type | Source | Normalization → [0,1] | Dir | Phase |
|---|-----------|:------:|--------|------------------------|-----|-------|
| 7 | `operator_priority` | **R** | `Backend.priority` (u32) | fleet-relative, see below | higher | 1 |
| 8 | `tag_affinity` | A | request `tags` ∩ `Backend.tags` | see below | higher | 1 |
| 9 | `backend_type_affinity` | A | `Backend.backend` vs configured preference | see below | higher | 1 |

**Dim 7 — `operator_priority` (THE one fleet-relative dimension).** `priority` is a `u32`
rank whose meaning is purely relative to the other nodes in *this* fleet (`50` is the
`Backend::default`; operators set `100` vs `10` to mean "prefer this over that"). There is
no physical scale, so absolute normalization would be arbitrary. Min/max over the
candidate set:
```
pmin = min priority over candidates,  pmax = max priority over candidates
present(b) = always (priority is non-Option)
n₇(b) = 0.5                                    if pmax == pmin   // n=1 OR all-equal rule
        (priority - pmin) / (pmax - pmin)      otherwise        // ∈ [0,1], exact
```
This is the dimension that makes a backend's score peer-dependent (see Determinism Proof:
acceptable). The `pmax == pmin` all-equal case is the **historical origin** of the general
**call-uniform-drop rule** (Scoring Math, Q6 = (b)): when every candidate has the same
priority (e.g. all default `50`), dim 7 cannot discriminate. The `n₇(b) = 0.5` shown above is
the *normalized value* the dimension would take; the **call-uniform pre-pass then DROPS dim 7
entirely for that call** (it is uniform across all candidates), so it contributes to neither
numerator nor denominator — the same outcome a `0.5`-with-weight-dropped term used to give for
a single candidate, now generalized. (The old "all-equal → 1.0" was wrong — it inflated every
score; the dropped-dimension behavior is the correct neutralization.) For `n=1` (single
candidate) the set is trivially uniform too, so dim 7 is likewise dropped.

**Dim 8 — `tag_affinity` (absolute).** Gate already requires `tags ⊆ b.tags`, so among
candidates every requested tag is matched; this dimension is a soft *bonus* for nodes
carrying **extra** relevant tags only if the request supplies a preference set. In Phase 1,
with the gate enforcing subset, `matched/requested` is uniformly `1.0` for all candidates
when tags are requested → non-discriminating; `0.5` when no tags requested.
```
present(b) = request supplied a non-empty tag set
n₈(b) = matched_tags / requested_tags          // = 1.0 for all gated candidates in P1
        ; 0.5 when no tags requested (not present)
```
(Kept as a catalog entry for forward use; in Phase 1, when tags are requested, it is
**uniformly `1.0` across all gated candidates**. Because it is identical for every candidate,
the **call-uniform pre-pass (Scoring Math, Q6 = (b)) DROPS dim 8 for that call** — it enters
neither numerator nor denominator and cannot skew any candidate's score, exactly like dim 1's
uniform `1.0`. It only begins to discriminate once tags become a *soft* preference that some
candidates satisfy better than others (Q5a) — at which point `matched/requested` would differ
across candidates and the dim would survive the pre-pass. The direction/semantics nuance
(extra-tag bonus vs forward placeholder) is **Q5a** in open-questions.)

**Dim 9 — `backend_type_affinity` (absolute).** Compares `b.config.backend` (the
`BackendType` enum: `Ollama` | `LlamaServer` | `OpenAICompat`) against an optional
configured `prefer_backend_type`. Default weight `0.0` (backend-agnostic principle).
```
present(b) = a preference is configured
n₉(b) = 1.0            if b.config.backend == preferred
        BACKEND_NEUTRAL (= 0.6)   otherwise        // soft nudge, never a gate
        ; 0.5 when no preference configured (not present)
```

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

> **`Some(0)` vs `None` present/absent predicate (Phase-2 note — fix the predicate NOW so
> Phase 2 doesn't reintroduce a double-count).** For the `Option` fields `queue_depth:
> Option<u32>` (dims 10, 12) and `vram_free_mb: Option<u64>` (dim 13), the presence predicate
> is **`field.is_some()`, NOT `field.unwrap_or(0) > 0`**. `Some(0)` is a *real, present*
> signal — an **empty queue** (best possible, `n₁₀ → 1.0`) or **zero free VRAM** (worst,
> `n₁₃ → 0.0`) — and must be **scored**, not treated as absent-neutral-`0.5`. Only `None`
> (the agent never reported the field) maps to **not present → neutral `0.5`, weight-dropped**.
> Concretely: `present₁₀(b) = b.queue_depth.is_some()`; an idle backend reporting `Some(0)`
> must out-rank a busy one reporting `Some(8)`, and must NOT be silently flattened to neutral.
> Conflating `Some(0)` with `None` would (a) discard the strongest "I am idle" signal and
> (b) risk a double-count against dim 4/5's `gpu_metrics`-derived view of the same pressure.
> Phase-2 implementation MUST match on the `Option` itself, never on a `.unwrap_or(0)` proxy.

### Group E — History & stability (Phase 3 — derived/EWMA)

| # | Dimension | Source | Normalization → [0,1] | Direction | Phase |
|---|-----------|--------|------------------------|-----------|-------|
| 14 | `ewma_latency` | `RoutingStats.ewma_latency_ms` per (backend,model) | `clamp(1 - ewma/LAT_REF, 0, 1)` | lower-better | 3 |
| 15 | `recent_error_rate` | `RoutingStats.err_window` (rolling) per (backend,model) | `1 - error_rate` | lower-better | 3 |
| 16 | `recent_success_throughput` | `RoutingStats.ewma_tps` per (backend,model) | `clamp(tps / TPS_REF, 0, 1)` | higher-better | 3 |
| 17 | `flap_stability` | `RoutingStats.health_transitions` (request-count decay) | `1 - clamp(transitions/FLAP_REF, 0, 1)` | lower-better | 3 |

> Phase 3 introduces the **per-(backend,model) `RoutingStats` store** (designed in
> [Phase 3 Metrics Store](#phase-3-metrics-store-item-5)). Determinism is preserved: the
> *scoring path* reads an immutable snapshot of the EWMA values via one `BTreeMap` lookup
> per candidate; updates happen out-of-band on the post-request hook, never during scoring.
> EWMA decay uses **request-count, not wall-clock**, keeping the score path free of time
> reads. Cold-start / `samples < MIN_SAMPLES` ⇒ dimension not present ⇒ neutral `0.5`.

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
  test, tie-break test, 503-on-empty test, **call-uniform-drop test (#12, Q6 = (b))** (all
  below); `cargo build` clean, no `unwrap`/`expect` in `scored.rs`.

### Phase 2 — Live load & latency (dims 10–13)

Flip dims 10–13 from "always absent" to reading their Phase-0 `BackendState` fields.
Add `max_concurrent` to `AgentCapabilities` + agent protocol to light up dim 12. No
scoring-engine change — purely turning on source reads + default weights.

> **Slice 1 (shipped, PR C):** dims 10 (`queue_depth`), 11 (`ttft_p50`), 13
> (`precise_vram_free`) read agent telemetry already at the pool boundary
> (`vram_total_mb`/`vram_free_mb`/`queue_depth`/`ttft_p50_ms` were carried in Phase 0).
> Presence uses the `is_some()` predicate so `Some(0)` is scored (empty queue → 1.0,
> zero free VRAM → 0.0), never conflated with `None`. Dim 13 **supersedes dim 5**
> per-candidate (clears its present flag) to avoid double-counting VRAM pressure.
> Default weights are latency-aware balanced: `ttft_p50=3`, `queue_depth=2`,
> `precise_vram_free=2`. **Slice 2 (deferred):** dim 12 (`concurrency_saturation`)
> needs a `max_concurrent` field on `AgentCapabilities` + the heartbeat protocol +
> the agent daemon — its `BackendState.max_concurrent` stays `None` until then.

### Phase 3 — History & stability (dims 14–17)

Add the per-(backend,model) derived-stats store (EWMA latency, rolling error-rate) updated
post-request above the router and snapshotted into scoring. New store, no engine reshape.
**The storage choice is investigated and recommended in the dedicated section
[Phase 3 Metrics Store](#phase-3-metrics-store-item-5) below** — it is the one Phase-3
decision that can reshape scope, so it is flagged to the Director in open-questions.

### Phase 4 — Locality, cost & capability (dims 18–23)

Policy/affinity layer: session stickiness (agent sessions), locality tiers, power/cost
weights, RPC-shard capability, GPU-class affinity, warm-model recency.

---

## Scoring Math

For a single candidate backend `b` and the active weight set `W` (the configured,
post-default weights for every dimension):

```
Let D       = the fixed-order list of all 23 dimensions.
Let C       = the candidate set (the gate's output; never empty here).
Let present₀(b, i) = true iff dimension i's source datum exists for b in this request.

// ---- Call-uniform pre-pass (Q6 = (b), resolved 2026-06-14) ----
// AFTER each present dimension's per-candidate nᵢ(b) is computed, and BEFORE the
// per-backend active-weight denominator is formed, drop any dimension that takes the
// SAME quantized value for every candidate that has it present — it cannot discriminate.
Let Pᵢ      = { b ∈ C : present₀(b, i) }        // candidates for which dim i is present
uniformᵢ    = (Pᵢ is non-empty) AND
              (∀ b,b' ∈ Pᵢ :  q(nᵢ(b)) == q(nᵢ(b')))   // all equal within the quantum
              where q(x) = (x * SCORE_SCALE).round() as i64,  SCORE_SCALE = 1_000_000
              // SAME quantization the determinism proof / SELECT key already use — no new epsilon.

present(b, i) = present₀(b, i) AND NOT uniformᵢ
              // a call-uniform dim is treated EXACTLY like an absent dim, for EVERY candidate.

normᵢ(b) =
    nᵢ(b)   if present(b, i)         // dimension's [0,1] formula
    0.5     otherwise                // neutral

activeᵢ(b) = wᵢ   if present(b, i) AND wᵢ > 0
             0    otherwise          // weight dropped for absent AND call-uniform dims

denom(b)  = Σ over i of activeᵢ(b)

score(b)  = ( Σ over i of activeᵢ(b) · nᵢ(b) ) / denom(b)      if denom(b) > 0
            0.5                                                  if denom(b) == 0
```

> **The call-uniform pre-pass (Q6 resolved = (b), 2026-06-14).** A dimension that takes the
> **same normalized value for ALL candidates** in a given routing call carries **zero
> discriminating information**, so it is **dropped from scoring for that call** — removed from
> every candidate's present-set *before* per-backend active-weight renormalization — rather
> than scored. This is the principled generalization of the dim-7 (`operator_priority`)
> all-equal → non-participation rule: that rule already drops the one fleet-relative dimension
> when `pmin == pmax`; the pre-pass applies the *same shape* to **every** dimension.
>
> **Exact ordering (deterministic — see Determinism Proof step 3a).** Per call, in the fixed
> canonical dimension order `D` (catalog 1..=23):
> 1. Compute each candidate's `nᵢ(b)` for every dimension `i` that is `present₀` for `b`
>    (the per-dimension `[0,1]` formula, clamped). No peer interaction yet, except dim 7's
>    own `pmin/pmax` which is a set reduction (order-independent).
> 2. For each dimension `i`, form `Pᵢ` (candidates with `i` present₀) and test `uniformᵢ`
>    using the **existing `1e-6` quantization** `q(·)` — *not* a new tolerance. The test is a
>    pure function of the candidate **set** (∀-over-a-set, order-independent).
> 3. For every `uniformᵢ` dimension, set `present(b, i) = false` for **all** `b` (drop it from
>    every candidate's present-set, so it contributes to neither numerator nor denominator for
>    any backend).
> 4. Then form `denom(b)`, the numerator, and `score(b)` over the surviving present dims.
>
> **Interaction with missing-value neutrality.** `Pᵢ` is the set of candidates for which the
> dim is *present₀* (its source datum exists) — a dim absent for some backends is tested for
> uniformity **only among the backends that have it**. If those present-among candidates all
> share one quantized value, the dim is still dropped (it cannot discriminate even among the
> ones that report it). A dim absent for *every* candidate has `Pᵢ = ∅` ⇒ `uniformᵢ = false`
> by the non-empty guard, but it was already not-present for everyone, so nothing changes.
> Net: "absent for a backend" and "present-but-call-uniform" both resolve to *weight-dropped*,
> exactly matching the missing-value-neutrality contract (constraint 3) — a candidate is never
> advantaged or penalized on a dimension that cannot tell it apart from its peers.

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

> **Call-uniform dimensions are DROPPED — the report-nothing vs report-bad-news artifact is
> resolved (Q6 = (b), 2026-06-14).** A dimension that is **present** for a candidate but takes
> the *same* quantized value for *every* candidate that has it present (e.g. dim 1
> `model_resident = 1.0` pre-relax, or `0.0` post-relax; dim 8 `tag_affinity = 1.0` when tags
> are gated) carries no discriminating information. The pre-pass above **drops it from every
> candidate's present-set for that call**, so — unlike an earlier draft that scored it — it
> enters **neither** numerator nor denominator for any backend. This removes the artifact in
> which a telemetry-poor candidate's mean was inflated by the `1.0`-pinned dims dominating its
> small denominator.
>
> **Worked example — the artifact is now resolved.** Default weights; both backends are model
> resident (dim 1 uniform `1.0`), tags requested and gated (dim 8 uniform `1.0`), priority
> equal (dim 7 all-equal ⇒ the *normalized* value would be `0.5`). Under the **old (a)
> status quo**, that uniform `0.5` priority term was still *scored* (present, weight 2), so
> `P` and `R` would have scored:
>
> | Candidate | Old (a)-status-quo present dims (norm·w) | old denom | old score |
> |-----------|------------------------------------------|-----------|-----------|
> | **P** (no GpuMetrics, no VRAM) | `1.0·5` (m_res), `1.0·1` (tag), `0.5·2` (prio) | 8 | `7/8 = 0.875` |
> | **R** (honest-mediocre: util 50%, vram 50%, temp 50%) | `1.0·5`, `1.0·1`, `0.5·2` (prio), `0.5·3` (util), `0.5·2` (vram), `0.5·1` (temp) | 14 | `10/14 ≈ 0.714` |
>
> *(the old (a)-status-quo numbers — `P` (0.875) beats honest `R` (0.714) purely on phantom
> strength.) Now apply the pre-pass. Across the candidate set: **dim 1 is uniform `1.0`**
> (both resident) → dropped;
> **dim 8 is uniform `1.0`** (both gated) → dropped; **dim 7 is uniform** (equal priority,
> `pmin == pmax`) → dropped. Dims 4/5/6 are present **only on `R`** (`P` has no `GpuMetrics`),
> so `Pᵢ = {R}` for each: a single-member present-set is trivially uniform → those dims are
> **also dropped** (a dim only one candidate reports cannot discriminate *between* candidates
> either). After the pre-pass:
>
> | Candidate | Surviving present dims | denom | score |
> |-----------|------------------------|-------|-------|
> | **P** | *(none — all weighted dims dropped or absent)* | 0 | `0.5` (denom == 0 guard) |
> | **R** | *(none — its GPU dims were single-member-uniform, dropped)* | 0 | `0.5` (denom == 0 guard) |
>
> Both land at neutral `0.5` and the decision falls through to **priority (desc) → name (asc)**
> — the no-metrics node `P` no longer *wins* on phantom strength. The general principle holds:
> after the pre-pass, candidates are scored **only on dimensions that genuinely discriminate
> among them**. (Had `R` reported a GPU value that some *other* candidate also reported with a
> different value, those dims would survive and decide the route on real signal — see the
> acceptance test #12 for exactly that case, where a discriminating dim Y picks the winner and
> a uniform dim X is provably dropped.)
>
> **This is now SPECIFIED behavior, not a documented artifact.** It encodes the policy stance
> "a node is neither advantaged for withholding telemetry nor penalized for reporting honest
> bad news" — silence and candor are equalized on any axis that can't tell candidates apart.
> The mechanism is the deterministic pre-pass (Scoring Math, above); see the Determinism Proof
> for the proof that it introduces no float-order or set-iteration nondeterminism, and Q6 in
> `scorer-open-questions.md` for the resolved decision record.

### SELECT — total tie-break ordering

`argmax` is defined by a **total** comparator so two distinct backends can never be
equal-ordered:

```
1. score        — descending   (compared as fixed-scale integers, see below)
2. priority     — descending   (Backend.priority, u32)
3. name         — ascending    (lexicographic, &str)  ← guarantees totality
```

`name` is unique per pool entry **because Phase-1 config validation enforces it** — see
impl-delta item 8: `config.rs::validate()` dedups duplicate static `backend.name` (warn +
keep-first) and rejects the reserved `agent:` / `node:` prefixes in static config, so the
three namespaces (static, `node:` enrolled, `agent:`) are provably disjoint and no two `Vec`
elements ever share a `name`. Given that *enforced* invariant, step 3 always breaks any
remaining tie, and the result is one unambiguous winner for any candidate set. (Without the
enforcement, two distinct elements could share a `name`, `name asc` would return `Equal`, and
`max_by` would become order-dependent — which is exactly why item 8 is a Phase-1 requirement,
not an assumption.)

**Float-comparison determinism — compute in f64, compare as quantized i64.**

> **Decision (item 2): scores are computed in `f64`, then quantized to a fixed-point
> `i64` comparison key.** Not full fixed-point arithmetic. Reasoning below.

Herd's house rule leans to **integer math in engine cores** (game-engine determinism).
That rule exists to kill platform-dependent float rounding in a loop that *accumulates
state over time*. The scorer is not such a core: it is a **pure, single-shot reduction**
over a bounded candidate set, recomputed from scratch each call, with **no state carried
between calls**. The determinism we must guarantee is "same snapshot ⇒ same route on this
machine, every call" — and IEEE-754 `f64` is fully deterministic for a *fixed operation
order on a fixed platform*. So full fixed-point (integer weights, integer divides with
explicit rounding) buys us nothing here except a less-readable scoring kernel and a worse
normalization story (the `[0,1]` norms are naturally fractional). We therefore:

1. **Compute** `nᵢ(b)`, the weighted sum, and `score(b) = Σ/denom` in `f64`.
2. **Quantize the comparison key**, never the stored score:
   `score_q(b) = (score(b) * SCORE_SCALE).round() as i64`, `SCORE_SCALE = 1_000_000`.
3. **Order on `score_q` (i64), not on the f64.** Integer comparison is total and exact —
   it eliminates `partial_cmp`'s `Option` and the `NaN` branch entirely.

This is the targeted application of the integer-math principle: the *comparison key* — the
thing whose ambiguity would actually break determinism — is an exact integer; the
arithmetic that produces it stays in the natural floating domain.

**Quantization tolerance (explicit).** With `SCORE_SCALE = 1_000_000` and `round()`, two
scores map to the **same** `score_q` iff they lie in the same `1e-6`-wide bucket — i.e. they
differ by **less than `0.5e-6`** after rounding to the nearest integer multiple of `1e-6`.
Two such "effectively equal" scores are **deliberately treated as a tie** and fall through
to the exact secondary keys: **priority (desc), then name (asc)**. This is the intended
behavior, not a lossy accident — it means a routing decision is never made on sub-micro
float noise that could flip with a compiler/codegen/LLVM-version change, while genuinely
different scores (Δ ≥ `1e-6`, far coarser than any realistic weighted-mean difference that
matters) still order strictly by score. The guarantee is therefore a **deterministic total
order**, *not* "every distinct f64 score is a distinct rank" — within one quantum, the
stable, meaningful priority/name keys decide.

Contrast with the existing `least_busy_cmp`, which does `partial_cmp(...).unwrap_or(Equal)`
— acceptable there (one axis, ties tolerated) but **not** adopted here: Scored never
constructs an `Ordering::Equal`-on-NaN fallback because it never compares floats directly.

### Determinism Proof (item 2)

**Claim.** For a fixed pool snapshot `S` and a fixed request `R` (model, tags, excluded,
`RouteContext`), `ScoredRouter::route_scored(R)` returns the same `RoutedBackend.name` on
every invocation, on a given build/platform, regardless of the order backends happen to sit
in `S`.

**Proof, stage by stage.**

1. **GATE is a pure filter.** The predicate (healthy ∧ ¬excluded ∧ tags⊆ ∧ model-resident-
   or-relaxed) is a deterministic function of each `BackendState` and `R`. It reads no
   clock, no RNG. Same `S`, same `R` ⇒ identical candidate **set** (set, not order).

2. **Canonical dimension order.** Scoring iterates the dimensions in one fixed, total
   order — the **canonical order `D`** is the catalog numbering 1..=23:
   ```
   1 model_resident, 2 model_fits_vram, 3 prompt_size_vs_capacity,
   4 gpu_utilization, 5 vram_headroom, 6 gpu_temperature,
   7 operator_priority, 8 tag_affinity, 9 backend_type_affinity,
   10 queue_depth, 11 ttft_p50, 12 concurrency_saturation, 13 precise_vram_free,
   14 ewma_latency, 15 recent_error_rate, 16 recent_success_throughput, 17 flap_stability,
   18 session_stickiness, 19 network_locality, 20 power_cost,
   21 rpc_shard_capability, 22 gpu_class_affinity, 23 warm_model_recency
   ```
   `D` is a `const`-ordered structure — an array indexed by a `#[repr(u8)] enum Dimension`
   in catalog order, **never** a `HashMap` (whose iteration order is nondeterministic).
   Per the house rule "`BTreeMap` over `HashMap` where determinism matters," any
   map-shaped weight store is a `BTreeMap<Dimension, f64>`; the reference implementation
   prefers the fixed-size array `[f64; 23]` indexed by `Dimension as usize`.

3. **Call-uniform pre-pass is deterministic.** Between gate and summation, the pre-pass
   (Scoring Math) drops every dimension whose quantized norm is equal across all candidates
   that have it present. This is deterministic for three reasons: (a) it iterates dimensions
   in the same fixed canonical order `D`; (b) the uniformity test is a `∀` over the candidate
   **set** `Pᵢ` — a set predicate, so it is order-independent (just like dim 7's `min`/`max`
   reduction); (c) it compares the **same `1e-6` quantization** `q(x) = (x·SCORE_SCALE).round()
   as i64` the SELECT key uses — **no new epsilon, no raw-`f64` equality**, so no float-order
   or codegen sensitivity enters. The candidate set, the dimension order, and the quantizer are
   all fixed for a given `(S, R)`, so the **set of dropped dimensions is a pure function of
   `(S, R)`** — same snapshot + same request ⇒ same dropped set ⇒ same surviving present-sets.
   Set/`BTreeMap` iteration order never affects *which* dims are dropped (membership in `Pᵢ`
   and the `∀`-equality are order-invariant); it only affects iteration, which is itself fixed
   by `D`.

3a. **Fixed float summation order.** `denom(b)` and the numerator `Σ activeᵢ·nᵢ` are summed
   by iterating the **surviving** dimensions in the canonical order `D` (the pre-pass only
   *removes* dimensions; it never reorders them). Floating-point addition is not associative,
   so summation order matters for the exact bits — fixing the order makes the bits
   reproducible. Same inputs, same surviving set, same order ⇒ same `f64` numerator and
   denominator ⇒ same `score(b)` ⇒ same `score_q(b)`.

   > **Edge case — the pre-pass drops ALL dimensions (no div-by-zero).** If *every* weighted
   > dimension is call-uniform across the candidates (e.g. a fleet of **identical** backends:
   > same models, same priority, same — or uniformly-absent — telemetry), the pre-pass drops
   > all of them, so each candidate's `denom(b) == 0`. This is **not** a divide-by-zero: the
   > existing **`denom == 0 → neutral 0.5`** guard fires for every candidate, they all tie on
   > `score_q`, and SELECT falls through to the exact secondary keys **priority (desc) → name
   > (asc)**. With `name` uniqueness enforced (proof step 5), that yields one unambiguous winner
   > with no panic and no NaN. An all-identical fleet thus degenerates **gracefully** to the
   > priority/name tie-break — the same terminal behavior as if no dimension had any weight.

4. **No fleet-relative cross-talk into determinism.** The one fleet-relative dim
   (`operator_priority`) computes `pmin/pmax` over the candidate **set**, which is order-
   independent (`min`/`max` are commutative). So even the peer-dependent dimension yields
   the same `n₇(b)` for the same candidate set regardless of input ordering.

5. **SELECT is a deterministic total order (not "all scores distinct").** The comparator is
   the lexicographic tuple **(`score_q` desc, `priority` desc, `name` asc)**:
   - `score_q: i64` — total order on integers, no `NaN`, no `Option`. Two scores within one
     `1e-6` quantum (Δ < `0.5e-6` after rounding) **deliberately tie here** and fall through
     to the next key — this is intended, see "Quantization tolerance" above.
   - `priority: u32` — total order.
   - `name: &str` — total lexicographic order, and `name` is the **unique pool key by
     *enforced* invariant** (impl-delta item 8: `validate()` dedups duplicate static names and
     rejects reserved `agent:`/`node:` prefixes, making the three namespaces disjoint), so it
     breaks any residual tie. The proof rests on this enforcement, **not** on an assumption:
     before item 8, a static `[[backends]]` entry named `agent:citadel` could collide with a
     reconciled agent key (two distinct `Vec` elements, same `name`), `name asc` would return
     `Equal`, and `max_by` would become order-dependent. With item 8 enforced, two *distinct*
     candidates can never compare `Equal` on all three keys (they differ in `name`) ⇒ the
     comparator is a **strict total order over the
     candidate set** ⇒ `max_by` (or a sort) yields a **unique** winner, invariant under input
     reordering. The claim is *deterministic total order*, **not** that every distinct f64
     score occupies a distinct rank — near-equal scores collapse to a tie and are resolved by
     the exact priority/name keys, which is exactly what makes the result reproducible across
     codegen changes. ∎

**Where could a `NaN` arise, and why it cannot.** A `NaN` in a score would poison the
comparison. The only source is `0.0/0.0` inside a normalization. Each such site is closed:
- `denom(b) == 0` (all weighted dims absent **or dropped by the call-uniform pre-pass**) →
  guarded: `score = 0.5`, no divide. This single guard also covers the all-identical-fleet
  edge case where the pre-pass drops every dimension (see proof step 3a).
- dim 2 `free/est`: present ⇒ `est_mb > 0`, enforced by the presence predicate
  `extract_param_billions(model).filter(|b| *b > 0)` — the parser **truncates** (`"0.5b"` →
  `0`), so `Some(0)`/`None` ⇒ dim **not present** (→ `0.5`), never `0/0`. (Corrected: the
  parser does **not** guarantee `b ≥ 1`; see dim 2.)
- dim 5 `free/total`: `total == 0` (or no VRAM source) ⇒ dimension declared **not present**
  (→ `0.5`), no divide. See dim 5's full presence predicate.
- dim 6: denominator is the constant `TEMP_MAX - TEMP_MIN = 45.0 ≠ 0`.
- dim 7 `(p-pmin)/(pmax-pmin)`: `pmax == pmin` ⇒ the all-equal rule returns `0.5`, no
  divide.
- dim 3 `prompt/ctx`: present ⇒ `ctx_window > 0` required by `present(b)`.
Every `nᵢ` additionally ends in `clamp(_, 0.0, 1.0)`; `f64::clamp` is `NaN`-propagating, so
clamp is the *backstop*, not the primary guard — the per-site guards above mean no `NaN` is
ever produced in the first place. The quantization `(score*1e6).round() as i64` therefore
operates only on finite `[0,1]` inputs, yielding a finite `i64` in `[0, 1_000_000]`.

### Other determinism invariants the scoring path obeys

- **No RNG** anywhere in gate/score/select.
- **No wall-clock reads.** Scored reads `BackendState` fields only; it never calls
  `Instant::now()`. (Phases 3/4 EWMA decay is request-count-based, computed off-path.)
- **Single snapshot.** One `pool.backends.read().await`, then all gate/score/select work
  on that borrowed slice — no interleaved awaits that could observe a mutated pool
  mid-decision. The borrow is held for the whole decision; no `.await` occurs between the
  read guard acquisition and the returned `RoutedBackend`'s name/url clone.

---

## GATE Stage

The gate hard-eliminates ineligible backends **before** any score is computed. The
predicate is the conjunction (a backend passes only if **all** hold):

| Predicate | Maps to | Source |
|-----------|---------|--------|
| **healthy** | `b.healthy` | circuit breaker (`mark_unhealthy` past `failure_threshold`) |
| **not excluded** | `!excluded.contains(&b.config.name)` | retry-loop exclusion set |
| **tags satisfied** | `tags.iter().all(\|t\| b.config.tags.contains(t))` | request tags |
| **model resident** (when model specified; pre-relax in `relaxed`, always in `strict`) | `b.models.contains(model)` | `BackendState.models` |
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

**Model gate mode (config switch `model_gate`, DEFAULT `relaxed`):**

> **Decision (item 3): `model_gate: relaxed | strict`, default `relaxed`.** Resolved here,
> not deferred — the *default* is the behavior that preserves `model_aware`'s contract
> (never 503 just because a model is loaded nowhere). `strict` is the opt-in escape hatch
> for operators who want a hard "the model must already be resident or fail."

- **`model_gate: relaxed` (DEFAULT)** — the gate first applies the model-resident
  predicate. If that yields ≥1 candidate, score them (dim 1 is uniformly `1.0`, so the
  call-uniform pre-pass **drops it** — placement decided by GPU/priority/etc.). If the
  model-resident gate **empties the set *and* a model was requested**, the gate **relaxes
  exactly once** to the model-unaware candidate set (healthy ∧ tags ∧ ¬excluded). In the
  relaxed set every candidate has the model absent, so `n₁ = 0.0` for all — again uniform, so
  the pre-pass **drops dim 1** and placement falls to the other dimensions exactly as
  `model_aware` falls back to priority today. **No 503** when a model simply isn't loaded
  anywhere; parity with `model_aware`. This is the documented, recommended default.

  > **Why "relax becomes fit-aware only in Phase 2."** In Phase 1, after a relax, every
  > candidate is equally model-absent and `model_fits_vram` (dim 2) is usually absent too
  > (no `vram_free_mb` yet, and `gpu_metrics`-derived free is coarse), so the relaxed
  > decision is effectively "best-placed healthy node" — fine, but it cannot yet prefer the
  > node that *would load the model fastest / has room for it*. Once Phase 2 lights up
  > `vram_free_mb`, dim 2 discriminates among the model-absent candidates ("which node can
  > actually fit and load this model"), making the relaxed path genuinely fit-aware rather
  > than merely load-aware. Phase 1's relax is correct but coarse by design; this is not a
  > bug to fix, it is the staged-capability boundary.

- **`model_gate: strict`** — model-absent backends are gated out and the gate **never
  relaxes**. If no candidate has the model, `route_scored` returns
  `Err("No healthy backends available")` ⇒ the caller 503s. For operators who want a hard
  guarantee that requests only ever hit a node with the model already resident (e.g. to
  avoid cold-load latency spikes entirely). This is a behavior choice, surfaced as a knob,
  not the default.

There is intentionally **no separate `soft` mode** in Phase 1. "Soft model preference"
(model never gated, dim 1 expresses residency as a pure score term) is achievable later by
exposing the gate predicate as configurable, but it is **out of scope** for this sprint —
folding it in now would add a third mode whose only Phase-1 effect duplicates `relaxed`
once the set is non-empty. Flagged in open-questions as a possible future mode, not shipped.

---

## Config Schema (item 6)

The full `routing.scored` block. Lives under the existing `RoutingConfig`
(`config.rs:99`) as `#[serde(default)] pub scored: ScoredConfig`, so omitting it entirely
yields `ScoredConfig::default()` and existing `herd.yaml` files are untouched.

```yaml
routing:
  strategy: scored          # opt-in; anything else ⇒ ScoredRouter is never constructed

  scored:
    # Gate behavior for a requested model. Default `relaxed` (preserves model_aware's
    # "never 503 just because the model is loaded nowhere" contract). See GATE.
    model_gate: relaxed     # relaxed | strict   (default: relaxed)

    # Optional soft preference for a backend type — feeds dim 9 (backend_type_affinity).
    # Unset ⇒ dim 9 not present (neutral) for every backend. Never a gate.
    prefer_backend_type:    # ollama | llama-server | openai-compat   (default: unset)

    # Per-dimension weights. Any omitted key falls to its default below.
    # Absolute scale is irrelevant — only the RATIOS between weights matter
    # (per-backend active-weight renormalization). Weight 0.0 disables a dimension.
    weights:
      # Group A — model & placement (Phase 1, active)
      model_resident:          5.0
      model_fits_vram:         2.0
      prompt_size_vs_capacity: 1.0   # inert until ctx-window source exists (see dim 3)
      # Group B — GPU pressure (Phase 1, active)
      gpu_utilization:         3.0
      vram_headroom:           2.0
      gpu_temperature:         1.0
      # Group C — operator intent & affinity (Phase 1, active)
      operator_priority:       2.0
      tag_affinity:            1.0
      backend_type_affinity:   0.0
      # Group D — live load (Phase 2; key recognized now, source absent ⇒ inert)
      queue_depth:             0.0
      ttft_p50:                0.0
      concurrency_saturation:  0.0
      precise_vram_free:       0.0
      # Groups E/F (Phase 3–4; recognized, inert)
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

**Recommended config struct shape** (`config.rs`):
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoredConfig {
    #[serde(default)]                       // ModelGate::default() == Relaxed
    pub model_gate: ModelGate,
    #[serde(default)]
    pub prefer_backend_type: Option<BackendType>,
    #[serde(default)]
    pub weights: ScoredWeights,             // every field #[serde(default = "...")]
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub enum ModelGate {
    #[default] #[serde(rename = "relaxed")] Relaxed,
    #[serde(rename = "strict")] Strict,
}
```
`ScoredWeights` holds the 23 named `f64` fields, each with a per-field
`#[serde(default = "...")]` returning its defaults-table value, so a partial `weights:`
block overrides only named keys (omitting a key ≠ setting `0.0`). **The field names below
are byte-for-byte identical to the YAML keys and the dimension-catalog names** — a typo in
any of the three would silently drop a weight (the field would deserialize to its default
and the operator's value would be ignored), so they are kept in lockstep:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]                       // any omitted field → its per-field default fn
pub struct ScoredWeights {
    // Group A — model & placement (Phase 1, active)
    #[serde(default = "w_model_resident")]          pub model_resident: f64,          // 5.0
    #[serde(default = "w_model_fits_vram")]         pub model_fits_vram: f64,         // 2.0
    #[serde(default = "w_prompt_size_vs_capacity")] pub prompt_size_vs_capacity: f64, // 1.0
    // Group B — GPU pressure (Phase 1, active)
    #[serde(default = "w_gpu_utilization")]         pub gpu_utilization: f64,         // 3.0
    #[serde(default = "w_vram_headroom")]           pub vram_headroom: f64,           // 2.0
    #[serde(default = "w_gpu_temperature")]         pub gpu_temperature: f64,         // 1.0
    // Group C — operator intent & affinity (Phase 1, active)
    #[serde(default = "w_operator_priority")]       pub operator_priority: f64,       // 2.0
    #[serde(default = "w_tag_affinity")]            pub tag_affinity: f64,            // 1.0
    #[serde(default = "w_zero")]                    pub backend_type_affinity: f64,   // 0.0
    // Group D — live load (Phase 2; recognized, inert in Phase 1)
    #[serde(default = "w_zero")]                    pub queue_depth: f64,             // 0.0
    #[serde(default = "w_zero")]                    pub ttft_p50: f64,                // 0.0
    #[serde(default = "w_zero")]                    pub concurrency_saturation: f64,  // 0.0
    #[serde(default = "w_zero")]                    pub precise_vram_free: f64,       // 0.0
    // Groups E/F (Phase 3–4; recognized, inert in Phase 1)
    #[serde(default = "w_zero")]                    pub ewma_latency: f64,            // 0.0
    #[serde(default = "w_zero")]                    pub recent_error_rate: f64,       // 0.0
    #[serde(default = "w_zero")]                    pub recent_success_throughput: f64, // 0.0
    #[serde(default = "w_zero")]                    pub flap_stability: f64,          // 0.0
    #[serde(default = "w_zero")]                    pub session_stickiness: f64,      // 0.0
    #[serde(default = "w_zero")]                    pub network_locality: f64,        // 0.0
    #[serde(default = "w_zero")]                    pub power_cost: f64,              // 0.0
    #[serde(default = "w_zero")]                    pub rpc_shard_capability: f64,    // 0.0
    #[serde(default = "w_zero")]                    pub gpu_class_affinity: f64,      // 0.0
    #[serde(default = "w_zero")]                    pub warm_model_recency: f64,      // 0.0
}
// e.g. fn w_model_resident() -> f64 { 5.0 }  ... fn w_zero() -> f64 { 0.0 }
```

> **Note — no `deny_unknown_fields`, but the warn still fires.** `ScoredWeights` deliberately
> does **not** set `#[serde(deny_unknown_fields)]`: an unknown key must *warn and be ignored*,
> never fail the load (house rule). With per-field defaults and no `deny_unknown_fields`, serde
> would drop an unknown key *silently* — so to satisfy the contract (and test #9, which asserts
> "load succeeds **with a warning**"), the loader **explicitly** diffs the raw parsed YAML
> mapping keys against the known field-name set and `warn!`s on any extra **before** typed
> deserialization. This key-diff is a committed, required step, not an optional nicety (see
> Validation table, "Unknown weight key").

The scorer materializes `ScoredWeights` into the fixed `[f64; 23]` array indexed by the
`Dimension` enum (same catalog order, same names) once, at router construction — not per
request.

### Defaults table (Phase 1 active dims)

| Dimension | Default weight | Rationale |
|-----------|----------------|-----------|
| `model_resident` | 5.0 | Strongest model signal; carries the gate-relax fallback semantics |
| `gpu_utilization` | 3.0 | Primary live-load axis (matches `least_busy` instinct) |
| `model_fits_vram` | 2.0 | Avoid OOM / CPU-spill routing |
| `vram_headroom` | 2.0 | Headroom for concurrent loads |
| `operator_priority` | 2.0 | Operator intent matters but isn't absolute |
| `gpu_temperature` | 1.0 | Thermal fairness, low weight |
| `tag_affinity` | 1.0 | Already enforced as subset by the tag gate; soft tiebreaker |
| `prompt_size_vs_capacity` | 1.0 | Inert until a ctx-window source exists (see dim 3) |
| `backend_type_affinity` | 0.0 | Off by default — backend-agnostic principle |
| all Phase 2–4 dims | 0.0 | Recognized keys, inert until their phase |

> **These default *values* are a policy choice, not a derivation.** They encode "prefer the
> model-resident, least-loaded node, respecting operator priority." An operator optimizing
> for utilization vs latency vs cost would weight differently. Surfaced to the Director in
> open-questions (the defaults are a sane starting point, overturnable).

### Validation behavior (house rule: never `bail!` on config — warn + degrade)

Per the project rule "**Never bail! on config errors — degrade gracefully, warn+disable
features**," config validation runs at load and **never fails the load**:

| Condition | Behavior |
|-----------|----------|
| **No `scored` block** | `ScoredConfig::default()` — full defaults table. |
| **Partial `weights`** | Each missing key → its default. Omitting ≠ setting `0.0`. |
| **Unknown weight key** | `warn!` + ignore — **the warn is required to actually fire** (test #9 asserts it). Mechanism: the loader parses `routing.scored.weights` first as a `serde_yaml::Mapping`, diffs its keys against the known `ScoredWeights` field-name set (the same 23 catalog names), and `warn!("unknown scored weight key '{k}' — ignored")` for each extra key, **then** deserializes into the typed struct (per-field `#[serde(default)]` drops the unknown). This is a deliberate, committed step — not "if wired"; without it the unknown would be silently dropped and test #9 would be unsatisfiable. (`deny_unknown_fields` is **not** used because it would *fail* the load, violating the house "never bail!" rule — the key-diff gives the warn without the failure.) |
| **Negative weight** (`w < 0.0`) | `warn!("weight {k}={w} < 0, using default")` → reset that key to its default. A negative weight would invert a dimension's contribution — never allowed. |
| **Non-finite weight** (`NaN`/`±Inf`) | `warn!` → reset that key to its default. Protects the f64 sum/quantize path. |
| **All *Phase-1-active* weights `0.0`** (see scope note below) | `warn!("all Phase-1 scored weights are zero, falling back to default weight set")` → use the full defaults table for this load. Prevents a permanent `denom == 0 → 0.5` flat fleet where routing degenerates to the priority/name tiebreak alone. (The per-backend `denom == 0` guard still exists as the runtime backstop, but an all-zero *config* is operator error worth correcting loudly.) |
| **Invalid `model_gate` value** | unknown enum string → serde default `Relaxed` + `warn!`. |
| **Invalid `prefer_backend_type`** | unknown enum string → `None` (no preference) + `warn!`. |

> **Scope of the all-zero rule (important — was ambiguous).** The all-zero fallback checks
> **only the Phase-1-active dimensions** (dims 1–9: `model_resident`, `model_fits_vram`,
> `prompt_size_vs_capacity`, `gpu_utilization`, `vram_headroom`, `gpu_temperature`,
> `operator_priority`, `tag_affinity`, `backend_type_affinity`). Dims 10–23 default to `0.0`
> and are **always source-absent in Phase 1** by design, so they must **never** count toward
> "all weights zero" — if they did, the *default* config (where 10–23 are all `0.0`) would
> spuriously trip the fallback. The trigger is precisely: *every dim in 1–9 has weight `0.0`*
> (after negative/non-finite resets). Note this is a config-weight check, independent of
> per-request presence: a fleet where dims 1–9 all have positive weight but happen to be
> *absent for a given backend* is the normal missing-value case (handled by the per-backend
> `denom == 0 → 0.5` runtime guard), not a config error.

Because weights are normalized per-backend over present dimensions (the missing-value-
neutrality mechanism), **absolute weight scale is irrelevant** — operators write ratios.
`gpu_utilization: 3.0, gpu_temperature: 1.0` means "utilization matters 3× temperature,"
and scaling both by any positive constant changes nothing. This is why the only forbidden
values are negative / non-finite / all-(Phase-1)-zero, never "too large."

- **`strategy` is the only switch.** There is no `scored.enabled` flag — selecting the
  strategy *is* the opt-in, consistent with how the other four strategies work.

---

## Determinism & Testability

Phase-1 test matrix (each a focused unit test in `scored.rs`, using the existing
`BackendPool::new` + `update_*` helpers):

1. **Determinism.** Build a fixed pool snapshot + fixed request; call the scorer N times;
   assert the identical `RoutedBackend.name` every time. Stronger: shuffle the *input
   order* of backends in the pool and assert the winner is invariant (the comparator's
   `name` key makes input order irrelevant).
2. **Gate-before-score.** Backend `X` has the requested model **absent** but is otherwise
   the obvious top scorer (idle, cool, high priority), and at least one model-resident
   candidate exists. Assert `X` is never returned (it was gated, not scored-to-zero) in
   **both** `relaxed` (gate doesn't relax because a resident candidate exists) and
   `strict`. A second assertion: an *unhealthy* top-scorer is also never returned.
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
   excluded), ⇒ `route_scored` returns `Err` ⇒ caller 503s. Plus the `model_gate: strict`
   case: model absent everywhere ⇒ `Err` ⇒ 503 (strict never relaxes). (Mirrors
   `pool_sync`'s `stale_agent_is_drained_…` test shape.)
8. **Relax fallback (default `relaxed`).** Model requested, resident on no backend, healthy
   backends exist ⇒ Scored relaxes the model gate once and returns the best-scored healthy
   backend (does **not** 503) — parity with `model_aware`. Dim 1 is uniformly `0.0` across
   the relaxed set, so the call-uniform pre-pass drops it; placement is decided by the other
   (surviving, discriminating) dimensions.
9. **Config backward-compat.** Parse a `herd.yaml` with `strategy: scored` and **no**
   `scored` block ⇒ defaults table applied. Parse one with a partial `weights` block ⇒
   only the specified keys override; the rest default. Parse one with an unknown weight
   key ⇒ **load succeeds (no panic, no `bail!`), the unknown key is ignored, AND a warn is
   emitted** — the test asserts both the success path *and* that the YAML-mapping key-diff
   fired the warn (assert on captured `tracing` output, or factor the key-diff into a pure
   `fn unknown_weight_keys(&Mapping) -> Vec<String>` returning the extras and assert it
   contains the bogus key). Mechanism and assertion now agree: the warn is produced by the
   explicit pre-deserialization key-diff (see Config Schema note + Validation table), not by
   serde (which would drop it silently).
10. **Phase-2+ inertness.** With Phase-1-only sources populated, assert dims 10–23
    contribute nothing (their `Option` sources are `None`), so routing decisions are
    identical whether their weights are `0.0` or unset.
11. **Name-uniqueness enforcement (determinism precondition — impl-delta item 8).** A
    `herd.yaml` with two `[[backends]]` sharing the same `name` ⇒ after `validate()` the pool
    holds exactly one (first kept, duplicate dropped with a warn). A static `[[backends]]`
    whose `name` begins with `agent:` or `node:` ⇒ dropped with a warn (reserved prefix). This
    is the test that makes the determinism proof's `name`-uniqueness step rest on an enforced
    fact: without it, the SELECT `name asc` tie-break could see two distinct same-named
    candidates and become order-dependent.
12. **Call-uniform dimension drop (Q6 = (b)).** Two parts:
    - **A uniform dim is dropped; a discriminating dim decides.** Build a candidate set where
      dimension **X** is uniform across all candidates (every candidate reports the *same* `nₓ`
      — e.g. all at `gpu_temperature` 60 °C) but dimension **Y** discriminates (different
      `vram_headroom`). Assert (i) the route is decided by **Y alone** — the winner is the
      candidate with the best `n_Y`; and (ii) the result is **identical** to a control run with
      **X's weight set to `0.0`** (i.e. removing X from the weights changes nothing), proving X
      was dropped, not scored. Stronger: assert the per-candidate breakdown lists X as
      **not contributing** (absent/dropped, not a `nₓ·wₓ` term) for every candidate.
    - **All-identical fleet falls through to the tie-break without panic.** Build a fleet of
      backends identical on **every** scored dimension (same models, same priority, same — or
      uniformly-absent — telemetry). Assert (i) no panic / no NaN; (ii) every candidate's
      `denom == 0` so all score neutral `0.5`; (iii) the winner is the lexicographically
      smallest `name` among the highest-priority candidates (the priority→name fall-through of
      proof step 3a), invariant under input reordering.

---

## Auditability

Every routing decision emits one structured `tracing::debug!` line per *winner* and,
when the per-candidate detail is enabled, a breakdown line per candidate. This is the
feature the four legacy strategies cannot offer.

**Winner line (always at debug):**

```
scored route → agent:citadel  score=0.842  (candidates=4, gated_out=2, gate=relaxed, relaxed=false)
```

**Per-candidate breakdown (debug, gated behind a cheap log-level check so it costs
nothing when off):**

```
scored: agent:citadel  score=0.801  denom=6.0
  gpu_utilization=0.78·3.0  vram_headroom=0.71·2.0  gpu_temperature=0.89·1.0
  [dropped (call-uniform): model_resident, operator_priority]
  [absent: model_fits_vram, prompt_size_vs_capacity, tag_affinity, queue_depth, …]
scored: gpu-2          score=0.611  denom=5.0
  gpu_utilization=0.60·3.0  vram_headroom=0.62·2.0
  [dropped (call-uniform): model_resident, operator_priority]
  [absent: gpu_temperature, model_fits_vram, …]
```

Each candidate line lists, in **fixed dimension order**: `dim=norm·weight` for surviving
present dimensions, a bracketed **`[dropped (call-uniform): …]`** list for dimensions the
pre-pass removed because they were equal across all candidates this call (Q6 = (b)), and a
bracketed `[absent: …]` list for the rest — plus the `denom` so an operator can hand-verify
`score = Σ(norm·weight)/denom` over the *surviving* dims only. Surfacing the dropped set is
what makes the Q6 behavior auditable: an operator can see that, e.g., `model_resident` won
nothing because every candidate was resident, and `operator_priority` because all priorities
were equal. (Here both candidates are model-resident ⇒ `model_resident` uniform `1.0` →
dropped; both have equal priority ⇒ `operator_priority` uniform → dropped; no tags requested ⇒
`tag_affinity` absent. The route is decided purely by the GPU dims that actually differ.)
Because order is fixed, the quantizer is fixed, and no clock/RNG is involved, the same
snapshot produces identical audit output — the log *is* a reproducible explanation.

No new endpoint in Phase 1; the breakdown is log-only. (A future `GET
/admin/route-explain?model=…` that returns the breakdown as JSON is a natural Phase-3+
addition but is a non-goal here.)

---

## Phase 3 Metrics Store (item 5)

> **Question.** Dims 14–17 (`ewma_latency`, `recent_error_rate`,
> `recent_success_throughput`, `flap_stability`) need per-(backend, model) latency/TTFT
> history and an error rate, read on the routing hot path. **Does this reuse existing
> storage, or need a new routing-history table?** Investigated against the real code below.

### Constraint that frames the answer

The scoring path runs **per request, under the pool read-lock, with no `.await` between
snapshot and decision** (Determinism Proof). Any store the scorer reads must therefore be:
(a) **cheap** — O(candidates) lookups, no file I/O, no full scans; (b) **snapshot-stable**
— read an immutable view so the decision is deterministic; (c) **decoupled from updates** —
writes happen post-request, off the scoring path. That immediately rules some options out.

### Option (a) — analytics `requests.jsonl` + `get_stats`  → **REJECTED for the hot path**

`analytics.rs` is an **append-only JSONL** file. `get_stats(since_seconds)`
(`analytics.rs:87`) opens the file and **reads every line linearly** (`reader.lines()`),
deserializing each `RequestLog`, aggregating per-backend/per-model durations into in-memory
`HashMap`s, then computing percentiles. This is O(file size) **per call**, holds the
`file_lock` mutex for the duration, and touches the disk. It is correct and fine for the
**dashboard analytics endpoint** (called rarely, by a human). It is categorically wrong for
a routing decision on every inbound request: a fleet doing thousands of req/min would
re-scan a growing multi-MB JSONL thousands of times per minute under a shared lock.
**Reject.** (We *may* still seed/warm an in-memory store from JSONL on startup — see below —
but the scorer never calls `get_stats`.)

### Option (b) — reuse/extend `metrics.rs` `labeled_latency`  → viable, but with sharp edges

`metrics.rs` already keeps `labeled_latency: HashMap<"{backend}|{model}|{status}",
LatencyHistogram>` (`metrics.rs:23`), populated by `record_request_labeled`
(`metrics.rs:213`) on the post-request path (`openai.rs:605`, `server.rs:1597`). Each
`LatencyHistogram` carries `sum` + `count` (→ exact **mean**) and cumulative buckets (→ a
**bucketed p50**). Error rate is derivable: the `status` label splits `success` vs `error`,
so per-(backend,model) error rate = `error_count / (success_count + error_count)` summed
across the two status keys. So *almost* everything dims 14–15 need is already there.

The sharp edges, all verified in code:
- **Volatile.** Pure in-memory; **lost on restart**. Cold-start ⇒ dims 14–17 neutral
  (`0.5`) until traffic re-accumulates. (Acceptable — see recommendation.)
- **Cardinality-capped at 200, silent drop.** `record_request_labeled` does
  `if !map.contains_key(&key) && map.len() >= MAX_LABEL_COMBOS { return; }`
  (`metrics.rs:222`). Past 200 distinct `backend|model|status` combos, **new combos are
  silently dropped** — those backend/model pairs would read neutral forever. A large fleet
  × many models blows this cap. (Escalated to the Director — see open-questions.)
- **No EWMA.** It stores cumulative `sum`/`count` (a lifetime mean), not a *recent* EWMA;
  `tokens_per_second_ema` is **global**, not per-(backend,model). A lifetime mean is the
  wrong signal for "is this node *currently* slow" — a node that was fast for a million
  requests then degraded would barely move. Dims 14/16 specifically want *recency-weighted*.
- **Coupled to the metrics module.** Bending it to also serve routing risks tangling the
  Prometheus-render concerns with hot-path read semantics.

So (b) gives error-rate and a lifetime mean cheaply, but **not** the recency-weighted EWMA
the history dimensions are specified around, and it carries the silent-drop cap.

### Option (c) — NEW dedicated `RoutingStats` store  → **RECOMMENDED (in-memory, v1)**

Introduce a purpose-built store updated on the **same post-request hook** that already feeds
metrics (`record_request_labeled` call sites in `openai.rs` and `server.rs`). Two concrete
shapes, both written out so the Director can pick:

**c1 — in-memory EWMA (RECOMMENDED for Phase-3 v1):**
```rust
// src/router/routing_stats.rs  (new; read by ScoredRouter, written post-request)
pub struct RoutingStats {
    // BTreeMap for deterministic iteration (house rule); key is (backend, model).
    inner: Arc<RwLock<BTreeMap<(String, String), BackendModelStats>>>,
}

#[derive(Clone, Default)]
pub struct BackendModelStats {
    pub ewma_latency_ms: f64,     // EWMA of completion latency,  alpha ~= 0.2
    pub ewma_ttft_ms: f64,        // EWMA of TTFT (Phase 2 telemetry feeds this)
    pub ewma_tps: f64,            // EWMA tokens/sec  (dim 16)
    pub err_window: u32,          // 1-bits = recent errors in a fixed-width ring (dim 15)
    pub samples: u32,             // observation count; gates "warm enough to trust"
    pub health_transitions: u32,  // running flap counter (dim 17)
}
```
- **Update** (off-path, post-request): on each completed request, `update(backend, model,
  outcome)` folds the new latency/TTFT/tps into the EWMAs and shifts the error ring.
  **Decay is request-count-based** (each update is one "tick"), never wall-clock — preserves
  the no-clock-on-the-score-path rule. This hook sits right beside the existing
  `record_request_labeled` calls (`openai.rs:605`, `server.rs:1597`); note those are gated
  behind `if let (Some(tin), Some(tout))`, so a **duration-only** `RoutingStats::update`
  should hang off the unconditional `record_request` (`openai.rs:596`, `server.rs:1582`)
  to also capture token-less and error requests for the error-rate window.
- **Read** (on-path): `ScoredRouter` holds an `Arc<RoutingStats>`; per candidate it does one
  `BTreeMap` lookup keyed by `(backend, requested_model)`. **Cold start / unknown key ⇒
  dimension not present ⇒ neutral `0.5`**, weight-dropped — same neutrality mechanism as
  every other absent dimension. `samples < MIN_SAMPLES` (e.g. 5) ⇒ also treat as not
  present (don't trust a one-sample EWMA).
- **Cold-start does NOT starve a history-less backend (no never-picked loop).** Because an
  unknown/cold key reads **neutral `0.5`** with its weight *dropped from the denominator*, a
  backend with no history is scored purely on its present dimensions (model, GPU, priority)
  exactly like any backend missing telemetry — it is **not** penalized to `0.0` on dims
  14–17. So it stays competitive, gets selected on its other merits, receives requests, and
  *thereby accrues* the history that lets dims 14–17 begin discriminating. There is no
  "never has history ⇒ never picked ⇒ never gets history" deadlock: neutrality breaks it.
- **Cardinality — expected scale + explicit bound policy (was an under-specified risk).**
  This was a stated reason to reject reusing `metrics.rs labeled_latency` (silent 200-combo
  drop), so `RoutingStats` must not silently inherit the same hazard. The key space is
  `(backend, model)`. Concrete scale: a large self-hosted fleet is ≈ **≤100 backends ×
  ≤100 distinct models ≈ 10 000 entries**; each `BackendModelStats` is six small scalars
  (~48 bytes + `BTreeMap` node overhead, call it ~100 B) ⇒ **low-hundreds-of-KB** even at
  10k entries — negligible. **Policy:** Phase-3 v1 sets a **soft ceiling `MAX_ROUTING_STATS
  = 20_000`** (≈2× the realistic max). Behavior at the ceiling is **NOT silent-drop** (the
  metrics-map mistake): instead **evict the least-recently-updated entry** (LRU by an
  internal monotonic update counter — request-count, not wall-clock, to keep determinism)
  before inserting a new one. An evicted-then-re-seen pair simply reads neutral `0.5` until
  it re-warms — the same harmless cold-start path, never a wrong score. If a deployment
  legitimately exceeds 20k live `(backend,model)` pairs, that is a signal to **revisit the
  ceiling (config-promote it)**, surfaced to the Director in open-questions. The ceiling is a
  memory backstop, not an expected operating condition.
- **Volatile** — lost on restart, dims neutral until re-warmed. **This is acceptable**: a
  freshly restarted router routing on Phase-1 placement signals for a few minutes until
  history re-accumulates is fine, and avoids any persistence/migration surface this sprint.

**c2 — persistent SQLite `routing_history` (only if cross-restart memory is required):**
The node registry already runs `rusqlite` with migrations (`src/nodes/`), so a table is
low-friction *infrastructurally* — but it adds write-amplification (a row or upsert per
request) and a load-time warm step, and it is **not needed for correctness**.
```sql
-- migration: routing_history  (ONLY if the Director wants survive-restart history)
CREATE TABLE routing_history (
    backend            TEXT NOT NULL,
    model              TEXT NOT NULL,
    ewma_latency_ms    REAL NOT NULL DEFAULT 0,
    ewma_ttft_ms       REAL NOT NULL DEFAULT 0,
    ewma_tps           REAL NOT NULL DEFAULT 0,
    err_window         INTEGER NOT NULL DEFAULT 0,  -- packed recent-error ring
    samples            INTEGER NOT NULL DEFAULT 0,
    health_transitions INTEGER NOT NULL DEFAULT 0,
    updated_at         INTEGER NOT NULL,            -- unix secs, for staleness eviction only
    PRIMARY KEY (backend, model)
);
```
Even with c2, the **scorer still reads an in-memory snapshot** (c1's struct), hydrated from
the table at startup and written through on update — SQLite is the durability layer behind
the in-memory store, never read on the hot path.

### Recommendation and reshape risk

- **Phase-3 v1 = option (c1): a new in-memory `RoutingStats` (per-(backend,model) EWMA +
  rolling error window), updated on the existing post-request hook, neutral-`0.5` cold
  start, request-count decay, NO new table.** It is the only option that gives the
  *recency-weighted* signal the history dimensions are specified around, stays off the hot
  path for writes, and adds zero persistence/migration surface. Reuse `metrics.rs` only for
  what it already does well (Prometheus export); do not couple routing reads to it.
- **Reshape risk → escalated.** *If the Director wants cross-restart history* (a node's
  reputation surviving a router restart, or a longer-than-in-memory window), that flips us to
  **c2**, which adds a SQLite migration, a per-request write path, and a startup hydration
  step — a real scope increase for Phase 3. This is the one item that can reshape Phase-3
  scope, so the persistence question is surfaced to the Director in
  `scorer-open-questions.md` rather than decided here. The default recommendation (c1) ships
  without it; c2 is purely additive on top of c1's in-memory struct if chosen later.

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
- **Node context-window source for dim 3.** `Backend` has no `max_context_len` field today
  (verified in `config.rs`). Adding one (registry/db or config) is required before
  `prompt_size_vs_capacity` can ever be present; out of scope this sprint.
- **`RoutingStats` SQLite persistence (option c2).** Phase-3 v1 is in-memory only; durable
  cross-restart history is a Director decision (open-questions), additive on top of c1.
