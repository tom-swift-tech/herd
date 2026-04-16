# Herd v1.1 Feature Sprint — Auto Mode & Frontier Gateway

**Date:** 2026-04-09
**Status:** Spec — ready for implementation
**Branch:** `feature/smart-routing`

---

## Overview

Two tightly coupled features that transform Herd from a local fleet router into a unified inference gateway — routing seamlessly between local models and frontier cloud providers based on task requirements.

**Feature 1: Auto Mode** — Herd selects the best model and node from local inventory based on the incoming request, without the client specifying a model.

**Feature 2: Frontier Gateway** — Herd routes requests to configured cloud LLM providers (Anthropic, OpenAI, Grok/xAI, OpenRouter, MiniMax, etc.) when a request requires capabilities beyond local inventory, or when explicitly requested.

Together these implement the "frontier for judgment, local for labor" architecture: a lightweight local LLM classifies each request and Herd routes it to the cheapest backend that can handle it.

---

## Feature 1: Auto Mode

### Problem

Today, every request to Herd must specify `"model": "qwen2.5-coder:32b"` or similar. The client must know what models exist and where. This is fine for agent frameworks that manage model selection, but creates friction for:
- Ad-hoc usage (Open WebUI, Cursor, one-off scripts)
- Agents that don't have model awareness
- Workloads where the "right" model depends on the task

### Design

#### New Routing Strategy: `auto`

```yaml
routing:
  strategy: "auto"           # new strategy alongside priority, model_aware, etc.
  auto:
    classifier_model: "qwen3:1.7b"   # small local model used to classify requests
    classifier_backend: null          # null = use any healthy Ollama backend (auto-select)
    fallback_model: "qwen2.5-coder:32b"  # if classifier fails or times out
    classifier_timeout_ms: 3000       # max time for classification call
```

#### Classification Flow

1. Request arrives at Herd with `"model": "auto"` or with model omitted entirely
2. Herd extracts the user/system messages from the request body
3. Herd sends a lightweight classification prompt to `classifier_model` on a local backend:

```
You are a request classifier for an LLM router. Given the following user request, determine the best model tier and capability required.

Respond ONLY with a JSON object:
{
  "tier": "light" | "standard" | "heavy" | "frontier",
  "capability": "general" | "code" | "reasoning" | "creative" | "vision" | "extraction",
  "needs_large_context": true | false,
  "language": "en" | "zh" | "multi" | ...
}

User request:
{request_content}
```

4. Herd maps the classification to a model from its inventory using a configurable model map:

```yaml
routing:
  auto:
    model_map:
      light:
        general: "qwen3:1.7b"
        code: "qwen2.5-coder:7b"
        extraction: "qwen3:1.7b"
      standard:
        general: "qwen3:8b"
        code: "qwen2.5-coder:32b"
        creative: "gemma-4-26B:latest"
        reasoning: "qwen3:32b"
      heavy:
        general: "qwen3:32b"
        code: "qwen2.5-coder:32b"
        reasoning: "qwen3:32b"
        creative: "gemma-4-26B:latest"
      frontier:
        _provider: true        # signals: route to frontier gateway (Feature 2)
        general: "claude-sonnet-4-20250514"
        code: "claude-sonnet-4-20250514"
        reasoning: "claude-opus-4-20250514"
        creative: "claude-opus-4-20250514"
```

5. Herd resolves the target model to a backend via existing model_aware routing
6. If the model isn't loaded anywhere, fall back to `fallback_model`
7. Add response headers: `X-Herd-Auto-Tier`, `X-Herd-Auto-Capability`, `X-Herd-Auto-Model`

#### Inventory Awareness

The classifier model map references model names, but Herd should validate against actual inventory. At classification time:

- Check which models from the map are actually loaded (via existing model discovery cache)
- If the ideal model isn't available, fall back within the same tier (e.g., `qwen3:32b` not loaded → try `gemma-4-26B:latest` if it's in the same tier)
- If no model in that tier is available, escalate to next tier
- If escalation reaches `frontier` and frontier gateway is not configured, use `fallback_model`

#### Bypass

Clients can always override auto mode by specifying a model explicitly. Auto mode only activates when:
- `"model": "auto"` is sent
- `"model"` field is omitted entirely
- A new header `X-Herd-Auto: true` is sent

#### Config

```yaml
routing:
  strategy: "auto"        # or keep "model_aware" — auto can be a separate middleware
  auto:
    enabled: true          # off by default, zero overhead when disabled
    classifier_model: "qwen3:1.7b"
    classifier_backend: null
    classifier_timeout_ms: 3000
    fallback_model: "qwen2.5-coder:32b"
    cache_ttl_secs: 60     # cache classification results by message hash
    model_map:
      # ... as above
```

#### Implementation Notes

