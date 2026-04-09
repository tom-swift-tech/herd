# CLAUDE.md — Herd

> **For use with Claude Code.** Project-level instructions for the Herd repository.

---

## Project Overview

| Field | Value |
|-------|-------|
| **Repo** | `swift-innovate/herd` (GitHub) |
| **Language** | Rust |
| **Version** | 1.1.0 |
| **Purpose** | Intelligent LLM router — GPU-aware routing, circuit breakers, OpenAI compat, agent sessions, fleet management, dashboard |

Herd is a single-binary reverse proxy for local LLM backends. It routes AI workloads across local GPU nodes with model awareness, health tracking, and observability.

As of v0.9.0, the former private "Herd Pro" repository has been merged into this repo. **There is only one Herd repo now.**

## Backend Strategy

Herd supports three backend types per node:

1. **Ollama** (stable) — Herd talks to Ollama's HTTP API. herd-tune configures Ollama env vars.
2. **llama-server** (v1.0) — Herd talks to llama.cpp's llama-server via its OpenAI-compatible API. herd-tune detects GPU vendor, downloads the correct llama-server binary, and starts it.
3. **openai-compat** (v1.0) — Herd talks to any OpenAI-compatible endpoint.

**Why llama-server?** Benchmarked on RTX 5090 (2026-04-08): llama-server delivers 44-80% faster TTFT and ~4x throughput vs Ollama on the same model and hardware. Ollama's Go serving layer is the bottleneck. Full benchmark data and architecture details in `docs/LLAMA_CPP_BACKEND.md`.

**Herd doesn't care about GPU vendor.** Both backends expose OpenAI-compatible HTTP. The GPU backend complexity (CUDA vs ROCm vs SYCL vs Vulkan) is pushed to node setup via herd-tune. Herd routes requests — it doesn't do inference.

**Important:** Ollama support is NOT being removed. Both backends coexist. The `backend` field in node registration distinguishes them. Existing Ollama fleet deployments continue working unchanged.

## Architecture

- **Framework:** Axum 0.7, Tokio async runtime
- **State:** `Arc<RwLock<...>>` for shared mutable state, `AtomicU64`/`AtomicU32` for lock-free config values
- **Routing:** 4 pluggable strategies (priority, model_aware, least_busy, weighted_round_robin)
- **Persistence:** JSONL for analytics/audit, SQLite (`rusqlite`) for node registry
- **Config:** YAML with hot-reload via file watcher (30s) or `POST /admin/reload`

### Module Map

| Module | Purpose |
|--------|---------|
| `src/server.rs` | AppState, route registration, proxy handler, middleware |
| `src/config.rs` | All config structs, YAML parsing, validation |
| `src/router/` | 4 routing strategies + Router trait |
| `src/backend/` | BackendPool, HealthChecker, ModelDiscovery, ModelWarmer |
| `src/agent/` | Session management, tool execution, permissions, audit, WebSocket |
| `src/nodes/` | SQLite node registry, health polling, herd-tune integration |
| `src/api/` | Admin CRUD, OpenAI compat, agent endpoints, node endpoints |
| `src/classifier.rs` | Task-based tier classification middleware |
| `src/analytics.rs` | JSONL request logging with rotation |
| `src/metrics.rs` | In-memory Prometheus metrics |

## Build & Test

```bash
cargo build          # Debug build
cargo test           # 257 tests (unit + integration)
cargo build --release  # Release build
```

## Key Design Docs

- `docs/LLAMA_CPP_BACKEND.md` — Benchmark results, llama-server backend strategy, GPU support matrix, herd-tune changes, fleet architecture
- `docs/specs/herd-tune-spec.md` — Node auto-registration spec (supports both Ollama and llama-server backends)
- `ROADMAP.md` — Release milestones and feature targets

## Code Quality Rules

- All new features default to `enabled: false` — zero overhead when not opted in
- All new config fields must have sensible defaults and not break existing `herd.yaml` files
- New endpoints must appear in `skills.md` and the dashboard Agent Guide tab
- JSONL analytics logging must be extended (not replaced) with new fields
- Tests for each feature — at minimum: enabled/disabled, happy path, edge cases
- All public-facing headers use the `X-Herd-` prefix
- **Never bail! on config errors** — degrade gracefully, warn+disable features
- **Backend-agnostic routing** — routing logic must work identically for Ollama and llama-server backends. Use the `backend` field in node registration to determine health check paths and model list behavior, but the router itself treats all nodes as OpenAI-compatible HTTP endpoints.

## Commit Format

Use conventional commits: `feat:`, `fix:`, `chore:`, `refactor:`, `docs:`, `test:`

Git default branch is `main`. Always `git branch -M main` after `git init`.

## Roadmap

See `ROADMAP.md`. Current priorities:
- Plugin system for custom routing strategies (v1.2+)
- Multi-node discovery (mDNS — full implementation)
- Distributed health consensus
- Multi-model consensus routing
- llama.cpp RPC integration for tensor-parallel sharding
