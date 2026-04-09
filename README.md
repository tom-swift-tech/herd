# 🦙 Herd

[![GitHub release](https://img.shields.io/github/v/release/swift-innovate/herd)](https://github.com/swift-innovate/herd/releases/latest)
[![GitHub stars](https://img.shields.io/github/stars/swift-innovate/herd?style=social)](https://github.com/swift-innovate/herd/stargazers)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.70+-orange.svg)](https://www.rust-lang.org)
[![Roadmap](https://img.shields.io/badge/roadmap-v1.1-blue)](ROADMAP.md)

**Intelligent LLM fleet router with GPU awareness, multi-backend support, and real-time observability.**

Route your llama herd with intelligence — Herd sits in front of your inference nodes (Ollama, llama-server, or any self-hosted OpenAI-compatible backend) and routes requests based on model availability, GPU load, and node health. One binary. One endpoint. Your entire fleet, unified.

> **New in v1.1:** TLS termination, per-client rate limiting, budget caps, routing profiles, Ollama blob extraction, and multi-node discovery. See [What's New in v1.1](#whats-new-in-v11).
>
> **v1.0:** llama-server (llama.cpp) as a first-class backend. Benchmarked at **44–80% faster TTFT** and **~4x throughput** vs Ollama on identical hardware. Herd is now vendor-agnostic across NVIDIA, AMD, and Intel GPUs. See [Backend Types](#backend-types) and the [benchmark data](docs/LLAMA_CPP_BACKEND.md).

<img width="1735" height="803" alt="image" src="https://github.com/user-attachments/assets/d625b30f-8110-482e-80cd-e3297a5ff428" />

## Built for AI agent swarms 🦞

Running multiple agents + parallel tool calls? Herd stops the GPU wars.
Point your agents at `http://your-herd:40114` — model homing + live VRAM routing keeps every request on the fastest available node.

Benchmarked on RTX 5090 with 4 concurrent agent requests: **41 seconds average TTFT via Ollama → 8 seconds via llama-server through Herd.** That's the difference between agents waiting and agents working.

## What's New in v1.1

### TLS Termination

Optional HTTPS via rustls, feature-gated behind `--features tls`. Falls back to HTTP gracefully on errors.

```yaml
tls:
  enabled: true
  cert_path: /path/to/cert.pem
  key_path: /path/to/key.pem
  redirect_http: true     # 301 redirect HTTP -> HTTPS
  redirect_port: 80
```

### Per-Client Rate Limiting

Per-API-key token buckets with response headers (`X-Herd-RateLimit-Limit`, `X-Herd-RateLimit-Remaining`, `X-Herd-RateLimit-Reset`). Returns HTTP 429 when exceeded. Falls back to global limit for unknown keys.

```yaml
rate_limiting:
  global: 100              # requests/sec for unknown clients
  clients:
    - name: my-agent
      api_key: sk-agent-12345
      rate_limit: 50
    - name: vip-client
      api_key: sk-vip-99999
      rate_limit: 0        # unlimited
```

### Budget Caps & Cost Tracking

Per-client and per-model budget limits in USD with daily/weekly/monthly reset. Action: `reject` (HTTP 429) or `warn` (log + allow). Query current spend via `GET /api/budget`.

```yaml
budget:
  enabled: true
  global_limit_usd: 50.00
  reset_period: monthly    # daily, weekly, monthly
  action: reject
  clients:
    agent-team: 20.00
  models:
    "llama3:70b": 30.00
```

### Routing Profiles

Named presets combining strategy, tags, backend filter, and preferred model. Clients select via the `X-Herd-Profile` request header. Manage via `GET /api/profiles` and `PUT /api/profiles/default`.

```yaml
routing_profiles:
  enabled: true
  default_profile: balanced
  profiles:
    fast:
      strategy: priority
    coding:
      strategy: model_aware
      preferred_model: "qwen2.5-coder:32b"
      tags: [gpu, high-vram]
```

### Ollama Blob Extraction

Extract raw GGUF files from Ollama's blob storage for reuse with llama-server. Symlinks on Unix, copies on Windows.

- `GET /api/ollama/models` -- list extractable models
- `POST /api/ollama/extract` -- extract GGUF from blob storage

### Multi-Node Discovery

Static fleet configuration with auto-probing. Define known node URLs and Herd probes them on an interval to register backends automatically. mDNS discovery is stubbed (feature-gated, not yet implemented).

```yaml
discovery:
  enabled: true
  probe_interval_secs: 60
  static_nodes:
    - url: http://192.168.1.100:8090
      backend: llama-server
      tags: [gpu, nvidia]
    - url: http://192.168.1.101:11434
      backend: ollama
```

## Getting Started

Herd is a reverse proxy that sits in front of your LLM inference backends, routing requests to the best available node based on model availability, GPU load, and priority.

### 1. Create a config file

```yaml
# herd.yaml — minimal setup
backends:
  - name: "my-gpu"
    url: "http://localhost:11434"
    priority: 100

routing:
  strategy: "model_aware"
```

### 2. Run Herd

```bash
cargo install herd
herd --config herd.yaml
```

### 3. Verify it works

```bash
curl http://localhost:40114/health          # → "OK"
curl http://localhost:40114/status          # → backend list + GPU info
curl http://localhost:40114/v1/models       # → available models across all backends
```

You're now routing through Herd. Point any OpenAI-compatible client at `http://localhost:40114`.

## Architecture

```
┌──────────────────────────────────────────────────────────┐
│                         Herd                              │
├──────────────────────────────────────────────────────────┤
│  ┌─────────┐  ┌──────────┐  ┌─────────────┐  ┌────────┐│
│  │  HTTP   │  │  Router  │  │   Circuit   │  │Metrics ││
│  │  Proxy  │→ │  Engine  │→ │   Breaker   │→ │& Telem ││
│  └─────────┘  └──────────┘  └─────────────┘  └────────┘│
│       ↓             ↓              ↓                     │
│  ┌─────────────────────────────────────────────────────┐│
│  │                  Backend Pool                        ││
│  │  ┌───────────┐  ┌───────────┐  ┌───────────┐       ││
│  │  │  node-a   │  │  node-b   │  │  node-c   │       ││
│  │  │  Ollama   │  │llama-svr  │  │  Ollama   │       ││
│  │  │  :11434   │  │  :8090    │  │  :11434   │       ││
│  │  │  CUDA     │  │  ROCm    │  │  CUDA     │       ││
│  │  └───────────┘  └───────────┘  └───────────┘       ││
│  └─────────────────────────────────────────────────────┘│
└──────────────────────────────────────────────────────────┘
```

## Backend Types

Herd supports three backend types per node:

| Backend | Config Value | Health Check | Model Discovery | keep_alive Injection |
|---------|-------------|-------------|-----------------|---------------------|
| **Ollama** | `ollama` (default) | `GET /` | `/api/tags` + `/api/ps` | Yes |
| **llama-server** | `llama-server` | `GET /health` | `/v1/models` | No (not applicable) |
| **OpenAI-compatible** | `openai-compat` | `GET /v1/models` | `/v1/models` | No |

The `openai-compat` type is for any **self-hosted** backend that exposes `/v1/` endpoints — vLLM, TGI, custom servers, etc. Herd does not connect to cloud APIs.

```yaml
backends:
  - name: "fast-node"
    url: "http://node-a:8090"
    backend: "llama-server"    # llama.cpp direct — fastest TTFT
    priority: 100

  - name: "easy-node"
    url: "http://node-b:11434"
    backend: "ollama"          # default if omitted
    priority: 80

  - name: "vllm-node"
    url: "http://node-c:8000"
    backend: "openai-compat"   # vLLM, TGI, etc.
    priority: 60
```

**Why llama-server?** Benchmarked on RTX 5090 (Gemma 4 26B MoE, Q4_K_M):

| Scenario | Ollama TTFT | llama-server TTFT | Improvement |
|----------|-------------|-------------------|-------------|
| Single-shot | 6,812 ms | 3,781 ms | -44% |
| Heavy system prompt | 14,331 ms | 5,574 ms | -61% |
| 4 concurrent clients | 40,980 ms | 8,282 ms | -80% |
| Average across all tests | — | — | **-62%** |

Ollama's Go serving layer is the bottleneck. llama-server talks directly to the GPU. Full benchmark data and methodology in [`docs/LLAMA_CPP_BACKEND.md`](docs/LLAMA_CPP_BACKEND.md).

**Backward compatible.** Existing Ollama-only configs continue to work unchanged. The `backend` field defaults to `ollama`.

## What Herd delivers (and what it doesn't)

**The real win for swarms is contention reduction, not cold-start elimination.**

With a thoughtful `hot_models` list and `model_aware` strategy, Herd eliminates the worst waits for your top 20–30 most-used models — routing every request to a backend where that model is already resident and warm. The remaining models still hit normal load times on first use; Herd can't conjure VRAM that isn't there.

What makes Herd more than DNS round-robin + client retry:

| | DNS round-robin | Herd |
|---|---|---|
| Knows which backend has the model loaded | ❌ | ✅ via model discovery |
| Routes by live GPU utilization | ❌ | ✅ via gpu-hot telemetry |
| Enforces `keep_alive` centrally | ❌ | ✅ injected on every Ollama request |
| Circuit-breaks failed nodes | ❌ | ✅ |
| Single place to add retries/failover | ❌ | ✅ |
| Routes across mixed backend types | ❌ | ✅ Ollama + llama-server + openai-compat |
| Token-level telemetry | ❌ | ✅ per-model, per-backend |

Herd is a **smart stateless proxy with a stateful routing cache**. It is not HA failover — in-flight requests are not redirected if a backend dies mid-stream. That's on the roadmap.

## Features

### Core Routing
- **Priority-based routing** — Route to the best GPU first
- **Model-aware routing** — Route to nodes with models already loaded
- **Weighted round-robin** — Distribute by priority weight
- **Least-busy routing** — Route to lowest GPU utilization
- **Tag-based routing** — Filter by `X-Herd-Tags` header
- **Circuit breaker** — Auto-recover from failed nodes
- **keep_alive injection** — Override `keep_alive` centrally; prevents clients from accidentally evicting models
- **Hot models warmer** — Declare `hot_models` per backend; Herd pre-loads and keeps them warm
- **Hot-reload config** — File watcher + `POST /admin/reload`
- **Rate limiting** — Global + per-client token-bucket rate limiter with `X-Herd-RateLimit-*` headers
- **Budget caps** — Per-client/per-model USD spending limits with configurable reset periods
- **Routing profiles** — Named presets (strategy + tags + backend filter) selected via `X-Herd-Profile` header
- **TLS termination** — Optional HTTPS via rustls (`--features tls`) with HTTP redirect
- **OpenAI-compatible endpoints** — Drop-in `/v1/chat/completions` for any client
- **Multi-backend** — Route across Ollama, llama-server, and openai-compat backends simultaneously
- **Auto-update** — `herd --update` or `POST /admin/update`

### Fleet Management
- **Node registration** — `herd-tune` scripts auto-enroll nodes with GPU detection
- **GPU vendor detection** — NVIDIA (nvidia-smi), AMD (rocm-smi), Intel (sycl-ls)
- **Blackwell detection** — Identifies RTX 5000-series GPUs requiring CUDA 13.x
- **llama-server provisioning** — herd-tune downloads the correct llama-server binary per GPU vendor
- **SQLite node registry** — Persistent fleet state with health polling
- **HuggingFace model search** — Search GGUF models with VRAM compatibility per-node
- **Model download** — Pull models to Ollama nodes via API (llama-server download in roadmap)
- **Ollama blob extraction** — Extract raw GGUF from Ollama blob storage for llama-server reuse
- **Multi-node discovery** — Static fleet config with auto-probing (mDNS stubbed, not yet active)

### Agent-Friendly
- **Agent sessions** — Create, resume, list, and delete sessions with message history and TTL
- **Built-in tool calling** — read_file, write_file, list_files, run_command
- **Permission engine** — Regex-based deny patterns for file and shell access
- **JSONL audit logging** — Full tool call and permission denial audit trail
- **WebSocket streaming** — Real-time agent events
- **Agent skills reference** — [`skills.md`](skills.md) teaches AI agents how to use Herd's API
- **Dashboard Agent Guide** — Built-in tab at `/dashboard` with endpoint tables and best practices
- **Correlation IDs** — `X-Request-Id` propagation for distributed agent tracing

### Observability & Telemetry
- **Prometheus metrics** — `/metrics` endpoint with request counters, backend gauges, latency histograms
- **Token counters** — `herd_tokens_total` by direction (in/out) and model
- **Tokens per second** — Exponential moving average gauge
- **Labeled latency** — Per-backend, per-model, per-status latency histograms (cardinality-capped at 200)
- **Request analytics** — JSONL logging with auto-retention
- **Interactive dashboard** — Real-time charts (Backends, Analytics, Sessions, Fleet, Models, Agent Guide, Settings)
- **GPU metrics** — Real-time VRAM, utilization, temperature via gpu-hot
- **Latency tracking** — P50, P95, P99 percentiles
- **Log rotation** — Size-based rotation with configurable retention
- **Task classification** — Keyword-based tier classification middleware with `X-Herd-Tier` header
- **Update checker** — Automatic GitHub release notifications

## Quick Start

```bash
# Install
cargo install herd

# Run with config
herd --config herd.yaml

# Or with CLI args
herd --port 40114 \
  --backend node-a=http://node-a:11434:100 \
  --backend node-b=http://node-b:11434:80 \
  --backend node-c=http://node-c:11434:50
```

## Using Herd with any LLM client

Herd works as a drop-in proxy for any Ollama or OpenAI-compatible client — Open WebUI, Continue.dev, Cursor, LiteLLM, custom scripts, whatever.

```bash
# Before: point directly at one node
OLLAMA_HOST=http://my-gpu:11434

# After: point at Herd instead — nothing else changes
OLLAMA_HOST=http://herd:40114
```

You get model-aware routing, circuit breakers, GPU telemetry routing, and centralized `keep_alive` enforcement with zero client changes. If you only have one GPU node today, Herd still centralizes `keep_alive` and gives you a dashboard — and you can add more backends later without touching your clients.

## herd-tune: Node Setup & Registration

herd-tune auto-detects your GPU hardware, sets up the inference backend, and registers with a Herd instance. It supports both Ollama and llama-server backends.

```bash
# Linux
curl -sf http://your-herd:40114/api/nodes/script?os=linux | bash

# Windows PowerShell
irm http://your-herd:40114/api/nodes/script?os=windows | iex
```

### Backend selection

```bash
# Auto-detect: use Ollama if running, otherwise set up llama-server
herd-tune --backend auto

# Explicitly use llama-server with a model
herd-tune --backend llama-server --model /path/to/model.gguf

# Explicitly use Ollama and apply recommended env vars
herd-tune --backend ollama --apply
```

### GPU detection

herd-tune detects GPU vendor and selects the correct llama-server binary:

| GPU | Detection | Binary Selected |
|-----|-----------|----------------|
| NVIDIA (Ampere, Ada) | `nvidia-smi` | CUDA 12.x |
| NVIDIA (Blackwell/5000-series) | `nvidia-smi` + compute capability | CUDA 13.x (required — 12.x silently falls back to CPU) |
| AMD (RDNA3+) | `rocm-smi` / `hipconfig` | ROCm/HIP |
| Intel (Arc) | `sycl-ls` | SYCL (beta) |
| Fallback | Any | Vulkan (universal, ~25% slower) |

### What herd-tune does

**Ollama backend:** Probes running Ollama instance, detects GPU, calculates VRAM-based config recommendations (num_parallel, max_loaded_models, context length, flash attention, kv cache type). With `--apply`, writes a systemd override and restarts the service.

**llama-server backend:** Detects GPU vendor, queries the latest llama.cpp release from GitHub, downloads the correct binary for your GPU (CUDA 12/13, ROCm, Vulkan), extracts it to `~/.herd/bin/`, generates a launch config at `~/.herd/llama-server.conf`, and prints the start command.

Both paths detect the best reachable IP (Tailscale > LAN > localhost) and register with the Herd host via `POST /api/nodes/register`.

## For AI Agents

Herd ships with built-in documentation for AI agents routed through it:

- **`GET /skills`** — JSON endpoint agents can fetch at startup for best practices, endpoints, headers, and error codes
- **[`skills.md`](skills.md)** — Complete API reference with examples
- **Dashboard Agent Guide** — The `/dashboard` includes an "Agent Guide" tab

```bash
# Agent self-onboarding: fetch skills at startup
curl http://herd:40114/skills | jq .best_practices
```

**Key things agents should know:**
1. Always specify `"model"` in requests for optimal routing
2. Use `"stream": true` for long responses
3. Send `X-Herd-Tags` to target specific backends
4. Send `X-Request-Id` for traceability across distributed systems
5. Query `GET /v1/models` to discover available models before requesting
6. Prefer native Ollama endpoints (`/api/chat`, `/api/generate`) over `/v1/chat/completions` — Herd routes both identically, but native endpoints bypass Ollama's OpenAI compat layer

## Configuration

```yaml
# herd.yaml
server:
  host: "0.0.0.0"
  port: 40114
  api_key: "your-secret-key"  # Required for admin API
  enrollment_key: "fleet-key" # Required for node registration (auto-generated if not set)
  rate_limit: 0               # Requests/sec (0 = unlimited)

routing:
  strategy: "model_aware"  # priority | model_aware | least_busy | weighted_round_robin
  timeout: 120s
  retry_count: 2
  default_keep_alive: "-1"  # inject into every Ollama request

model_warmer:
  interval_secs: 240       # ping hot_models every 4 min

backends:
  # llama-server backend — fastest TTFT
  - name: "fast-node"
    url: "http://node-a:8090"
    backend: "llama-server"
    priority: 100
    gpu_hot_url: "http://node-a:1312"
    tags: ["gpu", "fast"]

  # Ollama backend (default if backend omitted)
  - name: "easy-node"
    url: "http://node-b:11434"
    priority: 80
    hot_models:
      - "qwen2.5-coder:32b"

  # Small node with model filter
  - name: "small-node"
    url: "http://node-c:11434"
    priority: 50
    model_filter: "≤8B"

circuit_breaker:
  failure_threshold: 3
  timeout: 30s
  recovery_time: 60s

observability:
  metrics: true
  admin_api: true
  log_retention_days: 7
  log_max_size_mb: 100
  log_max_files: 5

agent:
  enabled: false             # Agent sessions, tool calling, permissions
```

## Choosing the Right Endpoint

> **Important:** Not all models work on all endpoints. Choose based on your model and client.

**Decision tree:**

1. **Using GLM, custom-template models, or any model that misbehaves?** → Use `/api/chat` (native Ollama). Bypasses Ollama's OpenAI compat layer.
2. **Need OpenAI SDK compatibility** (e.g., `openai.ChatCompletion.create`)? → Use `/v1/chat/completions`, but **test with your specific model first**.
3. **Single-turn generation?** → Use `/api/generate`.

`/api/chat` and `/api/generate` receive Herd's `keep_alive` injection. `/v1/*` endpoints do not. Only applies to Ollama backends.

## API Endpoints

### OpenAI-Compatible Endpoints

```bash
# Works with OpenAI SDK, Open WebUI, Continue.dev, LiteLLM, Cursor, etc.
base_url: http://herd:40114/v1
api_key: anything   # Ollama doesn't require auth; any value works
```

| Endpoint | Description |
|----------|-------------|
| `GET /v1/models` | List all models from healthy backends |
| `POST /v1/chat/completions` | Chat completions (streaming supported) |
| `POST /v1/completions` | Text completions (streaming supported) |

### Fleet & Model Management

| Endpoint | Description |
|----------|-------------|
| `POST /api/nodes/register` | Register a node (herd-tune) |
| `GET /api/nodes` | List all registered nodes |
| `GET /api/nodes/:id` | Get node details |
| `PUT /api/nodes/:id` | Update node priority/tags/enabled |
| `DELETE /api/nodes/:id` | Remove node |
| `GET /api/nodes/:id/models` | List models on a node |
| `DELETE /api/nodes/:id/models/:model_name` | Delete model from Ollama node |
| `POST /api/nodes/:id/models/download` | Pull model to Ollama node |
| `GET /api/models/search?q=...` | Search HuggingFace for GGUF models |

### Correlation IDs

Every request gets an `X-Request-Id` header for end-to-end tracing:

```bash
# Herd generates a UUID v4 if you don't provide one
curl http://localhost:40114/v1/chat/completions -d '...'
# Response includes: X-Request-Id: 550e8400-e29b-41d4-a716-446655440000

# Or provide your own
curl -H "X-Request-Id: my-trace-123" http://localhost:40114/api/generate -d '...'
```

Request IDs are included in JSONL analytics logs for correlation across systems.

### All Endpoints

| Endpoint | Description |
|----------|-------------|
| `GET /` | Proxy to highest priority backend |
| `POST /api/*` | Forward Ollama API requests |
| `GET /v1/models` | OpenAI-compatible model list |
| `POST /v1/chat/completions` | OpenAI-compatible chat (streaming) |
| `GET /dashboard` | Interactive analytics dashboard |
| `GET /status` | Node health + GPU metrics (JSON) |
| `GET /analytics?hours=N` | Request stats, latency, timeline |
| `GET /update` | Check for new releases |
| `GET /metrics` | Prometheus metrics |
| `GET /health` | K8s liveness probe |
| `GET /skills` | Agent skills JSON |
| `POST /admin/backends` | Add backend at runtime |
| `GET /admin/backends/:name` | Get backend details |
| `PUT /admin/backends/:name` | Update backend config |
| `DELETE /admin/backends/:name` | Remove backend |
| `POST /admin/reload` | Hot-reload config file |
| `POST /admin/update` | Self-update from GitHub Releases |
| `GET /admin/config` | View running config (secrets redacted) |
| `PUT /admin/config` | Update config via API |
| `GET /api/budget` | Current budget spend and limits |
| `GET /api/profiles` | List routing profiles |
| `PUT /api/profiles/default` | Set default routing profile |
| `GET /api/ollama/models` | List extractable Ollama models |
| `POST /api/ollama/extract` | Extract GGUF from Ollama blob storage |

## Telemetry & Prometheus Metrics

Herd exposes rich telemetry at `GET /metrics`:

```
# Request totals by status and backend
herd_requests_total{status="success"} 15230
herd_requests_by_backend{backend="fast-node"} 12030

# Token counters by direction and model
herd_tokens_total{direction="in", model="qwen2.5-coder:32b"} 892000
herd_tokens_total{direction="out", model="qwen2.5-coder:32b"} 431000

# Tokens per second (exponential moving average)
herd_tokens_per_second 145.23

# Latency histogram (global)
herd_request_duration_ms_bucket{le="100"} 8923
herd_request_duration_ms_bucket{le="1000"} 14200

# Labeled latency (per backend/model/status, cardinality-capped at 200)
herd_request_duration_labeled_ms_count{backend="fast-node", model="qwen2.5-coder:32b", status="success"} 4310

# Routing strategy selections
herd_routing_selections_total{backend="fast-node", strategy="model_aware"} 12030
```

Wire these into Grafana, Datadog, or any Prometheus-compatible monitoring stack.

## Analytics & Dashboard

Access the interactive dashboard at `http://your-herd:40114/dashboard`

**Tabs:**
- **Backends** — Real-time node status with GPU metrics
- **Analytics** — Request volume, top models, latency percentiles
- **Sessions** — Agent session management
- **Fleet** — Registered nodes, GPU badges, health status
- **Models** — HuggingFace GGUF search with VRAM compatibility
- **Agent Guide** — API reference and best practices for AI agents
- **Settings** — Config editor with secret redaction

### Request Logging

All proxied requests are logged to `~/.herd/requests.jsonl`:

```json
{"timestamp":1709395200,"model":"qwen2.5-coder:32b","backend":"fast-node","duration_ms":234,"status":"success","path":"/api/generate","request_id":"550e8400-...","tier":"heavy","classified_by":"keyword"}
```

**Log management:**
- Logs older than `log_retention_days` (default 7) are pruned daily at 3 AM
- Log files are rotated when they exceed `log_max_size_mb` (default 100 MB)
- Up to `log_max_files` (default 5) rotated files are kept

### Analytics API

Query statistics programmatically:

```bash
# Last 24 hours (default)
curl http://localhost:40114/analytics

# Last hour
curl http://localhost:40114/analytics?hours=1

# Response
{
  "total_requests": 1523,
  "latency_p50": 145,
  "latency_p95": 892,
  "latency_p99": 1204,
  "model_counts": {
    "qwen2.5-coder:32b": 892,
    "llama3.1:8b": 431
  },
  "backend_counts": {
    "fast-node": 1203,
    "easy-node": 320
  },
  "timeline": [[1709395200, 45], [1709395260, 52], ...]
}
```

## Hot Models & keep_alive

Herd solves model eviction centrally. Ollama unloads models after 5 minutes by default, and clients like Open WebUI often send `"keep_alive": "5m"` which overrides any node-level env var. Herd fixes this at the proxy layer.

### keep_alive Injection

```yaml
routing:
  default_keep_alive: "-1"   # never unload; set on every Ollama request
```

Herd injects this into every `/api/generate` and `/api/chat` request body, overriding whatever the client sent. Only applies to Ollama backends.

### Hot Models Warmer

```yaml
model_warmer:
  interval_secs: 240   # ping every 4 min; safely under Ollama's 5-min eviction window

backends:
  - name: "node-a"
    url: "http://node-a:11434"
    hot_models:
      - "qwen2.5-coder:32b"
      - "llama3:8b"
```

Pre-loads declared models on startup and re-loads them after OOM eviction by sending a minimal `keep_alive: "-1"` ping on every interval.

## GPU Awareness

Herd integrates with [gpu-hot](https://github.com/psalias2006/gpu-hot) for real-time metrics:

```bash
# On each GPU node
docker run -d --gpus all -p 1312:1312 \
  -e NODE_NAME=node-a \
  ghcr.io/psalias2006/gpu-hot:latest
```

Then configure Herd to query metrics:

```yaml
backends:
  - name: "node-a"
    url: "http://node-a:11434"
    gpu_hot_url: "http://node-a:1312"
```

**Dashboard GPU section:**
- Displays per-GPU cards with utilization, temperature, memory, power draw
- Auto-polls every 10 seconds
- Automatically hides if gpu-hot is unreachable
- Shows all GPUs on multi-GPU nodes

Herd routes based on model already loaded (via model discovery), GPU VRAM available, and current utilization.

## GPU Vendor Support

Herd + herd-tune support multi-vendor GPU fleets:

| GPU Vendor | Backend | Maturity | Notes |
|------------|---------|----------|-------|
| NVIDIA (Ampere, Ada, Blackwell) | CUDA | Production | Blackwell (5000-series) requires CUDA 13.x |
| AMD Radeon (RDNA3+) | ROCm/HIP | Solid | Pre-built binaries, validated by AMD |
| AMD Instinct (MI300X, MI325X) | ROCm/HIP | Production | Official AMD Docker images |
| Intel Arc (A/B-series) | SYCL | Beta | Community-maintained |
| Any (fallback) | Vulkan | Functional | ~25% slower, zero vendor-specific setup |

## Auto-Update

Herd can update itself from GitHub Releases:

```bash
# CLI: check and install update
herd --update

# API: trigger update remotely (requires API key)
curl -X POST -H "X-API-Key: your-key" http://localhost:40114/admin/update

# Check without installing (no auth required)
curl http://localhost:40114/update
```

On startup, Herd checks for updates in the background and logs a notification if a newer version is available. After updating, the server must be restarted to run the new version. The previous binary is kept as a backup for rollback.

## Comparison

| Feature | Herd | LiteLLM Proxy | Ollama (standalone) |
|---------|------|--------------|-------------------|
| Multi-backend routing | ✅ Ollama + llama-server + openai-compat | ✅ 100+ providers | ❌ Single node |
| GPU-aware routing | ✅ VRAM + utilization | ❌ | ❌ |
| Model-aware routing | ✅ Routes to loaded model | ❌ | ❌ |
| keep_alive injection | ✅ | ❌ | ❌ (env var only) |
| Hot models warmer | ✅ | ❌ | ❌ |
| Circuit breaker | ✅ | ✅ | ❌ |
| Agent sessions + tools | ✅ | ❌ | ❌ |
| Fleet auto-registration | ✅ herd-tune | ❌ | ❌ |
| HuggingFace model search | ✅ | ❌ | ❌ |
| Self-hosted, no cloud | ✅ | ✅ | ✅ |
| Token-level telemetry | ✅ | ✅ | ❌ |
| Single binary (Rust) | ✅ | ❌ (Python) | ❌ (Go) |
| TLS termination | ✅ (feature-gated) | ✅ | ❌ |
| Per-client rate limiting | ✅ | ✅ | ❌ |
| Budget caps | ✅ | ✅ | ❌ |
| Routing profiles | ✅ | ❌ | ❌ |
| Multi-node discovery | ✅ | ❌ | ❌ |
| Self-update | ✅ | ❌ | ❌ |

## License

MIT

## Support

If Herd is useful to you, consider sponsoring development:

[![GitHub Sponsors](https://img.shields.io/github/sponsors/swift-innovate?style=social)](https://github.com/sponsors/swift-innovate)

---

**Herd your llamas with intelligence.** 🦙
