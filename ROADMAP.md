# Herd Roadmap

**Updated:** June 12, 2026

## Vision

Herd is the **fastest way to route AI workloads across local inference backends**.

One fast, single Rust binary gives you:
- GPU-aware routing across multiple inference nodes (llama-server, Ollama, or any OpenAI-compatible backend)
- Circuit breaker resilience with configurable failure thresholds
- Unified observability: metrics, analytics, and a live dashboard
- OpenAI-compatible API for drop-in compatibility
- Fleet mode: one host orchestrates multiple GPU nodes across hardware vendors (NVIDIA, AMD, Intel)

No cloud dependency. No API keys exposed. Full local control.

## Roadmap

### v0.2.1 — Security Hardening

- Configurable circuit breaker (failure threshold, recovery time)
- API key authentication for admin endpoints
- Proxy hardening (body size cap, header forwarding, query string preservation)
- Analytics race condition fix
- CLI backend specification parser
- Conditional route registration (admin API off by default)

### v0.3.0 — Routing & Reliability ✅

- ~~Retry loop with configurable attempt count~~ ✅ (shipped v0.2.1)
- ~~Request timeout enforcement per routing strategy~~ ✅ (shipped v0.2.1)
- ~~Weighted round-robin routing strategy~~ ✅
- ~~OpenAI `/v1/chat/completions` full compatibility layer~~ ✅ (pulled forward from v0.4.0)
- ~~Rate limiting (global token bucket)~~ ✅ (pulled forward from v0.5.0)
- ~~Model filter (regex-based per-backend)~~ ✅
- ~~Dashboard polish (stats, tabs, latency percentiles, mobile responsive)~~ ✅
- ~~Backend tagging and tag-based routing~~ ✅
- ~~Health check endpoint customization (configurable path and expected status)~~ ✅
- ~~Hot-reload configuration without restart~~ ✅ (file polling + POST /admin/reload)

### v0.4.0 — Observability & Operations ✅ (v0.4.1)

- ~~Prometheus-native metrics export~~ ✅ (in-memory counters + histogram, `/metrics` endpoint)
- ~~Request tracing with correlation IDs~~ ✅ (X-Request-Id propagation + UUID v4 generation)
- ~~Log rotation and retention policies~~ ✅ (size-based rotation, configurable retention days)
- ~~Auto-update from GitHub Releases~~ ✅ (`herd --update`, `POST /admin/update`)
- ~~GitHub Actions CI/CD~~ ✅ (test on 3 platforms, release builds for 5 targets)
- ~~Graceful config error handling~~ ✅ (v0.4.1 — warn+disable instead of crash)

### v0.4.3 — Keep-Alive & Hot Models (Breaking)

> **Breaking changes:** `default_model` and `routing.idle_timeout_minutes` are removed. See README migration guide.

- `keep_alive` injection — override `keep_alive` on every proxied Ollama request centrally; prevents clients (Open WebUI, LiteLLM, agents) from accidentally evicting models
- Hot models warmer — `hot_models: [...]` per backend; background warmer pings every 4 min with `keep_alive: "-1"` for pre-load and OOM recovery
- Removes `ModelHoming` and `default_model` — superseded by `hot_models` + proxy injection

### v0.5.0 — Task Classification ✅

- Keyword-based task tier classification middleware
- Automatic model selection by complexity tier (heavy/standard/lightweight)
- `X-Herd-Tier` response header on classified requests
- Analytics logging with `tier` and `classified_by` fields
- Off by default — zero overhead when disabled

### v0.9.0 — Herd Pro Merge (Unified Release) ✅

> **Herd Pro features merged into the public repo. Herd Pro is now archived.**

- Agent session management (create, list, resume, delete with message history and TTL)
- Built-in tool calling (read_file, write_file, list_files, run_command)
- Permission engine — regex-based deny patterns for file and shell access
- JSONL audit logging for tool calls and permission denials
- WebSocket streaming for real-time agent events
- Node registration — herd-tune scripts for auto-enrolling Ollama nodes
- Fleet management — SQLite node registry, health polling, dashboard Fleet tab
- Enrollment key authentication for node registration
- Dashboard: Sessions, Fleet, and Settings (config editor) tabs
- Config editor API (`GET/PUT /admin/config`) with secret redaction

