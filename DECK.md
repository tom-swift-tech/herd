# Herd

## Purpose
Intelligent LLM router — a single-binary reverse proxy for local LLM backends. Routes AI workloads across local GPU nodes with model awareness, health tracking, circuit breakers, agent sessions, and observability. OpenAI-compatible API.

## Architecture
Rust (Axum 0.7, Tokio). Supports three backend types: Ollama, llama-server (llama.cpp), and any OpenAI-compatible endpoint. Four pluggable routing strategies (priority, model_aware, least_busy, weighted_round_robin). SQLite node registry. JSONL analytics. YAML config with hot-reload. Dashboard at `/dashboard`. Agent session management with tool execution and WebSocket support. Task-based tier classifier (manual or LLM-auto mode).

Key insight: Herd doesn't do inference. It routes requests. GPU vendor complexity (CUDA/ROCm/Vulkan) is pushed to node setup via herd-tune. All nodes are treated as OpenAI-compatible HTTP endpoints.

## Conventions
- All new features default to `enabled: false` — zero overhead when not opted in.
- New config fields must have sensible defaults and not break existing `herd.yaml` files.
- Never `bail!` on config errors — degrade gracefully, warn and disable.
- Backend-agnostic routing: routing logic works identically for all backend types.
- All public-facing headers use `X-Herd-` prefix.
- New endpoints must appear in `skills.md` and the dashboard Agent Guide tab.
- 258 tests. Conventional commits. Default branch `main`.
- See `CLAUDE.md` for full module map, build commands, and design docs.

## State
v1.1.0 stable. Herd Pro merged into public repo as of v0.9.0. llama-server backend benchmarked at 44-80% faster TTFT and ~4x throughput vs Ollama on RTX 5090. Roadmap priorities: plugin system for custom routing (v1.2+), mDNS multi-node discovery, distributed health consensus, llama.cpp RPC tensor-parallel sharding.