- New module: `src/classifier_auto.rs` (separate from existing `src/classifier.rs` which does keyword-based tier classification)
- The auto classifier uses a real LLM call through Herd's own backend pool — it's a request-within-a-request. Use a dedicated `reqwest::Client` with tight timeout to avoid cascading delays.
- Cache classifications by hashing the first 500 chars of the user message. An LRU cache with `cache_ttl_secs` TTL prevents re-classifying identical requests.
- Classification prompt should be kept under 200 tokens to ensure sub-second response on small models.
- Add `auto_classifications_total`, `auto_classification_duration_ms`, and `auto_cache_hits` to Prometheus metrics.
- Analytics JSONL should include `auto_tier`, `auto_capability`, `auto_model` fields when auto mode is active.

#### Edge Cases

- **Streaming requests:** Classify before proxying the first chunk. Classification adds latency to TTFT — this is the tradeoff. The `classifier_timeout_ms` caps worst-case added latency.
- **Classifier model not loaded:** If `classifier_model` isn't loaded on any backend, log a warning and use `fallback_model` directly. Don't crash.
- **Empty messages:** Use `fallback_model`. Don't classify empty content.
- **Vision/multimodal requests:** If request contains image data, set capability to "vision" without calling classifier. Route to a model that supports vision.

---

## Feature 2: Frontier Gateway

### Problem

Herd currently only routes to self-hosted backends (Ollama, llama-server, openai-compat on your own hardware). When a request needs frontier-level intelligence — complex reasoning, nuanced creative work, tasks where local models fall short — the client has to handle that routing itself, bypassing Herd entirely.

This breaks the "one endpoint" promise. Agents need to know about two different APIs. Cost tracking is split. There's no unified observability.

### Design

#### New Backend Type: `frontier`

```yaml
providers:
  - name: "anthropic"
    type: "frontier"
    api_url: "https://api.anthropic.com/v1"
    api_key_env: "ANTHROPIC_API_KEY"    # read from env var, never stored in config
    models:
      - "claude-opus-4-20250514"
      - "claude-sonnet-4-20250514"
    rate_limit: 50                       # requests per minute
    monthly_budget: 100.00               # USD — hard cap, requests rejected after
    priority: 50                         # lower than local backends

  - name: "openai"
    type: "frontier"
    api_url: "https://api.openai.com/v1"
    api_key_env: "OPENAI_API_KEY"
    models:
      - "gpt-4.1"
      - "o4-mini"
    rate_limit: 60
    monthly_budget: 50.00
    priority: 40

  - name: "xai"
    type: "frontier"
    api_url: "https://api.x.ai/v1"
    api_key_env: "XAI_API_KEY"
    models:
      - "grok-3"
      - "grok-3-mini"
    rate_limit: 30
    monthly_budget: 30.00
    priority: 30

  - name: "openrouter"
    type: "frontier"
    api_url: "https://openrouter.ai/api/v1"
    api_key_env: "OPENROUTER_API_KEY"
    models: []                           # empty = accept any model name, proxy through
    rate_limit: 100
    monthly_budget: 50.00
    priority: 20                         # lowest priority — fallback provider

  - name: "minimax"
    type: "frontier"
    api_url: "https://api.minimaxi.chat/v1"
    api_key_env: "MINIMAX_API_KEY"
    models:
      - "MiniMax-M1"
    rate_limit: 30
    monthly_budget: 20.00
    priority: 35
```

#### Safety Configuration

Frontier access must be explicitly enabled. This is critical — Herd's core value prop is zero API cost. Frontier routing should never activate by accident.

```yaml
frontier:
  enabled: false                # MUST be explicitly true — default off
  allow_auto_escalation: false  # if true, auto mode can route to frontier when local can't handle it
  require_header: true          # if true, requests must include X-Herd-Frontier: true to use frontier
  log_all_requests: true        # JSONL audit log for every frontier request with cost
  warn_threshold: 0.80          # warn at 80% of monthly budget
  block_threshold: 1.00         # hard block at 100% of monthly budget
```

#### Routing Logic

When a request targets a frontier model (either explicitly or via auto-mode escalation):

1. **Model resolution:** Find which provider serves the requested model
2. **Budget check:** Verify the provider hasn't exceeded its monthly budget (tracked in SQLite)
3. **Rate limit check:** Verify provider rate limit hasn't been hit (in-memory token bucket per provider)
4. **API translation:** Transform Herd's OpenAI-compatible request format to the provider's format:
   - **Anthropic:** OpenAI format → Anthropic Messages API format (different JSON structure, `max_tokens` required, role mapping)
   - **OpenAI / xAI / OpenRouter / MiniMax:** Already OpenAI-compatible, pass through with `Authorization: Bearer` header
