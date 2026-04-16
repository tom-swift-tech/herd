# Herd — Agent Skills Reference

> This document is for AI agents that are routed through Herd.
> Read this to understand how to make optimal requests.

## What Herd Does

Herd is a reverse proxy that sits between you and one or more Ollama backends.
When you send a request, Herd picks the best backend based on model availability,
GPU load, priority, and tags. If a backend fails, Herd retries on another.

**You do not talk to Ollama directly. You talk to Herd, and Herd routes for you.**

## Base URL

Herd listens on port `40114` by default.

```
http://<herd-host>:40114
```

All examples below assume this base URL.

## Endpoints You Should Use

### Chat — Native Ollama (Recommended)

```
POST /api/chat
Content-Type: application/json
```

```json
{
  "model": "qwen2.5-coder:32b",
  "messages": [
    {"role": "system", "content": "You are a helpful assistant."},
    {"role": "user", "content": "Explain TCP handshakes."}
  ],
  "stream": true
}
```

- **This is the preferred endpoint.** It goes directly to Ollama's native chat API and works reliably with all models.
- **Streaming:** Set `"stream": true` for SSE streaming (recommended for long responses).
- **Model:** Always specify the model you need. Herd uses this to route to a backend that already has it loaded.

### Chat Completions (OpenAI-compatible)

```
POST /v1/chat/completions
Content-Type: application/json
```

Use this only if you need OpenAI client library compatibility. It forwards to Ollama's OpenAI compatibility layer on the backend.

> **Warning:** Some models with custom chat templates (e.g. GLM, some Qwen variants) may hang or fail on this endpoint while working correctly on `/api/chat`. If you see timeouts or unexpected errors, switch to `/api/chat`.

- **Response format:** Identical to OpenAI's API. Existing OpenAI client libraries work unchanged.
- **keep_alive:** Not injected on `/v1/*` paths. Use `/api/chat` or `/api/generate` if you need Herd's `keep_alive` injection.

### List Available Models

```
GET /v1/models
```

Returns all models available across all healthy backends (deduplicated).

```json
{
  "object": "list",
  "data": [
    {"id": "qwen2.5-coder:32b", "object": "model", "owned_by": "ollama"},
    {"id": "llama3.1:8b", "object": "model", "owned_by": "ollama"}
  ]
}
```

**Use this to discover which models are available before making requests.**

### Health Check

```
GET /health
```

Returns `200 OK` with body `"OK"` if Herd is running. No auth required.

### Cluster Status

```
GET /status
```

Returns detailed backend information:

```json
{
  "healthy_backends": [
    {
      "name": "citadel-5090",
      "url": "http://citadel:11434",
      "priority": 100,
      "models": ["qwen2.5-coder:32b", "llama3.1:8b"],
      "model_count": 2,
      "current_model": "qwen2.5-coder:32b",
      "hot_models": ["qwen2.5-coder:32b"],
      "healthy": true,
      "gpu": {
        "utilization": 45.0,
        "memory_used": 18432,
        "memory_total": 24576,
        "temperature": 62.0
      }
    }
  ],
  "unhealthy_backends": [],
  "routing_strategy": "ModelAware"
}
```

### Ollama Native API (Proxied)

Herd proxies all Ollama endpoints transparently:

```
POST /api/generate      — Single-turn generation
POST /api/chat          — Multi-turn chat
GET  /api/tags          — List models on routed backend
GET  /api/ps            — Running models on routed backend
POST /api/pull          — Pull a model (routes to one backend)
POST /api/show          — Model info
```

These go through the same routing logic. Include `"model"` in your JSON body
so Herd can route to the right backend.

### Analytics

```
GET /analytics?hours=24
```

Returns request statistics: total requests, latency percentiles (p50/p95/p99),
model counts, backend counts, and a timeline.

### Prometheus Metrics

```
GET /metrics
```

Standard Prometheus exposition format with backend gauges, request counters,
and latency histograms.

## Task Classification

Herd can automatically classify requests by complexity tier when no explicit model is specified.

**Headers:**
- `X-Herd-Tier` (response) — tier name assigned by classifier (`heavy`, `standard`, `lightweight`)

**Behavior:**
- Requests with an explicit `model` field → classifier skipped
- Requests with `X-Herd-Tags` header → classifier skipped
- No model/tags → classifier runs keyword matching → assigns tier → injects model

**Configuration:** See `task_classifier` section in `herd.yaml`

## Auto Mode

When enabled, Auto Mode routes requests that specify `"model": "auto"` (or omit the model entirely) to the best available model using an LLM-based classifier.

**How it works:**
1. Herd calls a small local classifier model (default: `qwen3:1.7b`) to classify the request
2. The classifier assigns a **tier** (`light`, `standard`, `heavy`, `frontier`) and **capability** (`general`, `code`, `reasoning`, `creative`, `vision`, `extraction`)
3. Herd looks up the best model in the configured `model_map` and routes there
4. Classification results are cached by message hash — repeated requests skip the classifier

