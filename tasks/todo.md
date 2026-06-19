# Herd — Working TODO

> Scratchpad for in-flight work. Milestone tracking lives in `ROADMAP.md`;
> the v1.2 PR breakdown + acceptance checklist live in `tasks/HERD-V1.2-SPRINT.md`.
> Full scorer design: `docs/specs/smart-routing-scorer-spec.md`.

**Last updated:** 2026-06-19

---

## ACTIVE — Scorer Phase 2, Slice 2: measure real load + dim 12 (concurrency_saturation)

Branch `feat/v1.2-scorer-pr-d-slice2` off `main` (post-#23). Carries the todo cleanup.

**The problem Slice 1 left:** the agent's `snapshot()` HARDCODES `queue_depth: 0`,
`ttft_p50_ms: None` (`capabilities.rs:237`). The "live-load" dims consume placeholders —
every agent reads as perpetually idle. Slice 2 makes the agent **measure real load**.

**Scope decisions (Director, 2026-06-19):**
- **Measure real load** (not the thin field-only path). Agent probes llama-server.
- **Dims 10 & 12 coexist**, dim 12 at lower weight (no supersession). Both derive from
  queue_depth (absolute vs capacity-relative); per-backend renorm tolerates correlation.
- **ttft (dim 11) deferred** — needs the `--metrics` flag + p50 windowing; later micro-slice.

**llama-server API (verified vs ggml-org/llama.cpp docs):**
- `GET /props` → `total_slots` (int) = `max_concurrent`. **Read-only, no flag needed.**
- `GET /slots` → JSON array; count `is_processing==true` = real queue depth. Enabled by
  default (disabled via `--no-slots` → degrade to None).
- Ollama / openai-compat / probe failure → None (honest "unmeasured", not fake-0).

**Honesty change (in scope):** `AgentCapabilities.queue_depth: u32 → Option<u32>` so an
unmeasurable backend reports `None` (dim 10 absent) instead of `Some(0)` (fake idle that
would unfairly win queue routing). `#[serde(default)]` keeps wire-compat (old agents send
`0` → `Some(0)`, same as before).

### Plan
1. **`lifecycle.rs`** — extend `ProbeOutcome` with `queue_depth: Option<u32>` +
   `max_concurrent: Option<u32>`. Add pure parse fns (canned-output tested, matching the
   file's style): `parse_llama_total_slots(body)->Option<u32>` (`/props`.`total_slots`),
   `parse_llama_busy_slots(body)->Option<u32>` (`/slots` array, count `is_processing`).
   For LlamaServer (and auto-detected openai path), best-effort GET `/props` + `/slots`;
   any failure → None, never flips `reachable`. Ollama → both None.
2. **`capabilities.rs`** — `snapshot()` takes `queue_depth: Option<u32>` +
   `max_concurrent: Option<u32>` (no more hardcoded 0). `AgentCapabilities.queue_depth`
   → `Option<u32>`; add `max_concurrent: Option<u32>` (`#[serde(default)]`).
3. **`mod.rs` run loop** — thread `local.queue_depth` / `local.max_concurrent` into snapshot.
4. **`registry.rs`** — `queue_depth: Option<u32>`, add `max_concurrent: Option<u32>`
   (`#[serde(default)]`). Fix the ~10 literal sites (mostly test `queue_depth: 0` →
   `Some(0)`; client.rs/internal.rs/health.rs/db.rs/fleet_routing.rs).
5. **`pool.rs`** — `set_agent_telemetry` signature: `queue_depth: Option<u32>` +
   add `max_concurrent: Option<u32>`; assign straight through (no `Some()` wrap).
6. **`pool_sync.rs`** — `st.queue_depth = caps.queue_depth` (now Option),
   `st.max_concurrent = caps.max_concurrent`, on BOTH add & update branches.
7. **`scored.rs` dim 12** (`ConcurrencySaturation`, idx 11): present iff
   `max_concurrent.is_some() && >0` AND `queue_depth.is_some()`;
   `norm = clamp(1 - depth/max_concurrent, 0, 1)`. NO supersession (coexists with dim 10).
8. **`config.rs`** — `w_concurrency_saturation()=1.0` (lower than dim 10's 2.0); flip serde
   default + `Default` impl + `sanitize_weights` reset value (scored.rs).
9. **Tests:** lifecycle parse fns (canned `/props`+`/slots`); snapshot carries the values;
   pool_sync agent entry carries max_concurrent + real queue_depth, static stays None; dim
   12 saturation differentiates + coexists with dim 10 + div-by-zero guard (`Some(0)`
   max_concurrent → absent); config default weight; backward-compat (old agent `queue_depth:0`
   → Some(0)). Anti-trivial vs Q6 single-reporter drop (≥2 reporters per live-dim test).
10. **Gates:** build + test (lib count grown from 504) + clippy `-D warnings` + fmt. No lib unwrap.

### Roles
ARCHITECT (done, this session): API verified vs docs; queue_depth→Option ripple mapped
(~12 sites); /props for max_concurrent (no flag), /slots for depth (default-on). BUILDER:
steps 1–9. REVIEWER (independent, blocking): (1) probe failure → None, never flips
`reachable` or breaks the beat; (2) queue_depth Option honest — Ollama/unmeasured → None,
not Some(0); (3) wire-compat — old agent JSON still deserializes; (4) dim 12 div-by-zero
guard (`max_concurrent` Some(0)/None → absent); (5) determinism; (6) no lib unwrap; (7)
tests anti-trivial vs Q6. OPERATOR: gates, lib/total counts. LEAD: docs (ROADMAP, spec
§Phase 2 Slice 2) + commit + PR. **STOP at open PR — independent outside review before merge.**

### Status
- [x] STOP for go-ahead → approved (measure-real-load + coexist-both-dims)
- [x] ARCHITECT — llama-server API verified (Context7/ggml-org docs); design locked
- [x] BUILDER steps 1–9 — agent probe (`/props`+`/slots`), `queue_depth`→Option,
  `max_concurrent` through caps→pool→scorer, dim 12 (no supersession), weight 1.0.
- [x] OPERATOR gates — build ✓, clippy `-D warnings` ✓, fmt ✓; lib 504→**512** (+8). No lib unwrap.
- [x] REVIEWER independent hunt list — **CLEAN, all 8 items PASS** (best-effort probe,
  None honesty, wire-compat, div-by-zero guard, no-supersession, determinism, no lib
  unwrap, anti-trivial tests vs Q6).
- [x] LEAD docs (ROADMAP, spec §Phase 2 Slice 2) + commit + PR.
- [ ] INDEPENDENT outside review before merge

---

## DONE — audit log (collapsed; full detail in git history + ROADMAP)

- **Scorer Phase 2 Slice 1 — live-load dims 10/11/13** (PR #23, `2026-06-19`, merged
  `90b5251`) — `queue_depth`/`ttft_p50`/`precise_vram_free` read agent telemetry already
  at the pool boundary (`vram_total_mb` was already a `BackendState` field + populated by
  `pool_sync`, so Slice 1 was pure `scored.rs` + `config.rs`). `is_some()` presence
  predicate (`Some(0)` scored, not absent); dim 13 supersedes dim 5 per-candidate (no
  VRAM double-count); latency-aware balanced weights (`ttft_p50=3`/`queue_depth=2`/
  `precise_vram_free=2`). `QUEUE_REF=8`/`TTFT_REF=2000`. lib 499→504. Independent
  fresh-context review CLEAN; all CI green. **Test gotcha:** the Q6 call-uniform pre-pass
  drops any dim present on only ONE candidate — live-dim tests need ≥2 reporters, and
  per-candidate supersession is tested directly on `compute_raw`.
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
- **Agent ttft measurement (dim 11)** — the agent reports `ttft_p50_ms: None` today; dim 11
  reads the field but is never fed. Probe llama-server `/metrics` (`--metrics` flag) and track
  a rolling p50 on the agent. Lights up dim 11 the way Slice 2 lit dims 10/12.
- **Deferred dim-3** (`prompt_size_vs_capacity`) — new `Backend.max_context_len` config +
  proxy token estimation; switch proxy (`api/openai.rs:359`, `server.rs:1437`) from
  `route_excluding` to `route_scored` populating `RouteContext.prompt_tokens`.
- **Scorer Phase 3** — per-(backend,model) `RoutingStats` (EWMA latency, error-rate).
  Open-question Q1: in-memory (recommended) vs SQLite persistence — decide before building.
- **Scorer Phase 4** — locality/cost/capability (dims 18–23).
- v1.3 milestone — speculative decoding, full mDNS discovery, routing-strategy plugins.
