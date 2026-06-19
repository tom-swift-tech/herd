# Herd — Working TODO

> Scratchpad for in-flight work. Milestone tracking lives in `ROADMAP.md`;
> the v1.2 PR breakdown + acceptance checklist live in `tasks/HERD-V1.2-SPRINT.md`.
> Full scorer design: `docs/specs/smart-routing-scorer-spec.md`.

**Last updated:** 2026-06-19

---

## ACTIVE — Scorer Phase 2, Slice 1: live-load telemetry dims (10/11/13)

Branch off `main` (`8fa849b`). v1.2.0 is shipped; scorer Phases 0–1 landed
(PRs #17, #19, hardened in #21). This slice flips three Phase-2 dims from
"always absent" to reading their Phase-0 `BackendState` fields. **No agent
protocol change** — the agent already reports queue_depth / ttft_p50_ms /
vram_total_mb / vram_free_mb in `AgentCapabilities`.

**Scope decisions (Director, 2026-06-19):**
- **Slice = dims 10/11/13 ONLY.** Dim 12 (`concurrency_saturation`) needs a new
  `max_concurrent` on `AgentCapabilities` + heartbeat protocol + agent daemon
  (cross-machine, version-compat) → **deferred to Slice 2**. Deferred dim-3
  (`prompt_size_vs_capacity`, proxy token estimation) → **separate PR**.
- **Weight posture = latency-aware balanced** (open-question Q2): give the live
  dims real influence without dominating placement. Defaults below.

**Invariants the spec already resolved (honor, don't re-decide):**
- **`Some(0)` is a REAL signal, not absent.** Presence predicate is
  `field.is_some()`, NEVER `unwrap_or(0) > 0`. An empty queue (`Some(0)` →
  n=1.0, best) must out-rank a full one (`Some(8)` → n=0.0); zero free VRAM
  (`Some(0)` → n=0.0, worst) must be scored, not flattened to neutral 0.5.
  Only `None` (agent never reported) → not-present → neutral 0.5, weight-dropped.
- **Dim 13 supersedes dim 5** when present (spec §Group D). Both measure free-VRAM
  pressure; scoring both double-counts it. When dim 13 is present for a candidate,
  mark dim 5 NOT present for that candidate (drops its weight from the denominator).

### Plan (~10 lines)
1. **`pool.rs`** — add `vram_total_mb: Option<u64>` to `BackendState` (init `None`
   in `new()`/`add()`/test helpers, mirroring the Phase-0 four fields). Extend the
   Phase-0 `set_agent_telemetry` setter to carry it. Static/enrolled stay `None`.
2. **`pool_sync.rs::reconcile`** — populate `vram_total_mb = Some(caps.vram_total_mb)`
   for `agent:` entries on BOTH the add and update branches (alongside the existing
   `vram_free_mb`). No other entry type touched.
3. **`scored.rs` dim 10** (`queue_depth`): present iff `b.queue_depth.is_some()`;
   `norm = clamp(1 - depth/QUEUE_REF, 0, 1)`, `QUEUE_REF = 8`.
4. **`scored.rs` dim 11** (`ttft_p50`): present iff `b.ttft_p50_ms.is_some()`;
   `norm = clamp(1 - ttft/TTFT_REF, 0, 1)`, `TTFT_REF = 2000` (ms).
5. **`scored.rs` dim 13** (`precise_vram_free`): present iff `vram_free_mb.is_some()`
   AND `vram_total_mb` is `Some(t)` with `t > 0`; `norm = clamp(free/total, 0, 1)`.
   **On present → set dim 5 present0 = false** (supersession, step above).
6. **`config.rs` default weights** — flip from `w_zero()`: `ttft_p50 = 3.0`,
   `queue_depth = 2.0`, `precise_vram_free = 2.0`. `concurrency_saturation` STAYS
   `w_zero()` (dim 12 deferred). Update the two default-weight asserts
   (config.rs:~2096, ~2158) to the new values.
7. **Tests** (anti-trivial — each must flip a winner if its logic is removed):
   - dim 10: idle `Some(0)` out-ranks busy `Some(8)`; `None` (static) stays neutral.
   - dim 11: fast ttft out-ranks slow; verify removing the dim flips the winner.
   - dim 13: `Some(0)` free-VRAM scored as worst (not neutral); supersession —
     a backend with dim 13 present must NOT also count dim 5 (assert denom/breakdown).
   - backward-compat: existing scored configs still parse; new defaults applied.
   - determinism: run-twice + shuffle-invariant still hold with the new dims live.
8. **Gates:** `cargo build` + `cargo test` (report lib subtotal vs total separately —
   they differ, not a drop; current baseline lib 499) + `clippy --all-targets -- -D warnings`
   + `fmt --check`. NO `unwrap`/`expect` in `scored.rs`/`pool.rs`/`pool_sync.rs` lib code.

### Roles
ARCHITECT: confirm the `vram_total_mb` threading + dim-5/13 supersession mechanism
(does dropping dim 5's present0 correctly remove it from the per-backend denominator,
and does a static backend with neither still neutralize cleanly?). BUILDER: steps 1–7.
REVIEWER (independent, blocking hunt list): (1) `.is_some()` predicate everywhere —
no `unwrap_or(0)` proxy; `Some(0)` scored; (2) supersession is per-candidate, not
global; (3) static/enrolled untouched (`None` → neutral, not penalized); (4)
determinism intact; (5) no lib unwrap. OPERATOR: gates, separate lib/total counts.
LEAD: docs (ROADMAP Phase-2 status, spec §Phase 2 mark dims 10/11/13 live) + PR.
**STOP at open PR — no auto-merge; independent outside review before merge.**

### Rules
conventional commits; commit trailer
`Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`.

### Status
- [x] STOP for go-ahead → approved (2026-06-19)
- [x] ARCHITECT confirm — supersession mechanism verified: clearing `present0[VramHeadroom]`
  in `compute_raw` drops dim 5 from the per-backend score denominator (score loop only sums
  dims where `w>0 && present0[i] && !uniform[i]`); per-candidate by construction. **Scope
  shrank:** `vram_total_mb` was ALREADY a `BackendState` field + populated by `pool_sync`
  (Phase 0 carried more than the memo recorded) → NO pool.rs/pool_sync.rs change needed.
  Slice 1 = pure `scored.rs` + `config.rs`.
- [x] BUILDER — `scored.rs`: dims 10/11/13 read telemetry (`is_some()` predicate; `Some(0)`
  scored), dim 13 supersedes dim 5 per-candidate; `QUEUE_REF=8`/`TTFT_REF=2000`. `config.rs`:
  defaults `queue_depth=2`/`ttft_p50=3`/`precise_vram_free=2`; `sanitize_weights` reset values
  matched. 5 new tests (anti-trivial vs Q6 single-reporter drop; supersession tested directly
  on `compute_raw`).
- [x] OPERATOR gates — build ✓, clippy `-D warnings` ✓, fmt ✓; lib 499→**504** (+5). No lib unwrap.
- [x] REVIEWER independent hunt list — **CLEAN, no blocking findings** on all 6 items
  (Some(0) predicate, per-candidate supersession, determinism, no lib unwrap, weight
  agreement across serde/Default/sanitize, anti-trivial tests vs Q6 single-reporter drop).
- [x] LEAD docs (ROADMAP PR-C status, spec §Phase 2 Slice 1 note) + commit + PR.
- [ ] INDEPENDENT outside review before merge

---

## DONE — audit log (collapsed; full detail in git history + ROADMAP)

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
- **Scorer Phase 2 Slice 2** — dim 12 `concurrency_saturation`: add `max_concurrent`
  to `AgentCapabilities` + heartbeat + agent daemon; populate `BackendState.max_concurrent`
  in `pool_sync`; flip dim 12 in `scored.rs`. Version-compat: older agents omit it → `None`.
- **Deferred dim-3** (`prompt_size_vs_capacity`) — new `Backend.max_context_len` config +
  proxy token estimation; switch proxy (`api/openai.rs:359`, `server.rs:1437`) from
  `route_excluding` to `route_scored` populating `RouteContext.prompt_tokens`.
- **Scorer Phase 3** — per-(backend,model) `RoutingStats` (EWMA latency, error-rate).
  Open-question Q1: in-memory (recommended) vs SQLite persistence — decide before building.
- **Scorer Phase 4** — locality/cost/capability (dims 18–23).
- v1.3 milestone — speculative decoding, full mDNS discovery, routing-strategy plugins.