**Response headers:**
- `X-Herd-Auto-Tier` — tier assigned (`light`, `standard`, `heavy`, `frontier`)
- `X-Herd-Auto-Capability` — capability assigned (`general`, `code`, `reasoning`, etc.)
- `X-Herd-Auto-Model` — model selected from the map

**To use Auto Mode:**

```json
{"model": "auto", "messages": [{"role": "user", "content": "Write a Rust function to parse JSON"}]}
```

Or omit the model entirely — Herd detects the missing field and runs the classifier.

**Auto Mode is off by default.** Operators must enable it with `routing.auto.enabled: true` and configure a `model_map`. If not enabled, requests with `"model": "auto"` are rejected with `400 Bad Request`.

## Frontier Gateway

When a request targets a frontier (cloud) model — either by sending the model name explicitly or via Auto Mode classifying to the `frontier` tier — Herd proxies through the configured provider (Anthropic, OpenAI, xAI, OpenRouter, MiniMax).

**Request headers:**
- `X-Herd-Frontier: true` — required unless `require_header: false` in config, or Auto-Mode auto-escalation is enabled

**Response headers:**
- `X-Herd-Provider` — which provider served the request (e.g. `anthropic`, `openai`)
- `X-Herd-Cost-Estimate` — USD cost for this request (non-streaming only), calculated from response token usage and the pricing table
- `X-Herd-Auto-Tier` / `-Capability` / `-Model` — set when the request was auto-escalated from Auto Mode

**Status codes:**
- `403 Forbidden` — `require_header: true` and no `X-Herd-Frontier: true` header and not auto-escalated
- `402 Payment Required` — provider monthly budget cap exceeded
- `429 Too Many Requests` — per-provider `rate_limit` (requests/minute) exhausted; bucket refills at the next minute boundary
- `503 Service Unavailable` — provider API key env var is missing
- `502 Bad Gateway` — provider request failed

**Auto-Mode escalation behavior:**
- If `routing.auto.enabled: true` and the classifier returns `tier: "frontier"`, Herd routes to the frontier gateway **only** when `frontier.allow_auto_escalation: true`.
- When escalation is disabled, the request falls back to `routing.auto.fallback_model` — no cloud request fires.

**Cost visibility:**
```
GET /api/frontier/costs
```
Returns per-provider monthly spend, token totals, and request counts pulled from the SQLite cost log.

**The Frontier Gateway is off by default** (`frontier.enabled: false`). Operators must enable it explicitly and set provider API keys via environment variables.

## Headers You Should Know

### X-Herd-Tags (Request Routing)

Target specific backends by tag:

```
X-Herd-Tags: gpu,fast
```

Backends are configured with tags like `tags: ["gpu", "fast"]`. When you send
this header, Herd only considers backends matching **all** specified tags.

**Use case:** Route coding tasks to high-VRAM GPUs, research to slower but larger-context nodes.

### X-Request-Id (Correlation)

```
X-Request-Id: my-trace-id-123
```

- If you send this header, Herd preserves it through to the backend and returns it in the response.
- If you don't send it, Herd generates a UUID v4 and returns it in the response.
- Use this to correlate requests across distributed systems.

### Authentication (Admin Only)

Admin endpoints require an API key. Regular routing endpoints do not.

```
X-API-Key: your-secret-key
```

or

```
Authorization: Bearer your-secret-key
```

## Routing Strategies

Herd supports four strategies. You don't choose the strategy per-request — it's configured
server-side. But understanding the active strategy helps you make better requests.

| Strategy | Behavior |
|----------|----------|
| `model_aware` | Prefers backends that already have your requested model loaded. Avoids cold starts. **(default)** |
| `priority` | Always routes to the highest-priority healthy backend. |
| `least_busy` | Routes to the backend with the lowest GPU utilization. |
| `weighted_round_robin` | Distributes across backends weighted by priority. |

Check `GET /status` → `routing_strategy` to see which is active.

## Best Practices for Agents

### 1. Use native Ollama endpoints

```
POST /api/chat       ← preferred for chat
POST /api/generate   ← preferred for single-turn generation
```

`/api/chat` and `/api/generate` go directly to Ollama's native API. They work
reliably with **all** models and receive Herd's `keep_alive` injection.

`/v1/chat/completions` routes through Ollama's OpenAI compatibility layer.
Most models work, but models with custom chat templates (GLM, some Qwen variants)
may hang or fail. Only use `/v1/chat/completions` when you need OpenAI client
library compatibility and have confirmed the model supports it.

### 2. Always specify the model

```json
{"model": "qwen2.5-coder:32b", "messages": [...]}
```

Without a model, Herd can't do model-aware routing and may route you to a backend
that needs to cold-load the model (slow).

### 3. Use streaming for long responses

```json
{"stream": true}
```

Streaming prevents timeout issues on long generations and gives you incremental output.

### 4. Discover models before requesting

```bash
GET /v1/models
```

Don't guess model names. Query available models first to avoid 404s from Ollama.

### 5. Use tags for workload isolation

If your Herd instance has tagged backends:

```
X-Herd-Tags: fast
```

This ensures your request goes to a backend suited for your workload type.

### 6. Handle 503 gracefully

