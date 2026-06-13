# Herd — Working TODO

> Scratchpad for in-flight work. Milestone tracking lives in `ROADMAP.md`;
> the v1.2 PR breakdown + acceptance checklist live in `tasks/HERD-V1.2-SPRINT.md`.

**Last updated:** 2026-06-13

---

## PLANNED (awaiting go-ahead) — v1.2 PR #7: agent nodes routable via BackendPool

Branch `feat/v1.2-pr7-backendpool-integration` off `main` (`31f9203`, has 6c). The keystone:
agent nodes (in-memory `NodeRegistry`) appear in `BackendPool` and route identically to
static/enrolled backends. Source = registry heartbeat freshness, NOT SQLite (5.1 exclusions stay).

### Plan (~12 steps)
1. New module `src/nodes/pool_sync.rs`: `AgentPoolSync` struct + `reconcile(registry, pool)` —
   mirror of `health.rs::sync_to_pool` but agent-sourced.
2. Source = `registry.fresh_nodes()` (strict TTL, NO grace). Build the fresh-key set
   `{"agent:{node_id}"}` from `AgentState.capabilities.node_id`.
3. Map each `AgentCapabilities` → `Backend { name:"agent:{node_id}", url:address, backend, priority:50,
   tags:vec![], ..Default }`; on add: `update_models(models_loaded)`, `set_vram(vram_total_mb)` when >0.
   `healthy = true` (in fresh_nodes() ⇒ alive by definition).
4. PREFIX OWNERSHIP (load-bearing): removal guard = `name.starts_with("agent:") && !fresh.contains(name)`.
   Never touch `node:` or static entries. Add new / update existing (mirror sync_to_pool's get→mutate→update).
5. Driver: `AgentPoolSync::spawn(registry, pool, interval)` — dedicated bg task, default 2s,
   env override `HERD_AGENT_POOL_SYNC_SECS` (mirrors `HERD_AGENT_UPDATE_GRACE_SECS` resolution).
6. Wire in `server.rs` beside the eviction task (~L530, where `state.node_registry`+`state.pool` exist).
   Register `pub mod pool_sync;` in `src/nodes/mod.rs`; re-export `AgentPoolSync` if siblings are.
7. Drain→503: TTL lapse ⇒ leaves fresh_nodes() ⇒ reconciler removes `agent:` entry ⇒ empty pool ⇒
   existing routers already return "No healthy backends" ⇒ 503. No new fallback.
8. Do NOT factor a shared helper with sync_to_pool unless the static path stays provably byte-identical;
   default = keep the agent reconciler self-contained (disjoint prefix ownership). Note the decision.
9. Do NOT change: `get_routable_nodes`/`get_pollable_nodes` (5.1), `sync_to_pool`, any router strategy.
10. Tests (pool_sync.rs unit + model_aware where natural):
    (a) fresh agent → `agent:{id}` in pool w/ models, routable via ModelAwareRouter like a static backend;
    (b) agent stale (TTL lapse) → entry removed, pool empty → `route()` Errs (503 path);
    (c) run BOTH reconcilers → agent reconcile never removes a `node:`/static entry (static set survives);
    (d) same host as enrolled `node:` AND agent `agent:` → two distinct entries (decision 14, coexist).
11. Sprint doc: PR #7 → ✅ + tick the two acceptance items ("agent nodes in BackendPool route identically",
    "503 when backends gone"); note the known limitation (enrolled+agent = 2 entries; dedup is v1.3).
    Update ROADMAP. No Cargo.toml bump.
12. Done = `cargo build` + `cargo test` (count grown from 484) + `clippy --all-targets -- -D warnings`
    + `fmt --check`. Then commit + push + open PR vs main (do NOT auto-merge). #8 branches off main after #7.

### Rules
no unwrap/expect in lib code; conventional commits; commit trailer
`Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`.

---

## Backlog
- #8 — Integration test: gateway + 1 agent in-process, request routes through agent's stub llama-server.
- v1.1.1 / v1.1.2 not git-tagged (tags stop at v1.1.0); consider 1.2.0 bump + tags once #7/#8 land.