5. **Proxy request** to provider, stream response back to client
6. **Record cost:** Parse response for token usage, calculate cost, write to SQLite cost tracker and JSONL audit log
7. **Add response headers:** `X-Herd-Provider`, `X-Herd-Cost-Estimate`, `X-Herd-Budget-Remaining`

#### API Translation Layer

Most providers speak OpenAI format. Anthropic is the exception. Create a `src/providers/` module:

```
src/providers/
  mod.rs           # ProviderAdapter trait
  anthropic.rs     # OpenAI → Anthropic Messages API translation
  openai_compat.rs # Pass-through for OpenAI/xAI/OpenRouter/MiniMax
```

The `ProviderAdapter` trait:

```rust
#[async_trait]
pub trait ProviderAdapter: Send + Sync {
    /// Transform an OpenAI-format request body into provider-specific format
    fn transform_request(&self, body: &serde_json::Value) -> Result<serde_json::Value>;

    /// Transform provider-specific response back to OpenAI format
    fn transform_response(&self, body: &serde_json::Value) -> Result<serde_json::Value>;

    /// Transform streaming SSE chunks (for providers with different streaming format)
    fn transform_stream_chunk(&self, chunk: &str) -> Result<String>;

    /// Extract token usage from response for cost tracking
    fn extract_usage(&self, body: &serde_json::Value) -> Option<TokenUsage>;

    /// Build the Authorization header value
    fn auth_header(&self, api_key: &str) -> String;
}
```

#### Cost Tracking

New SQLite table (alongside existing node registry):

```sql
CREATE TABLE frontier_costs (
    id INTEGER PRIMARY KEY,
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    tokens_in INTEGER NOT NULL,
    tokens_out INTEGER NOT NULL,
    cost_usd REAL NOT NULL,
    request_id TEXT,
    timestamp TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX idx_frontier_costs_provider_month
    ON frontier_costs(provider, timestamp);
```

Cost calculation uses a hardcoded price table (updated with each Herd release) or a configurable override:

```yaml
providers:
  - name: "anthropic"
    pricing:                      # optional — overrides built-in defaults
      claude-sonnet-4-20250514:
        input_per_mtok: 3.00
        output_per_mtok: 15.00
```

#### Model Discovery for Frontier

`GET /v1/models` should include frontier models (with a `provider` field) when frontier is enabled:

```json
{
  "id": "claude-sonnet-4-20250514",
  "object": "model",
  "owned_by": "anthropic",
  "herd_provider": "anthropic",
  "herd_type": "frontier"
}
```

#### Dashboard

New **Costs** tab (or extend Analytics):
- Monthly spend per provider (bar chart)
- Budget utilization gauges
- Cost per request timeline
- Top models by spend

#### Prometheus Metrics

```
herd_frontier_requests_total{provider="anthropic", model="claude-sonnet-4-20250514"} 342
herd_frontier_tokens_total{provider="anthropic", direction="in"} 890000
herd_frontier_tokens_total{provider="anthropic", direction="out"} 445000
herd_frontier_cost_usd_total{provider="anthropic"} 23.45
herd_frontier_budget_remaining_usd{provider="anthropic"} 76.55
herd_frontier_rate_limit_rejections_total{provider="openai"} 3
```

---

## How Auto Mode + Frontier Gateway Work Together

The full request flow when both features are enabled:

```
Client sends: POST /v1/chat/completions { "model": "auto", "messages": [...] }
                                    │
                                    ▼
                        ┌─────────────────────┐
                        │  Auto Classifier     │
                        │  (local qwen3:1.7b)  │
                        └──────────┬──────────┘
                                   │
                      tier: "standard", capability: "code"
                                   │
                                   ▼
                        ┌─────────────────────┐
                        │  Model Map Lookup    │
                        │  → qwen2.5-coder:32b │
                        └──────────┬──────────┘
                                   │
                          Is model loaded locally?
                          ┌────────┴────────┐
                         YES               NO
                          │                  │
                    Route to local      Escalate tier?
                    backend via         ┌────┴────┐
                    model_aware        YES       NO
                          │             │         │
                          ▼             ▼         ▼
                      Response    Try next    Use fallback_model
                                  tier up         │
                                    │             ▼
                              tier: "frontier"  Response
                              capability: "code"
                                    │
                                    ▼
                          ┌───────────────────┐
                          │  Frontier Gateway  │
                          │  → claude-sonnet   │
                          │  via Anthropic API │
                          └────────┬──────────┘
                                   │
                                   ▼
                              Response
                    (with X-Herd-Provider, X-Herd-Cost-Estimate headers)
```

---

## Implementation Order

