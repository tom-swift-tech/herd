# Herd v2 — Distributed Inference Spec

**Status:** v1.2 in progress (PRs #1–#6c landed of 8; #7 BackendPool integration and #8 integration test pending; see `tasks/HERD-V1.2-SPRINT.md`)
**Author:** Tom Swift (Director) + Gage
**Date:** 2026-04-17 (last reconciled with implementation: 2026-06-12)
**Targets:** v1.2 (foundation) → v1.3 (speculative) → v1.4 (pipeline)
**Supersedes:** N/A — extends `ROADMAP.md` v1.2.0+ "llama.cpp RPC integration for tensor-parallel sharding"

---

## Summary

Herd today is a centralized router with a static (or auto-probed) backend list. Each backend is a complete, standalone llama-server / Ollama / openai-compat endpoint. Herd's job is to pick one and forward.

Herd v2 keeps the centralized gateway model but introduces a **second run mode for the same binary** — `herd agent` — that runs on each GPU node, owns the local inference process, and reports live state up to the gateway. This unlocks **deployment-aware routing**: instead of "pick a backend that has model X", the gateway asks "pick a *deployment* that serves model X", where a deployment may be a single node, a speculative draft/verifier pair, or a pipeline-parallel group.

Goals:

- Serve models too large for any single GPU (Qwen2.5-72B-class) via llama.cpp RPC pipeline parallelism
- Accelerate daily-driver models 2-3x via speculative decoding across nodes
- Self-registering nodes — adding GPU box #4 becomes additive, not a config rewrite
- Keep the single-binary, single-source-of-truth, audit-everything model

Non-goals (explicitly out of scope, see "Non-Goals" below):

- Gossip, CRDTs, peer-to-peer consensus
- Tensor parallelism (needs NVLink/InfiniBand)
- Untrusted-network operation
- Replacing the existing static-backend config path (it stays — agent registration is *additive*)

---

## Motivation

### The bottleneck

Combined fleet VRAM is ~60GB across CITADEL (32GB), minipc (16GB), warden (12GB), but it's stranded — Herd routes to whichever single GPU can hold the requested model. The largest single-node ceiling caps the entire fleet.

### What's already in v1.2.0+ roadmap

`ROADMAP.md` already lists "llama.cpp RPC integration for tensor-parallel sharding across fleet nodes" under v1.2.0+. This spec turns that line into an architecture, and adds two adjacent capabilities (speculative decoding, agent-mode) that share the same plumbing.

### Why agent-mode and not just RPC config

llama.cpp RPC alone would let us add `rpc:` URLs to existing static-backend config and call it a day. But:

- Static config can't track live VRAM, model load state, or queue depth on the RPC workers
- Standing up an RPC group requires coordinated lifecycle on N nodes — start `rpc-server` on workers, then start `llama-server --rpc <list>` on the leader. A node-side daemon makes this scriptable.
- Agent-mode also enables the speculative-decoding deployment, which has nothing to do with RPC
- Agent-mode is the natural place for per-node telemetry, auto-recovery, and model warm-up logic — moving these out of the gateway reduces gateway complexity

In short: RPC is one strategy; agent-mode is the substrate that makes RPC operationally tractable *and* unlocks speculative as a near-zero-cost addition.

---

## Architecture

### Run modes

```
herd serve                     # gateway mode (today's default — unchanged behavior)
herd agent --gateway <url>     # node mode (new)
```

Same binary. `cli.rs` already dispatches subcommands; `agent` becomes a peer to `serve` rather than a flag on it.

### Deployment topology

```
                    ┌──────────────────────────┐
                    │     Herd Gateway          │
                    │     (herd serve)          │
                    │                           │
                    │  • OpenAI-compat API      │
                    │  • NodeRegistry (new)     │
                    │  • DeploymentManager (new)│
                    │  • Router (extended)      │
                    │  • Existing v1.x features │
                    └────────────┬──────────────┘
                                 │ Tailscale (shared token auth)
              ┌──────────────────┼──────────────────┐
              │                  │                  │
       ┌──────▼──────┐    ┌──────▼──────┐   ┌──────▼──────┐
       │ herd agent  │    │ herd agent  │   │ herd agent  │
       │  citadel    │    │   minipc    │   │   warden    │
       │  (5090)     │    │   (4080)    │   │   (4070)    │
       │             │    │             │   │             │
       │ llama-server│    │ llama-server│   │ llama-server│
       │ +rpc-server │    │ +rpc-server │   │ +rpc-server │
       └─────────────┘    └─────────────┘   └─────────────┘
```

Three deployment strategies share this substrate:

1. **Single** — agent runs `llama-server`, gateway routes requests to it
2. **Speculative** — two agents run draft + verifier llama-server instances; verifier coordinates with draft via llama.cpp's `--model-draft`
3. **Pipeline** — N agents run `rpc-server`; one agent runs `llama-server --rpc <list>` as the pipeline leader

### Coexistence with existing static backends

**Static-backend config remains the supported v1.x path.** Agent-registered backends are added to the same `BackendPool` alongside statically-configured ones. The router doesn't care how a backend got there — only what it can serve and what its health looks like. This means:

- Existing deployments upgrade to v1.2+ with zero config changes
- Agents can be introduced node-by-node
- A single node can be both statically configured *and* agent-managed during transition

Conflict resolution for v1.2 is intentionally narrow: an agent registration overrides a static backend only when both describe the same logical node identity, defined as explicit `node_id` plus advertised inference address. If that identity does not match exactly, both entries remain visible and the gateway logs a duplication warning instead of silently shadowing one backend.

---

## Phased Delivery

### v1.2 — Agent/Gateway Foundation

**Scope:** `herd agent` ships. Single-node deployments only. No speculative, no pipeline.

**Acceptance:**

- [ ] `herd agent --gateway <url> --node-id <id>` subcommand exists
- [ ] Agent sends heartbeat every 2s with full capability snapshot
- [x] `POST /api/internal/nodes/heartbeat` is the only v1.2 agent-control endpoint; unknown `node_id` values are implicitly registered on first heartbeat *(PR #3, `src/api/internal.rs`)*
- [x] Gateway maintains in-memory `NodeRegistry` keyed by `node_id` with TTL eviction (default 30s) *(struct in PR #2; on `AppState` + 10s eviction task in PR #3)*
- [ ] Agent-registered nodes appear in `BackendPool` and route identically to static backends *(PR #7)*
- [x] Existing static-backend config path is unchanged
- [ ] Dashboard Fleet tab shows agent-registered nodes with live state *(PR #5)*
- [ ] Both modes can run on the same host (CITADEL self-test scenario) *(PR #4/#8)*
- [x] Auth: shared bearer token via env var (`HERD_AGENT_TOKEN`) *(PR #3 — unset = warn+allow, set = required)*
- [ ] Gateway returns 503 with clear error if all healthy backends — agent and static — are gone (no hidden fallback) *(PR #7)*
- [~] Tests: `NodeRegistry` unit tests *(10 in PR #2)*, heartbeat protocol tests *(8 in PR #3)*, integration test with gateway + 1 agent in same process *(pending PR #8)*

### v1.3 — Speculative Decoding Deployments

**Scope:** Cheap-win throughput multiplier. CITADEL + warden as draft/verifier pair for daily-driver models.

**Acceptance:**

- [ ] `Deployment::Speculative { model, draft_model, draft_node, verifier_node }` variant
- [ ] `DeploymentManager` reads deployment manifests from config and/or admin API
- [ ] Verifier agent coordinates with draft agent via llama.cpp's native `--model-draft`
- [ ] Router selects deployment by model name; falls back to single-node if no deployment matches
- [ ] Telemetry: token acceptance rate exposed as Prometheus metric (`herd_speculative_acceptance_rate`)
- [ ] Validation: tokens/sec on Gemma-3-4B (draft on warden) + Qwen2.5-Coder-32B (verifier on CITADEL) vs. CITADEL alone
- [ ] Acceptance: ≥1.8x throughput on at least one daily-driver model

### v1.4 — Pipeline Parallel Deployments

**Scope:** Run models that don't fit on any single GPU. Qwen2.5-72B-Coder Q4_K_M is the target.

**Acceptance:**

- [ ] Agent can spawn and supervise `llama.cpp rpc-server` on demand
- [ ] `Deployment::Pipeline { model, nodes (ordered), leader }` variant
- [ ] Pipeline leader runs `llama-server --rpc node1:50052,node2:50052,...`
- [ ] Capability advertisement extended with `rpc_capable: bool` and `rpc_vram_free_mb`
- [ ] Auto-formation: gateway can stand up a pipeline group on demand if no node has the model and combined free VRAM is sufficient
- [ ] Validation: load Qwen2.5-72B-Coder Q4_K_M across CITADEL+minipc+warden
- [ ] Acceptance: ≥10 tok/s sustained throughput, TTFT under 5s

### v1.5+ — Polish

- Manifest as YAML in main config, with hot-reload via existing `POST /admin/reload`
- CLI: `herd deploy <model> --strategy <strategy> --nodes <list>`
- Optional gateway HA (probably not warranted at 3-node scale; revisit if fleet grows)
- mTLS option for agent↔gateway (current shared-token approach is adequate for Tailscale)

---

## Data Structures (Sketch)

> These are first-pass sketches. Verify against existing types in `src/nodes/`, `src/backend/`, and `src/config.rs` before implementing — there's likely overlap with `NodeInfo`, `BackendPool`, etc. that we should extend rather than parallel.

### NodeRegistry

> **Implementation note (2026-05-08):** PR #2 (`be6f24e`) landed this in `src/nodes/registry.rs`. The shipped shape diverges from the original sketch — specifics below. This block has been updated to reflect what's actually in tree.

```rust
// src/nodes/registry.rs
use crate::config::BackendType;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};   // Instant (monotonic), not SystemTime — clock jumps shouldn't evict nodes
use tokio::sync::RwLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCapabilities {
    pub node_id: String,                // human-readable, e.g. "citadel-5090"
    pub backend: BackendType,           // reused existing enum (not BackendKind)
    pub address: String,                // tailscale hostname:port for inference traffic
    #[serde(default)]
    pub gpu_model: Option<String>,      // string, not GpuInfo struct (no GpuInfo type exists in tree)
    pub vram_total_mb: u64,
    pub vram_free_mb: u64,
    #[serde(default)]
    pub models_loaded: Vec<String>,     // model IDs, not ModelInfo structs
    #[serde(default)]
    pub queue_depth: u32,
    #[serde(default)]
    pub ttft_p50_ms: Option<u32>,
    #[serde(default)]
    pub rpc_capable: bool,              // can join llama-rpc pipeline group
    #[serde(default)]
    pub rpc_port: Option<u16>,          // if rpc_capable, which port rpc-server is on (or will be)
    pub agent_version: String,          // for version skew detection
}

#[derive(Debug, Clone)]
pub struct AgentState {
    pub capabilities: AgentCapabilities,
    pub last_heartbeat: Instant,        // monotonic
    // health: AgentHealth deferred — v1.2 only liveness signal is "last heartbeat within TTL"
}

impl AgentState {
    pub fn age(&self) -> Duration;
    pub fn is_fresh(&self, ttl: Duration) -> bool;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeartbeatOutcome {              // returned to caller for logging
    Registered,                          // unknown node_id
    Updated,                             // known node_id
}

pub struct NodeRegistry {
    nodes: Arc<RwLock<HashMap<String, AgentState>>>,
    ttl: Duration,
    clock: Clock,                        // injectable Arc<dyn Fn() -> Instant + Send + Sync> for tests
}

impl NodeRegistry {
    pub fn new(ttl: Duration) -> Self;
    // No public register() — protocol is heartbeat-only per sprint decision.
    pub async fn heartbeat(&self, caps: AgentCapabilities) -> HeartbeatOutcome;
    pub async fn evict_stale(&self) -> Vec<String>;          // returns evicted node_ids
    pub async fn list(&self) -> Vec<AgentState>;             // all current states
    pub async fn fresh_nodes(&self) -> Vec<AgentState>;      // filtered by TTL
    pub async fn get(&self, node_id: &str) -> Option<AgentState>;
    pub async fn len(&self) -> usize;
    pub async fn is_empty(&self) -> bool;
    // find_for_model() deferred — lands in PR #7 when BackendPool integration adds model-by-node lookup.
}
```

**Deferred from the original sketch** (decided in `tasks/todo.md`, accepted):
- `AgentHealth` enum — no v1.2 producer of `Degraded`/`Unreachable`. Liveness is expressed via `is_fresh(ttl)`. The enum lands when a producer does.
- Public `register()` — sprint protocol is heartbeat-only; the endpoint registers unknown nodes implicitly.
- `find_for_model()` — added in PR #7 alongside `BackendPool` integration.
- `GpuInfo` / `ModelInfo` reuse — no such types exist in tree; `Option<String>` and `Vec<String>` are the v1.2 surface. Revisit if richer GPU telemetry becomes useful.

### Deployment

```rust
// src/router/deployment.rs (new)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "strategy", rename_all = "snake_case")]
pub enum Deployment {
    Single {
        model: String,
        node: String,                   // node_id
    },
    Speculative {
        model: String,
        draft_model: String,
        draft_node: String,
        verifier_node: String,          // request entry point
    },
    Pipeline {
        model: String,
        nodes: Vec<String>,             // ordered: rank 0, 1, 2, ...
        leader: String,                 // request entry point (usually rank 0)
    },
}

impl Deployment {
    pub fn entry_node(&self) -> &str { /* ... */ }
    pub fn serves_model(&self, model: &str) -> bool { /* ... */ }
    pub fn participating_nodes(&self) -> Vec<&str> { /* ... */ }
}

pub struct DeploymentManager {
    deployments: Arc<RwLock<Vec<Deployment>>>,
}
```

### Heartbeat protocol

> **Implementation note (2026-06-05):** PR #3 landed the gateway side of this protocol in `src/api/internal.rs`. The `timestamp` field is accepted but ignored — the registry uses its own monotonic clock for freshness, so agent clock skew cannot cause premature eviction. `deployments_assigned` is always `[]` in v1.2 (single-node only). `next_heartbeat_secs` is a fixed `2`. The agent side (the client that *sends* these) lands in PR #4.

Initial implementation: HTTP POST every 2s with full state snapshot. Long-poll or gRPC-streaming as v1.5 if heartbeat overhead becomes measurable.

```
POST /api/internal/nodes/heartbeat
Authorization: Bearer <HERD_AGENT_TOKEN>
Content-Type: application/json

{
  "capabilities": { ...AgentCapabilities... },
  "timestamp": "2026-04-17T..."
}

200 OK
{
  "registered": true,
  "deployments_assigned": [ ... ],     // optional: gateway tells agent what to load
  "next_heartbeat_secs": 2
}
```

Initial registration uses the same endpoint — gateway treats unknown `node_id` as a registration event. v1.2 does not add a separate `/api/internal/nodes/register`.

Live agent state remains in the in-memory `NodeRegistry`. The gateway may project that state into existing fleet/dashboard responses, but it should not persist every heartbeat to SQLite. SQLite remains the source of truth for operator-managed nodes and historical/download metadata; `NodeRegistry` is the source of truth for live agent liveness/capabilities.

---

## Routing Logic

Pseudocode for the deployment-aware path:

```rust
async fn route(req: ChatRequest) -> Result<Response> {
    let model = req.model.as_str();

    // 1. Try deployment-aware path first
    if let Some(deployment) = deployment_manager.find_for_model(model).await {
        let entry_id = deployment.entry_node();
        if let Some(node) = registry.get(entry_id).await
            .filter(|n| n.health == AgentHealth::Healthy)
        {
            return forward_to(&node.capabilities.address, req).await;
        }
        // Entry node unhealthy — fall through to legacy path
    }

    // 2. Fall back to existing BackendPool routing
    //    (covers static backends and agent-registered single-node deployments)
    legacy_router.route(req).await
}
```

For v1.2, gateway proxies bytes both ways (cleanest client model, consistent with current behavior). Direct-connection (307 redirect to the agent) is a v1.5+ optimization with implications for client Tailscale access.

---

## File Layout

> The existing structure is modular and well-organized. Suggested additions, not a rewrite:

```
src/
├── agent/                     # EXISTING — agent sessions / tool calling
│   └── ...
├── nodes/                     # EXISTING — fleet registry, herd-tune integration
│   ├── mod.rs
│   ├── registry.rs            # NEW: NodeRegistry (in-memory, agent heartbeats)
│   └── ...
├── router/                    # EXISTING — 4 routing strategies
│   ├── mod.rs
│   ├── deployment.rs          # NEW: Deployment enum, DeploymentManager
│   └── ...
├── api/
│   ├── internal.rs            # NEW: agent-facing endpoints (heartbeat, register)
│   └── ...
├── daemon/                    # NEW: agent-mode code
│   ├── mod.rs
│   ├── client.rs              # heartbeat client
│   ├── capabilities.rs        # GPU/VRAM/model detection (reuse herd-tune logic where possible)
│   └── lifecycle.rs           # spawn/manage llama-server and rpc-server processes
└── cli.rs                     # EXISTING — extend to dispatch `agent` subcommand
```

**Naming note:** `src/agent/` already exists for agent sessions. To avoid confusion, the new node-side daemon code lives under `src/daemon/`. "Agent mode" remains the user-facing CLI term (`herd agent`), but the module is `daemon` to disambiguate from the existing agent-sessions feature.

---

## Open Questions for Tom

**All questions below are resolved and locked for v1.2.** See `tasks/HERD-V1.2-SPRINT.md` → "Decisions (locked for v1.2)" for the full record, including the fleet-authority decisions added during the PR #6 series (questions 15–25 in that doc).

For reference, the original questions and their outcomes:

1. **Module naming.** RESOLVED: `src/daemon/` — avoids collision with existing `src/agent/` (sessions). User-facing CLI term remains `herd agent`.

2. **Auth mechanism.** RESOLVED: Shared bearer token via `HERD_AGENT_TOKEN` env var. Unset = warn+allow; set = required. mTLS documented as future hardening.

3. **Node ID assignment.** RESOLVED: Human-readable, hostname-derived (`hostname-gpu` format, e.g. `citadel-5090`), with `--node-id` override flag.

4. **Heartbeat cadence.** RESOLVED: 2s default, configurable per-agent via flag or env. 30s TTL on the gateway.

5. **Deployment manifest source.** RESOLVED: Deferred beyond v1.2. v1.2 ships single-node only and adds no new deployment config. When it lands, top-level `deployments:` in `herd.yaml` (Option A) is the target.

6. **Conflict resolution.** RESOLVED: Agent registration overrides a static backend only on exact logical-node identity match (`node_id` + advertised inference address). Otherwise both remain and the gateway logs a duplication warning.

7. **Gateway address discovery.** RESOLVED: Explicit `--gateway <url>` required on the agent. No auto-discovery; `herd.starbase` (Tailscale DNS) documented as the recommended value.

8. **Speculative decoding model pairing.** RESOLVED: Manual per-deployment config for v1.3. Auto-suggestion is a v1.5 nice-to-have.

---

## Testing Strategy

**Time-dependent test infra (established in PR #2, reuse in PRs #3+):**

`NodeRegistry` accepts an injectable `Clock = Arc<dyn Fn() -> Instant + Send + Sync>` via a private `with_clock` constructor. Production calls `NodeRegistry::new(ttl)` which uses `Instant::now`; tests construct a `TestClock` (mutex-protected `Instant`) and drive time deterministically with `clock.advance(Duration)`. This keeps monotonic-`Instant` semantics in production while sidestepping `tokio::time::advance`, which does not affect `Instant::now`. Future time-dependent components (heartbeat client, evictor task, deployment health checks) should use the same pattern.

**v1.2:**

- Unit: `NodeRegistry` — heartbeat outcomes, eviction, freshness, re-registration after eviction *(landed in PR #2: 10 tests)*
- Unit: heartbeat protocol — auth, malformed payloads, version skew *(PR #3)*
- Integration: gateway + 1 agent in same process, request routes through agent's llama-server (stub) *(PR #5/#7)*
- Manual smoke test: gateway on CITADEL, agent on minipc, `curl` to gateway lands on minipc

**v1.3:**

- Integration: speculative deployment with mocked draft/verifier returning canned tokens; verify acceptance-rate metric
- Real-hardware benchmark: speculative throughput vs. single-node baseline on Qwen2.5-Coder-32B
- Failure mode: draft node disappears mid-request — verifier should fall back to non-speculative

**v1.4:**

- Integration: rpc-server spawn/teardown lifecycle on a single node (loopback RPC group of 1)
- Real-hardware: 72B model loads across 3 nodes, sustained throughput ≥10 tok/s
- Failure mode: middle pipeline node dies — leader detects and surfaces a clear error (no silent corruption)

---

## Non-Goals (Explicit)

To keep scope tight, **Herd v2 will NOT:**

- Run on untrusted networks (no peer auth beyond shared token; mTLS is future hardening)
- Support tensor parallelism (needs NVLink/InfiniBand, not viable on Tailscale)
- Implement gossip, CRDTs, or distributed consensus — centralized gateway is intentional
- Become an agent runtime in the VALOR sense — Herd serves inference, period
- Add a separate dashboard for agent-mode (extend existing Fleet tab)
- Auto-discover nodes on the network beyond the existing `discovery.static_nodes` mechanism (explicit registration only)
- Replace static-backend config — agent registration is additive

If scope creep starts pulling in any of the above, push back and re-scope.

---

## References

- llama.cpp RPC: https://github.com/ggerganov/llama.cpp/tree/master/examples/rpc
- llama.cpp speculative decoding: https://github.com/ggerganov/llama.cpp/tree/master/examples/speculative
- Existing Herd v1.x docs: `docs/LLAMA_CPP_BACKEND.md`, `docs/specs/herd-tune-spec.md`
- Roadmap entry: `ROADMAP.md` v1.2.0+ "llama.cpp RPC integration"