A `503 Service Unavailable` means no healthy backend could serve your request.
This can happen when:
- All backends are down
- No backend has the requested model and `model_aware` routing is active
- Circuit breakers have tripped on all backends

**Retry after a few seconds.** Herd's circuit breaker automatically recovers backends.

### 7. Handle 502 gracefully

A `502 Bad Gateway` means Herd reached a backend but it failed. The response includes
a `request_id` for debugging:

```json
{"error": "Bad Gateway", "request_id": "abc-123-def"}
```

### 8. Don't hard-code backend URLs

Always go through Herd. Never bypass it to talk to individual Ollama instances.
Herd handles failover, load balancing, and model routing — bypassing it defeats the purpose.

### 9. Send correlation IDs for traceability

```
X-Request-Id: agent-task-42-step-3
```

This makes it easy to trace your requests in logs and analytics.

## Circuit Breaker Behavior

When a backend fails repeatedly (default: 3 consecutive failures):
1. Herd marks it **unhealthy** and stops routing to it
2. After a recovery period (default: 60 seconds), Herd probes it again
3. If the probe succeeds, the backend is marked **healthy** and receives traffic

This is automatic. You don't need to do anything — just retry your request and
Herd will route to a healthy backend.

## Rate Limiting

If Herd has rate limiting enabled, you'll receive `429 Too Many Requests` when
the limit is exceeded. Back off and retry.

## Hot Models & keep_alive

Herd keeps models permanently loaded using two mechanisms:

**keep_alive injection:** Every request to `/api/generate` and `/api/chat` gets
`"keep_alive": "-1"` injected, overriding whatever the client sent. This prevents
clients like Open WebUI from accidentally evicting models.

**Hot models warmer:** Backends can declare `hot_models` — Herd pings each one
every 4 minutes with `keep_alive: "-1"` to pre-load on startup and recover from OOM.

## Quick Reference

| Action | Method | Endpoint | Notes |
|--------|--------|----------|-------|
| **Chat (native — recommended)** | POST | `/api/chat` | Works with all models |
| **Generate (native — recommended)** | POST | `/api/generate` | Works with all models |
| Chat with auto routing | POST | `/api/chat` | Set `"model": "auto"` (requires Auto Mode enabled) |
| Chat (OpenAI compat) | POST | `/v1/chat/completions` | Some models may not support |
| List models (OpenAI format) | GET | `/v1/models` | |
| List models (Ollama native) | GET | `/api/tags` | |
| Health check | GET | `/health` | |
| Cluster status | GET | `/status` | |
| GPU metrics | GET | `/status` | Includes GPU data when gpu-hot is configured |
| Analytics | GET | `/analytics?hours=24` | |
| Prometheus | GET | `/metrics` | |
| Update check | GET | `/update` | |
| Dashboard | GET | `/dashboard` | |
| Skills (this data as JSON) | GET | `/skills` | |

## Self-Onboarding

Agents can fetch their own best-practice prompt at startup:

```bash
curl http://herd:40114/skills
```

Returns structured JSON with endpoints, headers, best practices, and error codes.
Use this to bootstrap your system prompt or tool configuration.

## Example: Full Agent Workflow

```python
import requests

HERD = "http://herd:40114"

# 1. Discover available models
models = requests.get(f"{HERD}/v1/models").json()
available = [m["id"] for m in models["data"]]
print(f"Available: {available}")

# 2. Pick the best model for the task
model = "qwen2.5-coder:32b" if "qwen2.5-coder:32b" in available else available[0]

# 3. Send a chat request using the native Ollama endpoint (recommended)
response = requests.post(
    f"{HERD}/api/chat",
    headers={
        "X-Request-Id": "agent-session-001",
        "X-Herd-Tags": "gpu",
    },
    json={
        "model": model,
        "messages": [
            {"role": "system", "content": "You are a senior engineer."},
            {"role": "user", "content": "Review this code for security issues."},
        ],
        "stream": False,
    },
)

# 4. Use the response
result = response.json()
print(result["message"]["content"])

# Note: if you need OpenAI client library compatibility, use /v1/chat/completions instead.
# Most models work, but some (GLM, certain Qwen variants) require the native /api/chat.
```

## Configuration Reference (for operators)

Agents don't configure Herd, but understanding the config helps predict behavior:

```yaml
server:
  host: "0.0.0.0"
  port: 40114
  api_key: "secret"       # Required for admin API
  rate_limit: 0           # 0 = unlimited

routing:
  strategy: "model_aware" # priority | model_aware | least_busy | weighted_round_robin
  timeout: 120s           # Per-request timeout
  retry_count: 2          # Retries on failure
  default_keep_alive: "-1" # injected into every Ollama request

model_warmer:             # keeps hot_models loaded
  interval_secs: 240      # ping hot_models every 4 min

backends:
  - name: "citadel"
    url: "http://citadel:11434"
    priority: 100
    hot_models:            # models to keep permanently loaded
      - "qwen2.5-coder:32b"
    tags: ["gpu", "fast"]
    model_filter: "qwen|llama"  # Regex: only route matching models

circuit_breaker:
  failure_threshold: 3
  timeout: 120s
  recovery_time: 60s
```
