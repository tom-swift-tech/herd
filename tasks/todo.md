# Herd — Working TODO

> Scratchpad for in-flight work. Milestone tracking lives in `ROADMAP.md`;
> the v1.2 PR breakdown + acceptance checklist live in `tasks/HERD-V1.2-SPRINT.md`.
> Full scorer design: `docs/specs/smart-routing-scorer-spec.md`.

**Last updated:** 2026-06-25

---

## ACTIVE — Scorer Phase 4: Locality, cost & capability (dims 18–23)

Phase 4 is the **final 6 dims** (spec dims 18–23 = `Dimension` enum slots 17–22, all already
declared, weight-zeroed via `w_zero()`, and `present0=false`). The scorer engine does NOT
change — each dim needs a **data source + a `compute_raw` branch + a weight default**. Plan
only; no code yet (Director decision 2026-06-25).

### Seam audit (verified against current `main`, post-PR #28)

The plumbing is locked in and proven by Phases 1–3:
- `Dimension` enum + `ALL_DIMS` + `weights_array` carry all 23 dims (`scored.rs:46–195`).
- `compute_raw(b, model, tags, relaxed, prefer_type, pmin, pmax, prompt_tokens, stats)`
  sets `present0[i]` per candidate; dims 18–23 currently force `false` (`scored.rs:~434`).
- Injected state pattern: `ScoredRouter` holds an `Arc`, snapshots **once per call** off the
  scoring path (`routing_stats.snapshot_all()` at `scored.rs:519`; pool snapshot at `:466`).
  This is the template for any new stateful source.
- `ScoredWeights` already has all 6 Phase-4 fields, each `#[serde(default = "w_zero")]`
  (`config.rs:283–293`). `RouteContext` (`router/mod.rs:21–26`) carries `prompt_tokens` +
  `requested_ctx_len` and is the seam for any new request-side signal.

