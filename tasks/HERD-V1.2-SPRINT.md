# HERD v1.2 Sprint — Agent/Gateway Foundation

**Spec:** `docs/specs/v2-distributed-inference-spec.md`
**Target:** v1.2 (foundation) — `herd agent` ships, single-node deployments only. No speculative, no pipeline.
**Status:** PRs #1–#5 landed of 8 (last reconciled with implementation: 2026-06-10)

> This doc tracks the PR breakdown and acceptance checklist for the v1.2 milestone.
> The architecture, data structures, and rationale live in the spec — this is the
> sequencing/checklist companion the spec and `ROADMAP.md` reference.

---

## PR Breakdown

| PR | Title | Scope | Status |
|----|-------|-------|--------|
| #1 | Seed `Deployment` module | `Deployment::Single` variant in `src/router/deployment.rs`, `primary_backend()` accessor + unit tests | ✅ landed (`2960289`) |
| #2 | `NodeRegistry` with TTL eviction | In-memory `NodeRegistry` keyed by `node_id`, heartbeat-only protocol, injectable `Clock` for deterministic time tests, 10 unit tests | ✅ landed (`be6f24e`) |
| #3 | Gateway heartbeat ingestion | `NodeRegistry` onto `AppState`; stale-eviction background task; `POST /api/internal/nodes/heartbeat` with `HERD_AGENT_TOKEN` bearer auth; heartbeat protocol tests | ✅ landed |
| #4 | `herd agent` CLI + daemon | Restructure CLI into `serve`/`agent` subcommands; `src/daemon/` (heartbeat client, capability detection, lifecycle); single-node deployment | ✅ landed |
| #5 | Agent node persistence + Fleet | Migration v5 (`source`, `agent_version`); write-through on transition; soft-evict (mark offline) + reaper for stale agent rows; Fleet tab reads unified SQLite store | ✅ |
| #6 | Heartbeat protocol hardening | Version-skew handling, deployments-assigned response plumbing, configurable TTL/cadence | ⬜ |
| #7 | `BackendPool` integration | Agent-registered nodes route identically to static backends; `NodeRegistry::find_for_model()`; conflict resolution (agent overrides static only on exact node-identity match) | ⬜ |
| #8 | Integration test + smoke | Gateway + 1 agent in same process; request routes through agent's (stub) llama-server end-to-end | ⬜ |

---

## v1.2 Acceptance Checklist

From the spec's "v1.2 — Agent/Gateway Foundation" acceptance block, annotated with PR ownership:

- [x] `herd agent --gateway <url> --node-id <id>` subcommand exists *(PR #4 — node_id defaults to hostname-gpu, e.g. `citadel-5090`)*
- [x] Agent sends heartbeat every 2s with full capability snapshot *(PR #4 — exponential backoff capped at 30s while gateway unreachable)*
- [x] `POST /api/internal/nodes/heartbeat` is the only v1.2 agent-control endpoint; unknown `node_id` values are implicitly registered on first heartbeat *(PR #3)*
- [x] Gateway maintains in-memory `NodeRegistry` keyed by `node_id` with TTL eviction (default 30s) *(struct in PR #2; on `AppState` + eviction task in PR #3)*
- [ ] Agent-registered nodes appear in `BackendPool` and route identically to static backends *(PR #7)*
- [x] Existing static-backend config path is unchanged *(maintained; verified PR #3)*
- [x] Dashboard Fleet tab shows agent-registered nodes with live state *(PR #5 — agent rows persisted to SQLite with `source='agent'`, online/offline status + source badge in the Fleet table; written on register/model-change/eviction, steady beats stay in-memory)*
- [~] Both modes can run on the same host (CITADEL self-test scenario) *(PR #4: guarded self-test in `tests/agent_daemon.rs` + manual smoke verified 2026-06-09; PR #8 extends to routed end-to-end)*
- [x] Auth: shared bearer token via env var (`HERD_AGENT_TOKEN`) *(PR #3)*
- [ ] Gateway returns 503 with clear error if all healthy backends — agent and static — are gone (no hidden fallback) *(PR #7)*
- [~] Tests: `NodeRegistry` unit tests *(PR #2: 10 tests)*, heartbeat protocol tests *(PR #3: 8 tests, verified green 2026-06-05)*, daemon unit + heartbeat-client integration tests *(PR #4: 39 unit tests + `tests/agent_daemon.rs`)*, integration test with gateway + 1 agent in same process *(PR #8)*

---

## Decisions (locked for v1.2)

These resolve the spec's "Open Questions for Tom":

1. **Module naming** — node-side daemon lives under `src/daemon/` (avoids colliding with existing `src/agent/` sessions). User-facing CLI term stays `herd agent`.
2. **Auth** — shared bearer token in `HERD_AGENT_TOKEN` env var. mTLS documented as future hardening, not day-one. When the token is unset the gateway logs a warning and allows the heartbeat (mirrors `require_api_key`'s "no key configured = allow" precedent, so the CITADEL self-test works without setup); when set, it is required.
3. **Node ID** — human-readable, hostname-derived (`hostname-gpu`, e.g. `citadel-5090`), with `--node-id` override.
4. **Heartbeat cadence** — 2s default, configurable per-agent. Gateway TTL eviction default 30s.
5. **Deployment manifest source** — deferred beyond v1.2 (single-node only ships here). Recommended: top-level `deployments:` in `herd.yaml` when it lands (v1.3).
6. **Conflict resolution** — agent registration overrides a static backend only on exact logical-node identity match (`node_id` + advertised inference address); otherwise both remain visible and a duplication warning is logged *(enforced in PR #7)*.
7. **Gateway discovery** — explicit `--gateway <url>` required on the agent. `herd.starbase` (Tailscale DNS) documented as recommended value; no auto-discovery.

---

## PR #5 — Agent Node Persistence + Fleet Integration (revised scope)

Supersedes the original "project NodeRegistry alongside SQLite" framing. Agent nodes
are persisted to `node_db`, so the Fleet tab reads a single store and no render-time
merge of two stores is needed. The in-memory `NodeRegistry` remains the routing/liveness
layer (PR #7); SQLite persistence here is operator-visibility + housekeeping.

Locked decisions (extend the list above):

8.  **Persistence + discriminator.** Agent heartbeats persist to the SQLite `nodes`
    table, discriminated by a new `source` column ('enrolled' | 'agent'). Migration v5
    adds `source` (DEFAULT 'enrolled', so all existing rows are correctly tagged
    enrolled) and `agent_version`. Fleet tab reads SQLite only.
9.  **Soft eviction (mark-offline-then-reap).** In-memory TTL eviction sets the SQLite
    row's `status='offline'` — it does NOT delete. A separate reaper hard-deletes
    `source='agent'` rows offline beyond a grace window (default 24h, configurable via
    `HERD_AGENT_REAP_GRACE_SECS`). Enrolled rows are never auto-reaped. Routing ignores
    SQLite for agents, so an 'offline' row is never a routing hazard.
10. **Write-through on transitions only.** SQLite is written on: first heartbeat
    (register), material capability change (models_loaded set changes), and eviction.
    Steady unchanged 2s beats stay in-memory only and never touch the DB.
11. **NodeRegistry stays DB-free.** Persistence glue lives in the heartbeat handler
    (`api/internal.rs`) and the evictor + new reaper tasks (`server.rs`), which already
    hold `AppState` with both stores. The registry keeps its injectable-Clock
    determinism with no `NodeDb` dependency. The evictor must surface evicted node_ids
    so the glue can mark them offline.
12. **Fleet-visible, not SQLite-routable.** Agent rows use status values OUTSIDE the
    routable set ('online'/'offline', never 'healthy'/'degraded'), so
    `get_routable_nodes()` cannot pull an agent node into the static routing path before
    PR #7 wires it deliberately through the in-memory registry.
13. **Persist durable fields only.** Map node_id→node_id (and hostname, since hostname is
    NOT NULL UNIQUE and agents have no separate hostname), address→backend_url (on-disk
    column is the legacy `ollama_url` — mirror existing `upsert_node`), backend, gpu_model,
    vram_total_mb→vram_mb, models_loaded, agent_version→new column. Dynamic perf fields
    (vram_free, queue_depth, ttft_p50) stay in-memory only — routing inputs, not records.
14. **No agent/enrolled merge in v1.2.** If one physical host is both enrolled and running
    an agent, two rows coexist (different hostname/source). Dedup deferred.

---

## Testing Infra Notes

`NodeRegistry` accepts an injectable `Clock = Arc<dyn Fn() -> Instant + Send + Sync>` via a private
`with_clock` constructor (established in PR #2). Production uses `Instant::now`; tests drive a
mutex-protected `TestClock` with `advance(Duration)`. This sidesteps `tokio::time::advance` (which
does not affect `Instant::now`) while keeping monotonic semantics in production. Reuse this pattern
for the heartbeat client, evictor task, and deployment health checks in PRs #4+.

## References

- Spec: `docs/specs/v2-distributed-inference-spec.md`
- Roadmap: `ROADMAP.md` → "v1.2.0+ — Distributed Inference (In Spec)"
- Lessons: `tasks/lessons.md`
