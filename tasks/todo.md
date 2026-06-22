# Herd — Working TODO

> Scratchpad for in-flight work. Milestone tracking lives in `ROADMAP.md`;
> the v1.2 PR breakdown + acceptance checklist live in `tasks/HERD-V1.2-SPRINT.md`.
> Full scorer design: `docs/specs/smart-routing-scorer-spec.md`.

**Last updated:** 2026-06-19

---

## ACTIVE — Scorer Phase 2: prompt_size_vs_capacity (dim 3)

Branch `feat/v1.2-scorer-pr-e-dim3` off `main` (post-#24). The deferred dim-3 feature.

**Scope decision (Director, 2026-06-19):** picked over ttft (dim 11), which isn't cleanly
reachable for an observe-only agent (no per-request timings; `/metrics` has no quantiles).
dim-3 is self-contained, no approximation, no new protocol field.

- New `Backend.max_context_len: Option<u32>` config field (`#[serde(default)]`); static
  backends only — agent nodes stay None (a later slice could report `/props` `n_ctx`).
- `api/openai.rs::estimate_prompt_tokens(body)` — ~4 chars/token over chat `messages`
  content (string or multimodal text) or a `prompt` string; None when unrecognized/empty.
- BOTH proxy sites (`openai.rs` chat, `server.rs` generic) switch `route_excluding` →
  `route_scored(&RouteContext{prompt_tokens, ..})`. No-op for the 4 legacy routers (ctx-blind).
- `scored.rs` dim 3: present iff `prompt_tokens.is_some()` AND `max_context_len Some(>0)`;
  `n = clamp((1 - prompt/window)/0.5, 0, 1)`. `compute_raw` gains a `prompt_tokens` arg.

### Status
- [x] ARCHITECT — spec dim-3 formula confirmed; `RouteContext.prompt_tokens` seam already
  existed (route_scored), so wiring is threading one value + the config field + the proxy switch.
- [x] BUILDER — config field, estimator, both proxy switches, dim 3, tests.
- [x] OPERATOR gates — build ✓, clippy `-D warnings` ✓, fmt ✓; lib 512→**518** (+6). No lib unwrap.
- [x] REVIEWER independent hunt list — **CLEAN, all 8 items PASS** (legacy-router safety,
  div-by-zero/presence guard, formula, estimator honesty, wire-compat, determinism, no lib
  unwrap, anti-trivial tests).
- [x] LEAD docs (ROADMAP, spec dim-3 source-gap closed) + commit + PR.
- [ ] INDEPENDENT outside review before merge

---

## DONE — audit log (collapsed; full detail in git history + ROADMAP)

- **Scorer Phase 2 Slice 2 — measure real load + dim 12** (PR #24, `2026-06-19`, merged
  `53f86b7`) — the `herd agent` daemon now MEASURES load: probes llama-server `/props`
  (`total_slots`→`max_concurrent`) + `/slots` (busy count→`queue_depth`), best-effort
  (failure/disabled/non-llama → None, never flips reachable). `AgentCapabilities.queue_depth`
  → `Option<u32>` (honest unmeasured, no fake idle) + new `max_concurrent` (both serde-default
  wire-compat). dim 12 `concurrency_saturation` = `1-queue_depth/max_concurrent`, guard `max>0`,
  COEXISTS with dim 10 (no supersession), weight 1.0. lib 504→512. Endpoints verified vs
  ggml-org docs. Independent review CLEAN. **Still deferred:** agent ttft (dim 11) — `/metrics`
  has no quantiles, observe-only agent can't get a true p50.

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
  reads the field but is never fed. A TRUE p50 isn't reachable for an observe-only agent
  (`/metrics` is counters/gauges, no quantiles; per-request `timings.prompt_ms` only on
  completion responses the agent doesn't see). Realistic option: interval-MEAN TTFT from
  `/metrics` counter deltas (needs `--metrics` + daemon delta-state), reported as an
  approximation. Deferred pending a decision on whether the mean approximation is wanted.
- **dim-3 agent ctx source** — extend dim 3 to agent nodes by reporting llama-server
  `/props` `default_generation_settings.n_ctx` → `max_context_len` (Slice E does config-only).
- **Scorer Phase 3** — per-(backend,model) `RoutingStats` (EWMA latency, error-rate).
  Open-question Q1: in-memory (recommended) vs SQLite persistence — decide before building.
- **Scorer Phase 4** — locality/cost/capability (dims 18–23).
- v1.3 milestone — speculative decoding, full mDNS discovery, routing-strategy plugins.