Per-dim source reality (the part the spec assumed but didn't verify):

| dim | name | source today | verdict |
|----|------|--------------|---------|
| 18 | `session_stickiness` | **none on scoring path** — proxy never reads a session id; agent `executor.rs:160` uses legacy `route()` not `route_scored()` and never writes the chosen backend back to `Session` (no `last_backend` field) | **architectural** |
| 19 | `network_locality` | net-new `Backend` config field (tier) | **config-only ✅** |
| 20 | `power_cost` | net-new `Backend` config field (cost weight) | **config-only ✅** |
| 21 | `rpc_shard_capability` | `AgentCapabilities.rpc_capable` exists, but "**request needs sharding**" signal does NOT — dim is always `0.5` → Q6-dropped every call | **policy-blocked → defer** |
| 22 | `gpu_class_affinity` | hardware side exists (`gpu_model`/`gpu_vendor` in `NodeRegistration`/SQLite; `gpu_model` in `AgentCapabilities`) but **no model→preferred-class mapping** and `BackendState` carries no class string on the scoring path | **needs design** |
| 23 | `warm_model_recency` | `ModelWarmer` records **no timestamps** (just logs); `BackendState` has `last_check`/`last_request` `Instant`s but no per-model warm time | **stateful, self-contained ✅** |

### Recommended slice order (cheapest/lowest-risk first)

**Slice A (PR #29) — dims 19 + 20, config-only.** The clean dim-3-shaped slice: no protocol
field, no stateful plumbing, no agent change.
- `Backend` gains `locality: Option<LocalityTier>` (enum: `local`/`lan`/`tailnet`/`wan`,
  `#[serde(default)]`) and `power_cost: Option<f64>` (e.g. watts or $/1k-tok).
- `compute_raw` dim 19: tier map `local 1.0 / lan 0.8 / tailnet 0.6 / wan 0.4`, present iff
  `locality.is_some()`. dim 20: `clamp(1 - cost/COST_REF, 0, 1)`, present iff
  `power_cost.is_some() && COST_REF > 0`.
- **Weight default decision (open):** keep `w_zero()` (opt-in policy) — locality/cost are
  deployment value-judgments, not universally-good defaults. Document, don't flip.
- Tests: presence gating, tier map exactness, cost clamp, Q6-drop when all candidates same tier.

**Slice B (PR #30) — dim 23 `warm_model_recency`, one stateful source.**
- Add per-(backend, model) last-served time. Two options (Q-B1): (a) `BTreeMap<String,Instant>`
  on `BackendState` — reuses the pool snapshot already taken at `:466`, no new lock; (b) a
  separate `ModelWarmStore` `Arc` like `RoutingStats`. **Recommend (a)** — no extra snapshot,
  warm-state is backend-local.
- Writers: `ModelWarmer` on successful warm **and** the serve path (a model loaded by serving
  is warm too, not just `hot_models`) — record `Instant::now()` keyed by served model.
- Norm: `clamp(1 - age_secs/WARM_REF, 0, 1)`; `0.5` when model never seen (spec: neutral).
  Open (Q-B2): spec says `turns_since/WARM_REF` (turn count) — recommend **time-based**
  (`age_secs`) since there's no global turn counter; `WARM_REF` ~ a few minutes.

**Slice C (PR #31) — dim 18 `session_stickiness`, architectural.** Largest blast radius; do
after A/B prove the Phase-4 wiring.
- Needs three things that don't exist: (1) a session→backend ledger (extend `Session` with
  `last_backend: Option<String>`, or a side map), (2) a session key on the scoring path
  (`RouteContext.session_id` + proxy/agent extraction), (3) route agent turns through
  `route_scored()` and write the chosen backend back post-request (piggyback the existing
  post-request hooks at `openai.rs:601/670`, `server.rs:1618`).
- Open (Q-C1): does this apply to plain `/v1/chat/completions` (no session) at all, or only
  agent sessions? If agent-only, the proxy path stays untouched and scope shrinks a lot.

**Defer — dim 21 `rpc_shard_capability` → backlog / v1.3.** Inert until a "request needs
sharding" signal exists, which itself depends on the roadmap llama.cpp RPC tensor-sharding
integration. Building it now adds a field that Q6 drops on every call. Park with rationale.

**Needs-design — dim 22 `gpu_class_affinity` (PR #32, after a decision).** Blocked on Q-D1:
where does "model M's preferred GPU class" come from? Options: a config `model_gpu_class` map,
a request tag, or inferred from model size. Also needs the candidate's GPU class on the scoring
path (`BackendState` doesn't carry `gpu_model` today — would thread it from `AgentCapabilities`
via pool_sync, the dim-12 pattern). Decide source before building.

### Director decisions (locked 2026-06-25)

- **Reach:** *all dims except the blocked 21.* Build order **A (19+20) → B (23) → C (18) →
  D (22)**. dim 21 deferred to v1.3 (RPC integration). Target: **22/23 dims live.**
- **Q-A1 (weights):** *all Phase-4 dims stay opt-in `w_zero()`* — incl. dim 23. Operators
  turn on what fits their fleet; no non-zero defaults this milestone.

Still open (resolve at each slice's architect pass, not blocking Slice A):
- **Q-B1:** warm-time on `BackendState` (recommended) vs separate store. *(Slice B)*
- **Q-B2:** time-based recency (recommended) vs spec's turn-count. *(Slice B)*
- **Q-C1:** dim 18 agent-sessions-only (smaller) vs all proxy traffic. *(Slice C)*
- **Q-D1:** source of "model→preferred GPU class" for dim 22. *(blocks Slice D)*

### Status
- [x] SEAM AUDIT — 3 parallel explores (scorer plumbing / session model / warmer+capability),
      cross-checked against spec dims 18–23 formulas. Findings above.
- [x] DIRECTOR — reach + weights locked (above). Build order A→B→C→D, 21 deferred.
- [x] SLICE A (dims 19+20) — branch `feat/v1.2-scorer-phase4-slice-a-dims-19-20`.
      `LocalityTier` enum + `Backend.locality`/`Backend.power_cost` (both serde-default,
      wire-compat); `COST_REF=100`; dim 19 tier map + dim 20 `clamp(1-cost/COST_REF)` in
      `compute_raw`; both opt-in (`w_zero`). 4 tests (2 direct `compute_raw` formula/absence,
      2 anti-trivial e2e). Build ✓, clippy `-D warnings` ✓, fmt ✓, **lib 540→544**. Docs:
      herd.yaml example (weights + per-backend fields), spec Slice-A note. No lib unwrap.
      **Awaiting: commit + push + PR (user confirm).**
- [x] SLICE B (dim 23 `warm_model_recency`) — built on the locked decisions:
      Q-B1 = `last_served: BTreeMap<String, Instant>` on `BackendState` (read from existing
      pool snapshot, no new lock); Q-B2 = time-based `n = clamp(1 - age_secs/WARM_REF, 0, 1)`,
      `WARM_REF=300`, `0.5` when requested model never served here, absent when no model
      requested; Q-B3 = stamp on BOTH `ModelWarmer` success AND every successful served
      request via new `BackendPool::record_served` (3 hooks: openai.rs streaming+non-streaming,
      server.rs proxy; serve-path writer also covers llama-server nodes the warmer skips).
      `compute_raw` gained a `now: Instant` arg (captured once per call → fair + deterministic;
      dim 23 stays weight-0 opt-in so default determinism tests unaffected). 3 tests (dim-23
      formula/absence direct, e2e recent-backend-wins, pool `record_served`). Build ✓, clippy
      `-D warnings` ✓, fmt ✓, **lib 544→547**, no lib unwrap. Docs: herd.yaml.example weight +
      spec Slice-B note. **Awaiting: commit (user said local-only for the branch).**
- [x] SLICE B follow-up — `last_served` LRU cap (`MAX_LAST_SERVED=256`), commit `56c1544`.
- [x] SLICE C (dim 18 `session_stickiness`) — Q-C1 = **all proxy traffic**. New
      `router/session_affinity.rs` (`SessionAffinity`: session_id→backend, LRU 50k, mirrors
      `RoutingStats`), injected into `ScoredRouter` + `create_router` (all call sites incl. 2
      integration tests). `RouteContext.session_id` ← `X-Herd-Session` header in both proxy
      paths; write-back on all 3 post-request hooks. `compute_raw` gains `sticky_backend:
      Option<&str>` (resolved once/call); dim 18 = 1.0 match / 0.5 else, absent when no prior
      backend. Opt-in. 5 tests (3 store, 2 scored). Build ✓, clippy `-D warnings` ✓, fmt ✓,
      **lib 548→553**, integration green. Docs: skills.md X-Herd-Session, header JSON,
      herd.yaml.example weight, spec Slice-C note.
- [ ] SLICE D (dim 22 `gpu_class_affinity`) — Q-D1 = **infer from model size**. Parse param
      count from model name → class tier; thread candidate gpu_model to BackendState; norm
      1.0 exact / 0.7 same-vendor / 0.5 unknown. **NEXT.**

---

## DONE — audit log (collapsed; full detail in git history + ROADMAP)

- **Scorer Phase 3 — history/EWMA dims 14–17** (PR #28, `2026-06-24`, merged `95123a6`) —
  new `src/router/routing_stats.rs`: per-(backend,model) request-count EWMA (`alpha=0.2`),
  32-bit rolling error ring, LRU eviction at 20k entries. `ScoredRouter` holds
  `Arc<RoutingStats>`, `snapshot_all()` once per call; dims 14–17 absent for cold/
  under-sampled backends (`< MIN_SAMPLES=5`). Phase-3 weights flipped off 0:
  `error_rate=3`/`latency=3`/`throughput=2`/`flap_stability=1`. Post-request hooks in
  `server.rs`/`openai.rs` call `routing_stats.update()`. 14 new tests; lib 526→540.

- **dim-3 agent ctx source** (commit `147bb3f`) — agent reports llama-server `/props` `n_ctx`
  so dim 3 (`prompt_size_vs_capacity`) covers agent nodes, not just static `max_context_len`.

- **Scorer Phase 2 — prompt_size_vs_capacity (dim 3)** (PR #25, `2026-06-19`) — `Backend.
  max_context_len: Option<u32>` (static backends); `estimate_prompt_tokens` ~4 chars/token over
  chat messages; both proxy sites switched `route_excluding` → `route_scored(RouteContext)`;
  dim 3 `n = clamp((1 - prompt/window)/0.5, 0, 1)`, present iff both sides known. lib 512→518.

- **Scorer Phase 2 Slice 2 — measure real load + dim 12** (PR #24, `2026-06-19`, merged
  `53f86b7`) — `herd agent` probes llama-server `/props` (`total_slots`→`max_concurrent`) +
  `/slots` (busy→`queue_depth`), best-effort. `AgentCapabilities.queue_depth` → `Option<u32>`
  (honest unmeasured) + new `max_concurrent` (both serde-default). dim 12
  `concurrency_saturation = 1-queue_depth/max_concurrent`, guard `max>0`, coexists with dim 10,
  weight 1.0. lib 504→512. **Deferred:** agent ttft (dim 11) — `/metrics` has no quantiles.

- **Scorer Phase 2 Slice 1 — live-load dims 10/11/13** (PR #23, `2026-06-19`, merged
  `90b5251`) — read agent telemetry at the pool boundary. `is_some()` presence (`Some(0)`
  scored, not absent); dim 13 supersedes dim 5 per-candidate (no VRAM double-count); weights
  `ttft_p50=3`/`queue_depth=2`/`precise_vram_free=2`. `QUEUE_REF=8`/`TTFT_REF=2000`. lib
  499→504. **Test gotcha:** the Q6 call-uniform pre-pass drops any dim present on only ONE
  candidate — live-dim tests need ≥2 reporters; supersession tested directly on `compute_raw`.

- **Scorer Phase 1 hardening** (PR #21, `2026-06-15`) — dim-6 temp sentinel guard,
  per-candidate alloc drop, VRAM-comment fix. lib 499.
- **Scorer Phase 1 — `ScoredRouter`** (PR #19) — GATE→SCORE→SELECT over dims 1–9,
  per-backend active-weight renorm, Q6 call-uniform-drop, i64-quantized total
  tie-break, `routing.scored` config + weight sanitize. Dims 10–23 recognized/inert.
- **Scorer Phase 0 — telemetry to pool boundary** (PR #17) — `BackendState` gains
  `queue_depth`/`ttft_p50_ms`/`vram_free_mb`/`max_concurrent` `Option`s; `pool_sync`
  populates them for `agent:` entries.
- **Containerized-gateway persistence** (PR #18, `2026-06-14`) — all 6 stores root
  under `HERD_DATA_DIR`; default byte-identical when unset; Dockerfile volume.
- **v1.2 fleet foundation** (PRs #1–#8) — `herd agent` daemon, `NodeRegistry` (TTL
  evict), heartbeat endpoint, agent persistence, gateway version authority +
  sha256 self-update (#6a/#6b), `herd publish` (#6c), BackendPool routing (#7),
  in-process integration test (#8). Two-box cross-machine remains manual acceptance.

---

## Backlog
- **dim 21 `rpc_shard_capability`** — deferred from Phase 4: inert until a "request needs
  sharding" signal exists, which depends on llama.cpp RPC tensor-sharding integration (v1.3
  roadmap). Building now = a Q6-dropped no-op field. Revisit with the RPC work.
- **Agent ttft measurement (dim 11)** — agent reports `ttft_p50_ms: None`; a TRUE p50 isn't
  reachable for an observe-only agent (`/metrics` counters/gauges, no quantiles; per-request
  `timings.prompt_ms` only on completion responses the agent doesn't see). Realistic option:
  interval-MEAN TTFT from `/metrics` counter deltas (needs `--metrics` + daemon delta-state),
  reported as an approximation. Deferred pending a decision on the mean approximation.
- **Scorer Phase 4 leftovers** — dim 18 (session stickiness) and dim 22 (gpu-class affinity)
  may land or defer per Director decisions Q-C1/Q-D1 above.
- v1.3 milestone — speculative decoding, full mDNS discovery, routing-strategy plugins,
  llama.cpp RPC tensor-parallel sharding.