### v1.0.0 — llama.cpp Backend & Multi-Vendor Fleet ✅

> **Strategic shift:** Benchmarking validated that Ollama's Go layer adds 45-80% TTFT overhead vs raw llama-server. Herd v1.0 adds llama-server as a first-class backend, making Herd vendor-agnostic across NVIDIA, AMD, and Intel GPUs. See `docs/LLAMA_CPP_BACKEND.md` for full analysis and benchmark data.

- ~~**llama-server backend support** — route to llama-server (llama.cpp) endpoints alongside Ollama~~ ✅
- ~~**Backend type field** — `backend: "ollama" | "llama-server" | "openai-compat"` per node in config and registry~~ ✅
- ~~**herd-tune GPU detection** — auto-detect NVIDIA (nvidia-smi), AMD (rocm-smi), Intel (sycl-ls) and select correct llama-server binary~~ ✅
- ~~**herd-tune binary provisioning** — download and verify correct llama-server build (CUDA 12/13, ROCm, SYCL, Vulkan fallback)~~ ✅
- ~~**Blackwell detection** — CUDA 13.x required for RTX 5000-series; CUDA 12.x silently falls back to CPU (critical herd-tune check)~~ ✅
- ~~**Extended node registration** — `gpu_vendor`, `gpu_backend`, `cuda_version`, `backend_version`, `capabilities` fields~~ ✅
- ~~**Model search CLI** — `herd search <query>` for HuggingFace GGUF discovery (inspired by Fox engine UX)~~ ✅ (API endpoint, CLI wrapper deferred)
- ~~**Model download with resume** — robust GGUF pull with partial download tracking~~ ✅ (DB tracking ready, Ollama pull proxied)
- **Ollama blob extraction** — reuse existing Ollama models by extracting raw GGUF from blob storage *(shipped in v1.1.0)*
- ~~**Health check abstraction** — backend-aware health probes (Ollama `/api/ps` vs llama-server `/health`)~~ ✅
- ~~Backward compatible — existing Ollama-only configs continue to work unchanged~~ ✅
- ~~**Telemetry enrichment** — token counts, per-model/backend latency, cost estimation, Prometheus metrics~~ ✅
- ~~**Dashboard control plane** — Models tab with HF search, Fleet GPU badges, analytics visualizations~~ ✅
- ~~**HuggingFace model search API** — search, download, VRAM compatibility~~ ✅

### v1.1.2 — Frontier Gateway Enforcement ✅

- ~~Per-provider rate limiting (fixed-window token bucket, `rate_limit` requests/minute from `ProviderConfig`)~~ ✅
- ~~Rate-limited requests return `429 Too Many Requests`~~ ✅
- ~~Automatic cost recording after successful non-streaming frontier responses~~ ✅
- ~~`X-Herd-Cost-Estimate` response header with per-request USD cost~~ ✅
- ~~Cost DB hot-reload support: rate limiter + provider config rebuild on `/admin/reload`~~ ✅
- ~~Streaming responses (`stream: true`) pass through unchanged — SSE cost parsing deferred~~ ✅

### v1.1.1 — Sprint 3 Integration ✅

- ~~Auto-mode → frontier gateway escalation in OpenAI-compat handler (`/v1/chat/completions`)~~ ✅
- ~~Shared `frontier_route_if_applicable` helper for both proxy paths~~ ✅
- ~~`allow_auto_escalation` gate prevents unintended cloud requests when classifier returns `tier: "frontier"` but escalation is disabled~~ ✅
- ~~Auto-mode `X-Herd-Auto-*` headers emitted on escalated responses~~ ✅
- ~~Fallback to `auto.fallback_model` when gateway declines to handle a frontier-tier classification~~ ✅

### v1.1.0 — Scale & Security ✅

