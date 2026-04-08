# Herd

[![GitHub release](https://img.shields.io/github/v/release/swift-innovate/herd)](https://github.com/swift-innovate/herd/releases/latest)
[![GitHub stars](https://img.shields.io/github/stars/swift-innovate/herd?style=social)](https://github.com/swift-innovate/herd/stargazers)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.70+-orange.svg)](https://www.rust-lang.org)
[![Roadmap](https://img.shields.io/badge/roadmap-v1.0%20multi--backend-blue)](ROADMAP.md)

## Built for OpenClaw swarms

Running multiple agents + parallel tool calls? Herd stops the GPU wars.
Just set your OpenClaw `baseUrl` to `http://your-herd:40114` -- done.
Model homing + live VRAM routing keeps every agent on the fastest node.

> **Pro tip:** Point your OpenClaw agents at Herd and they instantly become GPU-smart -- live VRAM routing, model-aware routing to already-loaded backends, and parallel tool calls across your whole swarm without GPU wars.
<img width="1735" height="803" alt="image" src="https://github.com/user-attachments/assets/d625b30f-8110-482e-80cd-e3297a5ff428" />

**Intelligent LLM router with multi-backend support, GPU awareness, analytics, and real-time monitoring.**

Route your llama herd with intelligence -- llama-server, Ollama, and OpenAI-compatible backends with priority routing, circuit breakers, model awareness, real-time GPU metrics, HuggingFace model search, beautiful dashboards, and OpenAI-compatible endpoints.

## Getting Started

Herd is a reverse proxy that sits in front of your inference backends (Ollama, llama-server, or any OpenAI-compatible endpoint), routing requests to the best available backend based on model availability, GPU load, and priority.

### 1. Create a config file

```yaml
# herd.yaml -- minimal Ollama setup
backends:
  - name: "my-gpu"
    url: "http://localhost:11434"
    priority: 100

routing:
  strategy: "model_aware"
```

Or with a llama-server backend:

```yaml
backends:
  - name: "my-gpu-llama"
    url: "http://localhost:8090"
    backend: "llama-server"
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
curl http://localhost:40114/health          # -> "OK"
curl http://localhost:40114/status          # -> backend list + GPU info
curl http://localhost:40114/v1/models       # -> available models across all backends
```

You're now routing through Herd. Point any OpenAI-compatible client or Ollama agent at `http://localhost:40114`.

### Architecture

```
+---------------------------------------------------+
|                       Herd                         |
+---------------------------------------------------+
|  +---------+  +---------+  +---------------+       |
|  |  HTTP   |  | Router  |  |   Circuit     |       |
|  |  Proxy  |->| Engine  |->|   Breaker     |       |
|  +---------+  +---------+  +---------------+       |
|       |            |              |                 |
|  +------------------------------------------------+|
|  |              Backend Pool                       ||
|  |  +-----------+ +------------+ +-----------+     ||
|  |  | Citadel   | | minipc     | | warden    |     ||
|  |  | llama-srv | | Ollama     | | Ollama    |     ||
|  |  | :8090     | | :11434     | | :11434    |     ||
|  |  +-----------+ +------------+ +-----------+     ||
|  +------------------------------------------------+|
+---------------------------------------------------+
```

## What Herd delivers (and what it doesn't)

**The real win for swarms is contention reduction, not cold-start elimination.**

With a thoughtful `hot_models` list and `model_aware` strategy, Herd eliminates the worst waits for your top 20-30 most-used models -- routing every request to a backend where that model is already resident and warm. The remaining models still hit normal load time on first use or after an eviction cascade; Herd can't conjure VRAM that isn't there.

What makes Herd more than DNS round-robin + client retry:

| | DNS round-robin | Herd |
|---|---|---|
| Knows which backend has the model loaded | No | Yes -- via model discovery (Ollama `/api/ps`, llama-server `/v1/models`) |
| Routes by live GPU utilization | No | Yes -- via gpu-hot telemetry |
| Enforces `keep_alive` centrally | No | Yes -- injected on every Ollama request |
| Circuit-breaks failed nodes | No | Yes |
| Single place to add retries/failover | No | Yes |
| Supports mixed backends (Ollama + llama-server) | No | Yes |

Herd is a **smart stateless proxy with a stateful routing cache**. It is not HA failover -- in-flight requests are not redirected if a backend dies mid-stream. That's on the roadmap.

## Features

### Multi-Backend Support (v1.0)
- **Three backend types** -- `ollama`, `llama-server`, and `openai-compat` per node
- **llama-server integration** -- 44-80% faster TTFT and ~4x throughput vs Ollama on identical hardware ([benchmark data](docs/LLAMA_CPP_BACKEND.md))
- **Backend-agnostic routing** -- all backends expose OpenAI-compatible HTTP; the router treats them identically
- **herd-tune GPU detection** -- auto-detect NVIDIA (with Blackwell/CUDA 13 support), AMD (ROCm), Intel (SYCL), Vulkan fallback
- **llama-server provisioning** -- herd-tune downloads the correct llama-server binary for your GPU

### HuggingFace Model Management (v1.0)
- **Model search** -- `GET /api/models/search?q=llama` searches HuggingFace for GGUF models
- **Model download** -- `POST /api/models/download` triggers model download to a node
- **Per-node model listing** -- `GET /api/models/node/:id` shows models on a specific node
- **Model deletion** -- `DELETE /api/models/node/:id/:model` removes a model from a node

### Core Routing
- **Priority-based routing** -- Route to the best GPU first
- **Model-aware routing** -- Route to nodes with models already loaded
- **Weighted round-robin** -- Distribute by priority weight
- **Least-busy routing** -- Route to lowest GPU utilization
- **Tag-based routing** -- Filter by `X-Herd-Tags` header
- **Circuit breaker** -- Auto-recover from failed nodes
- **keep_alive injection** -- Override `keep_alive` on every Ollama request centrally; prevents clients from accidentally evicting models
- **Hot models warmer** -- Declare `hot_models` per backend; Herd pre-loads and keeps them warm automatically
- **Hot-reload config** -- File watcher + `POST /admin/reload`
- **Rate limiting** -- Global token-bucket rate limiter
- **OpenAI-compatible** -- Drop-in `/v1/chat/completions` endpoint
- **Auto-update** -- `herd --update` or `POST /admin/update`

### Telemetry & Observability (v1.0)
- **Token counting** -- `herd_tokens_total` and `herd_tokens_per_second` Prometheus metrics
- **Per-model/backend latency** -- Labeled duration histograms with percentile breakdowns
- **Cost estimation** -- Token-based cost tracking per request
- **Prometheus metrics** -- `/metrics` endpoint with request counters, backend gauges, token metrics, and latency histograms
- **Log rotation** -- Size-based rotation with configurable retention (days, max size, max files)
- **Request analytics** -- JSONL logging with token counts and auto-retention
- **GPU metrics** -- Real-time VRAM, utilization, temperature
- **Latency tracking** -- P50, P95, P99 percentiles
- **Correlation IDs** -- `X-Request-Id` propagation for distributed agent tracing

### Dashboard Control Plane (v1.0)
- **Models tab** -- HuggingFace search, per-node model listing, download and delete from the UI
- **Fleet tab** -- GPU badges showing vendor, VRAM, backend type per node
- **Analytics charts** -- Real-time request volume, latency, and model usage visualizations
- **Backend management** -- One-click add/edit/remove backends
- **Agent Guide tab** -- API reference, best practices, and error handling for AI agents

### Agent-Friendly
- **Agent skills reference** -- [`skills.md`](skills.md) teaches AI agents how to use Herd's API, routing, and headers
- **Dashboard Agent Guide** -- Built-in tab at `/dashboard` with endpoint tables, best practices, and error handling
- **OpenAI drop-in** -- Point any agent's `base_url` to Herd and it just works
- **Tag-based routing** -- Agents can target specific backends via `X-Herd-Tags` header

> **v1.0.0** -- llama-server backend, multi-vendor fleet, HuggingFace model search, telemetry enrichment, dashboard control plane. 189 tests. See the [Roadmap](ROADMAP.md) for what's next.

## Quick Start

```bash
# Install
cargo install herd

# Run with config
herd --config herd.yaml

# Or with CLI args
herd --port 40114 \
  --backend citadel=http://citadel:11434:100 \
  --backend minipc=http://minipc:11434:80 \
  --backend warden=http://warden:11434:50
```

## For AI Agents

Herd ships with built-in documentation for AI agents routed through it:

- **`GET /skills`** -- JSON endpoint agents can fetch at startup for best practices, endpoints, headers, and error codes. Self-service onboarding.
- **[`skills.md`](skills.md)** -- Complete API reference with examples. Point your agent at this file for the full guide.
- **Dashboard Agent Guide** -- The `/dashboard` includes an "Agent Guide" tab with endpoint tables, do/don't checklists, and error handling.

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
6. Prefer native Ollama endpoints (`/api/chat`, `/api/generate`) over `/v1/chat/completions` -- Herd routes both identically, but the native endpoints bypass Ollama's OpenAI compat layer and work reliably with all models

## Using Herd with any LLM client

Herd is built for OpenClaw swarms, but it works as a drop-in proxy for any OpenAI-compatible or Ollama client -- Open WebUI, Continue.dev, Cursor, LiteLLM, custom scripts, whatever.

```bash
# Before: point directly at one backend
OLLAMA_HOST=http://my-gpu:11434

# After: point at Herd instead -- nothing else changes
OLLAMA_HOST=http://herd:40114
```

You get model-aware routing, circuit breakers, GPU telemetry routing, and centralized `keep_alive` enforcement with zero client changes. If you only have one GPU node today, Herd still centralizes `keep_alive` and gives you a dashboard -- and you can add more backends later without touching your clients.

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
  default_keep_alive: "-1"  # inject into every Ollama request

model_warmer:
  interval_secs: 240       # ping hot_models every 4 min

backends:
  # Ollama backend (default)
  - name: "citadel-ollama"
    url: "http://citadel:11434"
    priority: 80
    gpu_hot_url: "http://citadel:1312"
    tags: ["gpu", "fast"]
    hot_models:
      - "qwen2.5-coder:32b"
      - "glm-4.7-flash:latest"

  # llama-server backend
  - name: "citadel-llama"
    url: "http://citadel:8090"
    backend: "llama-server"          # Backend type
    priority: 100
    tags: ["gpu", "high-vram"]
    # health_check_path defaults to /health for llama-server
    # Model discovery uses /v1/models instead of /api/tags

  # OpenAI-compatible backend (any provider)
  - name: "external-api"
    url: "http://my-openai-compat-server:8080"
    backend: "openai-compat"
    priority: 50

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

### Backend Types

| Backend | `backend` value | Health Check | Model Discovery | keep_alive Injection |
|---------|-----------------|--------------|-----------------|---------------------|
| Ollama | `"ollama"` (default) | `/api/version` | `/api/tags` + `/api/ps` | Yes |
| llama-server | `"llama-server"` | `/health` | `/v1/models` | No (not applicable) |
| OpenAI-compat | `"openai-compat"` | `/v1/models` | `/v1/models` | No |

## herd-tune: Node Setup & Registration

herd-tune auto-detects your GPU hardware, sets up the inference backend, and registers with a Herd instance.

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

# Explicitly use llama-server
herd-tune --backend llama-server --model /path/to/model.gguf

# Explicitly use Ollama
herd-tune --backend ollama --apply
```

### GPU detection

herd-tune detects GPU vendor and selects the correct llama-server binary:

| GPU | Detection | Binary Selected |
|-----|-----------|----------------|
| NVIDIA (Ampere, Ada) | `nvidia-smi` | CUDA 12.x |
| NVIDIA (Blackwell/5000-series) | `nvidia-smi` + compute capability | CUDA 13.x (required -- 12.x silently falls back to CPU) |
| AMD (RDNA3+) | `rocm-smi` / `hipconfig` | ROCm/HIP |
| Intel (Arc) | `sycl-ls` / WMI | SYCL (beta) |
| Fallback | Any | Vulkan (universal, ~25% slower) |

## Choosing the Right Endpoint

> **Important:** Not all models work on all endpoints. Choose based on your model and client.

**Decision tree:**

1. **Using GLM, custom-template models, or any model that misbehaves?** -> Use `/api/chat` (native Ollama). This bypasses Ollama's OpenAI compatibility layer and works reliably with all models.
2. **Need OpenAI SDK compatibility** (e.g., `openai.ChatCompletion.create`)? -> Use `/v1/chat/completions`, but **test with your specific model first**. Some models hang or error on this endpoint.
3. **Single-turn generation?** -> Use `/api/generate`.

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

### Model Management Endpoints (v1.0)

| Endpoint | Description |
|----------|-------------|
| `GET /api/models/search?q=llama` | Search HuggingFace for GGUF models |
| `POST /api/models/download` | Download a model to a specific node |
| `GET /api/models/node/:id` | List models on a specific node |
| `DELETE /api/models/node/:id/:model` | Delete a model from a node |

### Correlation IDs

Every request gets an `X-Request-Id` header for end-to-end tracing:

```bash
# Herd generates a UUID v4 if you don't provide one
curl http://localhost:40114/v1/chat/completions -d '...'
# Response includes: X-Request-Id: 550e8400-e29b-41d4-a716-446655440000

# Or provide your own -- Herd forwards it to the upstream backend
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
| `GET /api/models/search` | HuggingFace GGUF model search |
| `POST /api/models/download` | Download model to node |
| `GET /api/models/node/:id` | List models on node |
| `DELETE /api/models/node/:id/:model` | Delete model from node |
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
| `POST /admin/reload` | Hot-reload config file (when enabled, API key required) |
| `POST /admin/update` | Self-update from GitHub Releases (API key required) |

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
- **Models tab** -- HuggingFace search, per-node model listing, download and delete
- **Fleet tab** -- GPU badges showing vendor, VRAM, and backend type per node
- **Analytics charts** -- Real-time latency, throughput, and model usage visualizations
- **Agent Guide tab** -- API reference, best practices, and error handling for AI agents

### Request Logging
All proxied requests are logged to `~/.herd/requests.jsonl`:

```json
{"timestamp":1709395200,"model":"glm-4.7-flash:latest","backend":"citadel-5090","duration_ms":234,"status":"success","path":"/api/generate","request_id":"550e8400-e29b-41d4-a716-446655440000","tokens_prompt":128,"tokens_completion":256}
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
    "citadel-5090": 1203,
    "minipc-4080": 320
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

## Hot Models & keep_alive

Herd solves the model eviction problem centrally. Ollama unloads models after 5 minutes by default, and clients like Open WebUI often send `"keep_alive": "5m"` which overrides any node-level env var. Herd fixes this at the proxy layer.

### keep_alive Injection

Add to `herd.yaml`:

```yaml
routing:
  default_keep_alive: "-1"   # never unload; set on every Ollama request
```

Herd injects this into every `/api/generate` and `/api/chat` request body, overriding whatever the client sent. `/v1/*` (OpenAI format) requests are passed through unchanged. Only applies to Ollama backends.

### Hot Models Warmer

```yaml
model_warmer:
  interval_secs: 240   # ping every 4 min (default); safely under Ollama's 5-min eviction window

backends:
  - name: "citadel"
    url: "http://citadel:11434"
    hot_models:
      - "glm-4.7-flash:latest"
      - "llama3:8b"
```

Herd pre-loads declared models on startup and re-loads them after OOM eviction by sending a minimal `keep_alive: "-1"` ping on every interval. No idle timer -- models are always warm.

## GPU Awareness

Herd integrates with [gpu-hot](https://github.com/psalias2006/gpu-hot) for real-time metrics:

```yaml
# On each GPU node
docker run -d --gpus all -p 1312:1312 \
  -e NODE_NAME=citadel \
  ghcr.io/psalias2006/gpu-hot:latest
```

Then configure Herd to query metrics:

```yaml
backends:
  - name: "citadel"
    url: "http://citadel:11434"
    gpu_hot_url: "http://citadel:1312"
```

**Dashboard GPU section:**
- Displays per-GPU cards with utilization, temperature, memory, power draw
- Auto-polls every 10 seconds
- Automatically hides if gpu-hot is unreachable
- Shows all GPUs on multi-GPU nodes

Herd will route based on:
- Model already loaded (via model discovery)
- GPU VRAM available
- Current utilization

## Comparison to Olla

| Feature | Herd | Olla |
|---------|------|------|
| Priority routing | Yes | Yes |
| Circuit breaker | Yes | Yes |
| Model awareness | Yes | No |
| Multi-backend (Ollama + llama-server) | Yes | No |
| keep_alive injection | Yes | No |
| Hot models warmer | Yes | No |
| GPU metrics | Yes | No |
| HuggingFace model search | Yes | No |
| Token-level telemetry | Yes | No |
| Observability API | Yes | No |
| Retry with fallback | Yes | No |
| Admin API | Yes | No |
| OpenAI-compatible API | Yes | No |
| Streaming completions | Yes | No |
| Tag-based routing | Yes | No |
| Hot-reload config | Yes | No |
| Rate limiting | Yes | No |
| Prometheus metrics | Yes | No |
| Correlation IDs | Yes | No |
| Log rotation | Yes | No |
| Auto-update | Yes | No |
| Language | Rust | Go |

## License

MIT

## Support

If Herd is useful to you, consider sponsoring development:

[![GitHub Sponsors](https://img.shields.io/github/sponsors/swift-innovate?style=social)](https://github.com/sponsors/swift-innovate)

Your support helps keep the project maintained and moving forward. Thank you!

---

**Herd your llamas with intelligence.**