### Sprint 1: Auto Mode (local only)
1. `src/classifier_auto.rs` — LLM-based request classifier with cache
2. Config parsing for `routing.auto.*` and `model_map`
3. Auto routing strategy in `src/router/`
4. Response headers (`X-Herd-Auto-*`)
5. Prometheus metrics for classification
6. Tests: enabled/disabled, classification, cache hit/miss, fallback, timeout
7. Dashboard: show auto-classification stats in Analytics tab

### Sprint 2: Frontier Gateway
1. `src/providers/` module with `ProviderAdapter` trait
2. `src/providers/anthropic.rs` — Anthropic Messages API translation
3. `src/providers/openai_compat.rs` — pass-through adapter
4. Config parsing for `providers` and `frontier` sections
5. SQLite cost tracking table + queries
6. Budget enforcement middleware (check before proxy, record after)
7. Rate limiting per provider (in-memory token bucket)
8. `GET /v1/models` extended with frontier models
9. Response headers (`X-Herd-Provider`, `X-Herd-Cost-*`)
10. Prometheus metrics for frontier usage
11. Tests: budget enforcement, rate limiting, API translation, cost recording
12. Dashboard: Costs tab

### Sprint 3: Integration
1. Wire auto mode `frontier` tier to frontier gateway
2. `allow_auto_escalation` config flag
3. End-to-end test: request → auto classify → local miss → frontier escalation → cost tracking
4. Update README, ROADMAP, skills.md
5. Update dashboard Agent Guide with frontier documentation

---

## Config Summary (Full Example)

```yaml
server:
  host: "0.0.0.0"
  port: 40114
  api_key: "your-admin-key"

routing:
  strategy: "auto"
  timeout: 120s
  retry_count: 2
  default_keep_alive: "-1"
  auto:
    enabled: true
    classifier_model: "qwen3:1.7b"
    classifier_timeout_ms: 3000
    fallback_model: "qwen2.5-coder:32b"
    cache_ttl_secs: 60
    model_map:
      light:
        general: "qwen3:1.7b"
        code: "qwen2.5-coder:7b"
      standard:
        general: "qwen3:8b"
        code: "qwen2.5-coder:32b"
        reasoning: "qwen3:32b"
      heavy:
        general: "qwen3:32b"
        code: "qwen2.5-coder:32b"
      frontier:
        _provider: true
        general: "claude-sonnet-4-20250514"
        reasoning: "claude-opus-4-20250514"
        code: "claude-sonnet-4-20250514"

backends:
  - name: "fast-node"
    url: "http://node-a:8090"
    backend: "llama-server"
    priority: 100

  - name: "easy-node"
    url: "http://node-b:11434"
    priority: 80

frontier:
  enabled: false
  allow_auto_escalation: false
  require_header: true
  log_all_requests: true
  warn_threshold: 0.80
  block_threshold: 1.00

providers:
  - name: "anthropic"
    type: "frontier"
    api_url: "https://api.anthropic.com/v1"
    api_key_env: "ANTHROPIC_API_KEY"
    models: ["claude-opus-4-20250514", "claude-sonnet-4-20250514"]
    rate_limit: 50
    monthly_budget: 100.00
    priority: 50

  - name: "openai"
    type: "frontier"
    api_url: "https://api.openai.com/v1"
    api_key_env: "OPENAI_API_KEY"
    models: ["gpt-4.1", "o4-mini"]
    rate_limit: 60
    monthly_budget: 50.00
    priority: 40

  - name: "xai"
    type: "frontier"
    api_url: "https://api.x.ai/v1"
    api_key_env: "XAI_API_KEY"
    models: ["grok-3", "grok-3-mini"]
    rate_limit: 30
    monthly_budget: 30.00
    priority: 30

  - name: "minimax"
    type: "frontier"
    api_url: "https://api.minimaxi.chat/v1"
    api_key_env: "MINIMAX_API_KEY"
    models: ["MiniMax-M1"]
    rate_limit: 30
    monthly_budget: 20.00
    priority: 35

  - name: "openrouter"
    type: "frontier"
    api_url: "https://openrouter.ai/api/v1"
    api_key_env: "OPENROUTER_API_KEY"
    models: []
    rate_limit: 100
    monthly_budget: 50.00
    priority: 20
```

---

## Code Quality Rules (per CLAUDE.md)

- Both features default to `enabled: false` — zero overhead when not opted in
- All new config fields must have sensible defaults and not break existing `herd.yaml` files
- Frontier API keys read from env vars only — NEVER stored in config or logged
- New endpoints must appear in `skills.md` and the dashboard Agent Guide tab
- JSONL analytics logging must be extended (not replaced) with new fields
- Tests for each feature — at minimum: enabled/disabled, happy path, edge cases
- Never bail! on config errors — degrade gracefully
- Conventional commits: `feat:`, `fix:`, `test:`, `docs:`
- Git branch: `main`, always `git branch -M main` after `git init`