- ~~TLS termination (optional HTTPS via rustls, feature-gated `--features tls`)~~ ✅
- ~~Per-client rate limiting (per-API-key token buckets with `X-Herd-RateLimit-*` headers)~~ ✅
- ~~Budget caps and cost tracking (per-client/per-model USD limits, daily/weekly/monthly reset)~~ ✅
- ~~Routing profiles (named presets selected via `X-Herd-Profile` header)~~ ✅
- ~~Ollama blob extraction (extract raw GGUF from Ollama blob storage for llama-server reuse)~~ ✅
- ~~Multi-node discovery (static fleet config with auto-probe)~~ ✅
- ~~**Auto Mode classifier** — LLM-based request classification when `"model": "auto"` or model omitted; classifies tier (light/standard/heavy/frontier) and capability (general/code/reasoning/creative/vision/extraction), routes to best model from configurable map; results cached by message hash; off by default~~ ✅

### v1.2.0+ — Distributed Inference (In Progress)

> **See `docs/specs/v2-distributed-inference-spec.md`** for the full architecture.

Three-phase delivery introduces self-registering node agents and deployment-aware routing:

- **v1.2** — Agent/Gateway foundation. `herd agent` subcommand, `NodeRegistry`, single-node deployments. Sprint plan: `tasks/HERD-V1.2-SPRINT.md`. *Status: PRs #1–#6c landed (fleet foundation: `Deployment` module, `NodeRegistry` with TTL eviction, gateway heartbeat endpoint (`HERD_AGENT_TOKEN` auth), `herd agent` daemon (GPU/VRAM detection, local backend probe, 2s heartbeat with backoff), agent node persistence (migration v5 `source`/`agent_version`, write-through on transitions, soft-evict + reaper, Fleet tab reads unified SQLite store), gateway version authority with sha256-verified download offers (#6a), agent self-update with verify-before-swap and eviction grace (#6b), and `herd publish` promote helper (#6c — `herd publish [BINARY] --version <V> [--os --arch --publish-dir --config --force]`, copies binary into `{publish_dir}/{version}/{os}-{arch}/herd[.exe]`, prints sha256, refuses overwrite of differing bytes without `--force`), and `BackendPool` routing integration (#7 — `AgentPoolSync` mirrors fresh agent nodes from the in-memory `NodeRegistry` into the pool under the `agent:{node_id}` prefix so they route identically to static/enrolled backends; drains to 503 when all backends are gone; enrolled+agent on one host coexist as two entries, dedup deferred to v1.3), and the end-to-end integration test (#8 — `tests/fleet_routing.rs` proves the whole chain in one process over real HTTP: heartbeat → registry → reconcile → pool → router → proxy → stub upstream, with a routed 200, a drain→503 driven by an injected clock, and model-targeted routing). **v1.2 foundation complete** — all PRs #1–#8 landed. Two-box cross-machine reachability remains documented manual acceptance.*
- **v1.2 smart-routing scorer** — a fifth routing strategy (`scored`) that ranks backends by a weighted, multi-dimensional score (GATE → SCORE → SELECT) instead of a single axis. Full design (23 dimensions, 4-phase rollout): `docs/specs/smart-routing-scorer-spec.md`. *Status: Phase 0 (telemetry to the pool boundary — `BackendState` gains `queue_depth`/`ttft_p50_ms`/`vram_free_mb`/`max_concurrent` `Option` fields, populated from `AgentCapabilities` for `agent:` entries by `pool_sync::reconcile`; static/enrolled stay `None`) in review as PR A. Phase 1 (the `ScoredRouter` over pool+request dimensions) follows on a separate PR off updated main. Phases 2–4 (agent telemetry, history/EWMA, locality/cost/capability) documented for later.*
- **v1.3** — Speculative decoding deployments. Draft/verifier pairs across nodes via llama.cpp's `--model-draft` for 2-3x throughput on daily-driver models.
- **v1.4** — Pipeline parallel deployments. llama.cpp RPC integration to serve models that don't fit on any single GPU (Qwen2.5-72B-class).

The following items were originally listed alongside v1.2 but are **not part of the v1.2 foundation scope** — they target v1.3 and later:

- Multi-node discovery (mDNS — full implementation) *(v1.3+)*
- Plugin system for custom routing strategies *(v1.3+)*
- Distributed health consensus *(v1.4+)*
- Multi-model consensus routing *(v1.4+)*

## Get Involved

If you're interested in:
- Testing pre-release builds
- Contributing routing strategies or backend integrations
- Sharing real-world deployment patterns

...please open an issue or discussion.

— swift-innovate
