# 🦙 Herd

[![GitHub release](https://img.shields.io/github/v/release/swift-innovate/herd)](https://github.com/swift-innovate/herd/releases/latest)
[![GitHub stars](https://img.shields.io/github/stars/swift-innovate/herd?style=social)](https://github.com/swift-innovate/herd/stargazers)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.70+-orange.svg)](https://www.rust-lang.org)
[![Roadmap](https://img.shields.io/badge/roadmap-v0.9%20unified%20release-blue)](ROADMAP.md)

## Built for OpenClaw swarms 🦞
Running multiple agents + parallel tool calls? Herd stops the GPU wars.
Just set your OpenClaw `baseUrl` to `http://your-herd:40114` — done.
Model homing + live VRAM routing keeps every agent on the fastest node.

> **Pro tip:** Point your OpenClaw agents at Herd and they instantly become GPU-smart — live VRAM routing, model-aware routing to already-loaded backends, and parallel tool calls across your whole swarm without GPU wars.
<img width="1735" height="803" alt="image" src="https://github.com/user-attachments/assets/d625b30f-8110-482e-80cd-e3297a5ff428" />



**Intelligent Ollama router with GPU awareness, analytics, and real-time monitoring.**

Route your llama herd with intelligence — priority routing, circuit breakers, model awareness, real-time GPU metrics, beautiful dashboards, and OpenAI-compatible endpoints.

## Getting Started

Herd is a reverse proxy that sits in front of your Ollama instances, routing requests to the best available backend based on model availability, GPU load, and priority.

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

You're now routing through Herd. Point any OpenAI-compatible client or Ollama agent at `http://localhost:40114`.

### Architecture

```
┌─────────────────────────────────────────────────┐
│                    Herd                          │
├─────────────────────────────────────────────────┤
│  ┌─────────┐  ┌─────────┐  ┌─────────────┐     │
│  │  HTTP   │  │ Router  │  │   Circuit   │     │
│  │  Proxy  │→ │ Engine  │→ │   Breaker   │     │
│  └─────────┘  └─────────┘  └─────────────┘     │
│       ↓            ↓              ↓              │
│  ┌────────────────────────────────────────┐     │
│  │            Backend Pool                │     │
│  │  ┌─────────┐ ┌─────────┐ ┌─────────┐  │     │
│  │  │ Node A  │ │ Node B  │ │ Node C  │  │     │
│  │  │ :11434  │ │ :11434  │ │ :11434  │  │     │
│  │  │ :1312   │ │ :1312   │ │ :1312   │  │     │
│  │  └─────────┘ └─────────┘ └─────────┘  │     │
│  └────────────────────────────────────────┘     │
└─────────────────────────────────────────────────┘
```

## What Herd delivers (and what it doesn't)

**The real win for swarms is contention reduction, not cold-start elimination.**

With a thoughtful `hot_models` list and `model_aware` strategy, Herd eliminates the worst waits for your top 20–30 most-used models — routing every request to a backend where that model is already resident and warm. The remaining models still hit Ollama's normal 20–60 s load time on first use or after an eviction cascade; Herd can't conjure VRAM that isn't there.

What makes Herd more than DNS round-robin + client retry:

| | DNS round-robin | Herd |
|---|---|---|
| Knows which backend has the model loaded | ❌ | ✅ via `/api/ps` polling |
| Routes by live GPU utilization | ❌ | ✅ via gpu-hot telemetry |
| Enforces `keep_alive` centrally | ❌ | ✅ injected on every request |
| Circuit-breaks failed nodes | ❌ | ✅ |
| Single place to add retries/failover | ❌ | ✅ |

Herd is a **smart stateless proxy with a stateful routing cache**. It is not HA failover — in-flight requests are not redirected if a backend dies mid-stream. That's on the roadmap.

## Features

### Core Routing
- **Priority-based routing** — Route to the best GPU first
- **Model-aware routing** — Route to nodes with models already loaded
- **Weighted round-robin** — Distribute by priority weight
- **Least-busy routing** — Route to lowest GPU utilization
- **Tag-based routing** — Filter by `X-Herd-Tags` header
- **Model filtering** — Restrict backends by model size (e.g. `≤8B`) or name pattern
- **Task classification** — Automatic model selection by complexity tier (heavy/standard/lightweight)
- **Circuit breaker** — Auto-recover from failed nodes
- **keep_alive injection** — Override `keep_alive` on every Ollama request centrally; prevents clients from accidentally evicting models
- **Hot models warmer** — Declare `hot_models` per backend; Herd pre-loads and keeps them warm automatically
- **Hot-reload config** — File watcher + `POST /admin/reload`
- **Rate limiting** — Global token-bucket rate limiter
- **OpenAI-compatible** — Drop-in `/v1/chat/completions` endpoint
- **Auto-update** — `herd --update` or `POST /admin/update`

### Agent Sessions & Tool Calling (v0.9.0)
- **Agent session management** — Create, list, resume, and delete sessions with full message history and configurable TTL
- **Built-in tool calling** — `read_file`, `write_file`, `list_files`, `run_command` with automatic tool-call loop
- **Permission engine** — Regex-based deny patterns for file and shell access
- **Audit logging** — JSONL audit trail for every tool call and permission denial
- **WebSocket streaming** — Real-time agent events over WebSocket

### Fleet Management (v0.9.0)
- **Node registration** — `herd-tune` scripts for auto-enrolling Ollama nodes into the fleet
- **SQLite node registry** — Persistent fleet state with health polling
- **Enrollment key authentication** — Secure node registration
- **Config editor API** — `GET/PUT /admin/config` with automatic secret redaction
- **Dashboard Fleet tab** — Visual fleet management with node status

### Agent-Friendly
- **Agent skills reference** — [`skills.md`](skills.md) teaches AI agents how to use Herd's API, routing, and headers
- **Dashboard Agent Guide** — Built-in tab at `/dashboard` with endpoint tables, best practices, and error handling
- **OpenAI drop-in** — Point any agent's `base_url` to Herd and it just works
- **Correlation IDs** — `X-Request-Id` propagation for distributed agent tracing

### Observability
- **Prometheus metrics** — `/metrics` endpoint with request counters, backend gauges, and latency histogram
- **Log rotation** — Size-based rotation with configurable retention (days, max size, max files)
- **Request analytics** — JSONL logging with auto-retention
- **Interactive dashboard** — Real-time charts with Chart.js (Backends, Analytics, Sessions, Fleet, Settings, Agent Guide tabs)
- **GPU metrics** — Real-time VRAM, utilization, temperature
- **Latency tracking** — P50, P95, P99 percentiles
- **Update checker** — Automatic GitHub release notifications

## Quick Start

```bash
# Install
cargo install herd

# Run with config
herd --config herd.yaml

# Or with CLI args
herd --port 40114 \
  --backend node-a=http://gpu-1:11434:100 \
  --backend node-b=http://gpu-2:11434:80 \
  --backend node-c=http://gpu-3:11434:50
```

## For AI Agents

Herd ships with built-in documentation for AI agents routed through it:

- **`GET /skills`** — JSON endpoint agents can fetch at startup for best practices, endpoints, headers, and error codes. Self-service onboarding.
- **[`skills.md`](skills.md)** — Complete API reference with examples. Point your agent at this file for the full guide.
- **Dashboard Agent Guide** — The `/dashboard` includes an "Agent Guide" tab with endpoint tables, do/don't checklists, and error handling.

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
6. Prefer native Ollama endpoints (`/api/chat`, `/api/generate`) over `/v1/chat/completions` — Herd routes both identically, but the native endpoints bypass Ollama's OpenAI compat layer and work reliably with all models

## Using Herd with any Ollama client

Herd is built for OpenClaw swarms, but it works as a drop-in proxy for any Ollama client or tool — Open WebUI, Continue.dev, Cursor, LiteLLM, custom scripts, whatever.

```bash
# Before: point directly at one Ollama node
OLLAMA_HOST=http://my-gpu:11434

# After: point at Herd instead — nothing else changes
OLLAMA_HOST=http://herd:40114
```

You get model-aware routing, circuit breakers, GPU telemetry routing, and centralized `keep_alive` enforcement with zero client changes. If you only have one GPU node today, Herd still centralizes `keep_alive` and gives you a dashboard — and you can add more backends later without touching your clients.

## Configuration

```yaml
# herd.yaml
server:
  host: "0.0.0.0"
  port: 40114
  api_key: "your-secret-key"  # Required for admin API
  rate_limit: 0               # Requests/sec (0 = unlimited)

routing:
  strategy: "model_aware"  # priority | model_aware | least_busy | weighted_round_robin
  timeout: 120s
  retry_count: 2
  default_keep_alive: "-1"  # inject into every Ollama request (v0.4.3+)

model_warmer:              # v0.4.3+: replaces model_homing
  interval_secs: 240       # ping hot_models every 4 min

backends:
  - name: "node-a"
    url: "http://gpu-1:11434"
    priority: 100
    gpu_hot_url: "http://gpu-1:1312"
    tags: ["gpu", "high-vram"]
    health_check_path: "/api/version"
    hot_models:
      - "qwen2.5-coder:32b"

  - name: "node-b"
    url: "http://gpu-2:11434"
    priority: 80
    gpu_hot_url: "http://gpu-2:1312"
    tags: ["gpu", "medium-vram"]
    hot_models:
      - "glm-4.7-flash:latest"

  - name: "node-c"
    url: "http://gpu-3:11434"
    priority: 50
    model_filter: "≤8B"  # Only route small models to this backend
    tags: ["gpu", "low-vram"]

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
```

## Choosing the Right Endpoint

> **Important:** Not all models work on all endpoints. Choose based on your model and client.

**Decision tree:**

1. **Using GLM, custom-template models, or any model that misbehaves?** → Use `/api/chat` (native Ollama). This bypasses Ollama's OpenAI compatibility layer and works reliably with all models.
2. **Need OpenAI SDK compatibility** (e.g., `openai.ChatCompletion.create`)? → Use `/v1/chat/completions`, but **test with your specific model first**. Some models hang or error on this endpoint.
3. **Single-turn generation?** → Use `/api/generate`.

`/api/chat` and `/api/generate` also receive Herd's `keep_alive` injection. `/v1/*` endpoints do not.

## API Endpoints

### OpenAI-Compatible Endpoints

Point any OpenAI client at Herd and get full model-aware routing across your cluster:

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

All `/v1/*` requests use the same intelligent routing as native Ollama calls — model-aware, priority-based, with circuit breakers.

> **Note:** `/v1/chat/completions` is forwarded to Ollama's OpenAI compatibility layer on the backend. Most models work fine, but some models (particularly those with custom chat templates like GLM) may hang or return errors on this endpoint while working correctly on native `/api/chat`. If a model behaves unexpectedly via `/v1/chat/completions`, switch to `/api/chat` — Herd proxies it identically, with full routing and circuit-breaker support.

### Correlation IDs

Every request gets an `X-Request-Id` header for end-to-end tracing:

```bash
# Herd generates a UUID v4 if you don't provide one
curl http://localhost:40114/v1/chat/completions -d '...'
# Response includes: X-Request-Id: 550e8400-e29b-41d4-a716-446655440000

# Or provide your own — Herd forwards it to the upstream backend
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
| `GET /skills` | Agent skills JSON (endpoints, headers, best practices) |
| `POST /admin/backends` | Add backend at runtime |
| `GET /admin/backends/:name` | Get backend details |
| `PUT /admin/backends/:name` | Update backend config |
| `DELETE /admin/backends/:name` | Remove backend |
| `POST /admin/reload` | Hot-reload config file (API key required) |
| `POST /admin/update` | Self-update from GitHub Releases (API key required) |
| `GET /admin/config` | View current config (secrets redacted) |
| `PUT /admin/config` | Update config (API key required) |
| `POST /agent/sessions` | Create agent session |
| `GET /agent/sessions` | List active sessions |
| `POST /agent/sessions/:id/messages` | Send message to agent session |
| `DELETE /agent/sessions/:id` | Delete agent session |
| `WS /agent/ws/:id` | WebSocket for real-time agent events |

## Analytics & Monitoring

### Dashboard
Access the interactive dashboard at `http://your-herd:40114/dashboard`

**Features:**
- Real-time node status with GPU metrics
- Live request volume chart (updates every 30s)
- Top 5 models by request count
- Backend utilization distribution
- Model homing status and idle timers
- One-click backend management (add/edit/remove)
- Automatic update notifications
- **Sessions tab** — Active agent sessions, message history, tool call audit
- **Fleet tab** — Registered nodes, health status, enrollment management
- **Settings tab** — Live config editor with secret redaction
- **Agent Guide tab** — API reference, best practices, and error handling for AI agents

### Request Logging
All proxied requests are logged to `~/.herd/requests.jsonl`:

```json
{"timestamp":1709395200,"model":"glm-4.7-flash:latest","backend":"node-a","duration_ms":234,"status":"success","path":"/api/generate","request_id":"550e8400-e29b-41d4-a716-446655440000"}
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
    "glm-4.7-flash:latest": 892,
    "qwen2.5-coder:32b": 431,
    "llama3.1:8b": 200
  },
  "backend_counts": {
    "node-a": 1203,
    "node-b": 320
  },
  "timeline": [[1709395200, 45], [1709395260, 52], ...]
}
```

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

On startup, Herd checks for updates in the background and logs a notification if a newer version is available.

**Note:** After updating via `--update` or `/admin/update`, the server must be restarted to run the new version. The previous binary is kept as a backup for rollback.

## Hot Models & keep_alive (v0.4.3)

> **⚠️ Breaking change in v0.4.3:** `default_model` and `routing.idle_timeout_minutes` are removed. See the migration guide below.

Herd v0.4.3 solves the model eviction problem centrally. Ollama unloads models after 5 minutes by default, and clients like Open WebUI often send `"keep_alive": "5m"` which overrides any node-level env var. Herd fixes this at the proxy layer.

### keep_alive Injection

Add to `herd.yaml`:

```yaml
routing:
  default_keep_alive: "-1"   # never unload; set on every Ollama request
```

Herd injects this into every `/api/generate` and `/api/chat` request body, overriding whatever the client sent. `/v1/*` (OpenAI format) requests are passed through unchanged.

### Hot Models Warmer

```yaml
model_warmer:
  interval_secs: 240   # ping every 4 min (default); safely under Ollama's 5-min eviction window

backends:
  - name: "node-a"
    url: "http://gpu-1:11434"
    hot_models:
      - "glm-4.7-flash:latest"
      - "llama3:8b"
```

Herd pre-loads declared models on startup and re-loads them after OOM eviction by sending a minimal `keep_alive: "-1"` ping on every interval. No idle timer — models are always warm.

### Migration from v0.4.2

| Before | After |
|---|---|
| `backends[].default_model: "model:tag"` | `backends[].hot_models: ["model:tag"]` |
| `routing.idle_timeout_minutes: 30` | `model_warmer.interval_secs: 240` |

Old config keys are ignored after upgrading — Herd will log a warning at startup identifying the stale keys and their replacements. Update your `herd.yaml` to clear the warnings and restore intended behavior.

## GPU Awareness

Herd integrates with [gpu-hot](https://github.com/psalias2006/gpu-hot) for real-time metrics:

```yaml
# On each GPU node, run gpu-hot
docker run -d --gpus all -p 1312:1312 \
  -e NODE_NAME=my-node \
  ghcr.io/psalias2006/gpu-hot:latest
```

Then point Herd at the metrics endpoint:

```yaml
backends:
  - name: "node-a"
    url: "http://gpu-1:11434"
    gpu_hot_url: "http://gpu-1:1312"
```

**Dashboard GPU section:**
- Displays per-GPU cards with utilization, temperature, memory, power draw
- Auto-polls every 10 seconds
- Automatically hides if gpu-hot is unreachable
- Shows all GPUs on multi-GPU nodes

Herd will route based on:
- Model already loaded (via `/api/ps`)
- GPU VRAM available
- Current utilization

## Comparison to Olla

| Feature | Herd | Olla |
|---------|------|------|
| Priority routing | ✅ | ✅ |
| Circuit breaker | ✅ | ✅ |
| Model awareness | ✅ | ❌ |
| keep_alive injection | ✅ | ❌ |
| Hot models warmer | ✅ | ❌ |
| GPU metrics | ✅ | ❌ |
| Observability API | ✅ | ❌ |
| Retry with fallback | ✅ | ❌ |
| Admin API | ✅ | ❌ |
| OpenAI-compatible API | ✅ | ❌ |
| Streaming completions | ✅ | ❌ |
| Tag-based routing | ✅ | ❌ |
| Hot-reload config | ✅ | ❌ |
| Rate limiting | ✅ | ❌ |
| Prometheus metrics | ✅ | ❌ |
| Correlation IDs | ✅ | ❌ |
| Log rotation | ✅ | ❌ |
| Auto-update | ✅ | ❌ |
| Agent sessions | ✅ | ❌ |
| Fleet management | ✅ | ❌ |
| Task classification | ✅ | ❌ |
| Language | Rust | Go |

## License

MIT

## Support

If Herd is useful to you, consider sponsoring development:

[![GitHub Sponsors](https://img.shields.io/github/sponsors/swift-innovate?style=social)](https://github.com/sponsors/swift-innovate)

Your support helps keep the project maintained and moving forward. Thank you!

---

**Herd your llamas with intelligence.** 🦙
