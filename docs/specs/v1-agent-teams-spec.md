# Herd v1.0 — Agent Teams Spec

**Date:** 2026-04-08
**Author:** Tom Swift (Director) + Gage (Code Division Lead)
**Target:** Claude Code Agent Teams execution via 6 parallel lanes

---

## Context

**Note:** This spec was the implementation plan for v1.0. All lanes have been completed and shipped. See ROADMAP.md for the v1.0.0 changelog.

Herd v0.9.0 was the baseline — unified repo, 111 tests, Ollama routing working across the three-node GPU fleet (CITADEL-5090, minipc-4080, warden-4070). Benchmarking validated that llama-server delivers 44-80% faster TTFT and ~4x throughput vs Ollama on identical hardware. The v1.0 push added llama-server as a first-class backend, enriched telemetry with token-level tracking, and turned the dashboard into a full control plane with HuggingFace model search and download. Final test count: 189 tests.

## Enabling Agent Teams

```bash
# Requires Claude Code v2.1.32+ (current: v2.1.96)
export CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1
```

## Team Prompt

```
Create an agent team with 6 teammates to implement Herd v1.0. Each teammate owns one lane.
The shared codebase is at G:\Projects\herd. All work targets the `main` branch.

Read CLAUDE.md, ROADMAP.md, and docs/LLAMA_CPP_BACKEND.md before starting.
Read docs/specs/herd-tune-spec.md for the node registration and herd-tune architecture.
Read docs/specs/v1-agent-teams-spec.md for the full v1.0 specification.
Run `cargo test` before and after changes to ensure no regressions (189 tests as of v1.0.0).
Use conventional commits: feat:, fix:, refactor:, docs:, test:

== TEAMMATE 1: Telemetry Enrichment (analytics + metrics) ==

Owner: src/analytics.rs, src/metrics.rs, src/server.rs (proxy handler logging)

Goal: Extend request logging to capture token counts and per-model/per-backend latency
so the dashboard can show usage trends, cost estimation, and performance breakdowns.

Tasks:
1. Extend RequestLog struct with new fields:
   - tokens_in: Option<u32> (prompt tokens)
   - tokens_out: Option<u32> (completion/generation tokens)
   - tokens_per_second: Option<f32> (eval rate)
   - prompt_eval_ms: Option<u64> (time to first token / prompt processing)
   - eval_ms: Option<u64> (generation time)
   - backend_type: Option<String> ("ollama" or "llama-server")
   All fields Option to preserve backward compat with existing JSONL logs.

2. Extract token counts from upstream response bodies:
   - Ollama /api/generate: response JSON has prompt_eval_count, eval_count,
     prompt_eval_duration (nanoseconds), eval_duration (nanoseconds)
   - Ollama /api/chat: same fields in final response chunk (stream) or response body
   - llama-server /v1/chat/completions: usage.prompt_tokens, usage.completion_tokens
     in response body (non-streaming) or final SSE chunk (streaming)
   - For streaming responses, capture the final chunk that contains usage data.
     Buffer only the last SSE chunk, not the entire stream.

3. Extend AnalyticsStats with:
   - total_tokens_in: u64
   - total_tokens_out: u64
   - model_token_counts: HashMap<String, (u64, u64)> (model -> (tokens_in, tokens_out))
   - backend_latency: HashMap<String, LatencyPercentiles> (backend -> {p50, p95, p99})
   - model_latency: HashMap<String, LatencyPercentiles> (model -> {p50, p95, p99})
   - tokens_per_second_avg: f32
   - estimated_api_cost_usd: f32 (calculate based on model size tier:
     ≤8B = $0.10/M input, $0.30/M output;
     9-32B = $0.25/M input, $0.75/M output;
     33B+ = $0.50/M input, $1.50/M output)
     This is a rough "what you'd pay if this were an API" number for marketing.

4. Extend Prometheus metrics:
   - herd_tokens_total{direction="in|out", model="..."} counter
   - herd_tokens_per_second gauge (exponential moving average)
   - herd_request_duration_ms should gain model and backend labels

5. Extend /analytics API response to include the new fields.

6. Tests: deserialize old JSONL without new fields (backward compat), token extraction
   from mock Ollama and llama-server responses, cost estimation math, new Prometheus
   output format.

Coordinate with: Teammate 4 (Dashboard) for the new analytics data shapes.
Coordinate with: Teammate 6 (Router Integration) for where token extraction hooks
into the proxy handler.

== TEAMMATE 2: Node Data Layer ==

Owner: src/nodes/db.rs, src/nodes/types.rs, src/nodes/health.rs, src/nodes/mod.rs

Goal: Migrate the SQLite node registry to support llama-server backends with extended
GPU metadata, and make health polling backend-aware.

Tasks:
1. SQLite schema migration — add columns to `nodes` table:
   - backend TEXT NOT NULL DEFAULT 'ollama'
   - backend_version TEXT
   - gpu_vendor TEXT (nvidia, amd, intel, none)
   - gpu_model TEXT
   - gpu_backend TEXT (cuda, rocm, sycl, vulkan, cpu)
   - gpu_driver_version TEXT
   - cuda_version TEXT
   - max_context_len INTEGER DEFAULT 4096
   - model_paths TEXT DEFAULT '[]' (JSON array of GGUF paths)
   - capabilities TEXT DEFAULT '[]' (JSON array: cuda, flash_attn, moe, etc.)
   Use ALTER TABLE ADD COLUMN with defaults so existing DBs upgrade in place.

2. Update upsert_node() to handle both Ollama and llama-server registration payloads.
   The NodeRegistration struct in types.rs already has the fields — wire them to SQL.

3. Backend-aware health polling:
   - Ollama: GET {url}/api/ps (loaded models), GET {url}/api/tags (available models, less frequent)
   - llama-server: GET {url}/health, GET {url}/v1/models (loaded model)
   - Branch on node.backend field in the health check loop
   - Parse llama-server /health response for slot status if available

4. Add a model registry concept: for llama-server nodes, track which GGUF files exist
   on disk (reported by node agent) vs which are currently loaded. This enables the
   dashboard to show "available to load" vs "currently serving".

5. Tests: schema migration on empty DB, upsert with Ollama payload, upsert with
   llama-server payload, backward compat (old payload without backend field defaults
   to Ollama), health check path selection by backend type.

Coordinate with: Teammate 3 (herd-tune) for registration payload format.
Coordinate with: Teammate 5 (HF Search) for model registry integration.

== TEAMMATE 3: herd-tune Scripts ==

Owner: scripts/herd-tune.ps1, scripts/herd-tune.sh

Goal: Extend herd-tune to support --backend flag with GPU vendor detection,
llama-server binary download, and llama-server lifecycle management.

Tasks:
1. Add --backend parameter: ollama (default/legacy), llama-server, auto
   - auto: if Ollama is running locally, use it; otherwise set up llama-server

2. GPU vendor detection:
   - NVIDIA: parse nvidia-smi for GPU name, VRAM, driver version, CUDA version
   - AMD: rocm-smi or hipconfig for GPU arch (gfx110X, gfx120X)
   - Intel: sycl-ls or WMI/lspci for Arc GPUs
   - Fallback: Vulkan or CPU-only

3. CRITICAL — Blackwell detection:
   - Parse CUDA version and GPU architecture from nvidia-smi
   - RTX 5000-series (Blackwell, sm_120+) REQUIRES CUDA 13.x build
   - CUDA 12.x silently falls back to CPU with no error — llama-server starts fine
     but runs 10x slower. herd-tune must detect this and download the correct binary.

4. llama-server binary download:
   - Query llama.cpp GitHub releases API for latest build number
   - Select correct binary by platform + GPU vendor (see LLAMA_CPP_BACKEND.md matrix)
   - Download with progress bar and checksum verification
   - Support resume on interrupted downloads (Range header)
   - Install to a well-known location (~/.herd/bin/llama-server or similar)

5. llama-server launch:
   - Start with appropriate flags: -ngl 99, -c {context}, --port {port}, --model {path}
   - Detect available VRAM and suggest appropriate models/quant levels
   - Write a config file for restart (~/.herd/llama-server.conf or equivalent)

6. Extended registration payload — POST to Herd with:
   backend, backend_version, backend_url, gpu_vendor, gpu_model, gpu_backend,
   cuda_version, vram_mb, ram_mb, capabilities, model_paths, max_context_len

7. Daemon mode (stretch): herd-tune stays resident, exposes a small HTTP API on a
   local port for receiving commands from Herd (download model, restart llama-server,
   swap model). Register the daemon URL as part of the node registration payload.
   If not daemon mode, the dashboard control plane features (model swap, restart)
   won't work for llama-server nodes — document this limitation.

8. Both scripts (PS1 and bash) must support all the above. Keep feature parity.

Coordinate with: Teammate 2 (Data Layer) for registration payload schema.
Coordinate with: Teammate 5 (HF Search) if daemon mode is implemented — the daemon
needs a "download model" endpoint.

== TEAMMATE 4: Dashboard Control Plane ==

Owner: dashboard.html

Goal: Upgrade the dashboard from monitoring-only to a full control plane. Add
backend-awareness to the Fleet tab, create a Models tab with HuggingFace search
and download, and add telemetry visualizations.

Tasks:
1. Fleet tab upgrades:
   - Show backend type per node (badge: "Ollama" / "llama-server")
   - Display gpu_vendor, gpu_model, cuda_version, capabilities
   - Show backend_version (Ollama version or llama.cpp build number)
   - Update "Add Node" flow: add backend selector dropdown to script download
     (passes ?backend= param to script endpoint)
   - For llama-server nodes, show loaded model + available GGUFs on disk
   - Add "Restart" button for llama-server nodes (calls node agent if daemon mode)

2. New "Models" tab (between Fleet and Settings):
   - Search bar that queries HuggingFace for GGUF models (via Herd API)
   - Results show: model name, author, quant variants, file sizes, download count
   - "Fits on" indicator: compare GGUF size to each node's available VRAM
   - Download button per result: pick target node, show progress bar
   - Download progress via SSE or polling (similar to existing Ollama pull progress)
   - For Ollama nodes: trigger pull via existing /admin/backends/:name/pull
   - For llama-server nodes: trigger download via node agent daemon API
   - Show download queue / active downloads

3. Analytics tab upgrades (consumes Teammate 1's enriched data):
   - Token usage chart: tokens in/out over time, stacked by model
   - Cost estimation card: "Estimated API cost avoided: $X.XX"
   - Per-model latency breakdown: table with p50/p95/p99 per model
   - Per-backend latency breakdown: same, grouped by backend
   - Tokens/second trend line
   - Top models by token volume (not just request count)

4. Backends tab:
   - Show backend type badge on each card
   - For llama-server backends (from static config), show same extended info

5. Maintain the existing single-file dashboard.html architecture.
   Use the same dark theme, Chart.js, and CSS patterns. No external framework deps.
   Keep it under ~150KB.

Coordinate with: Teammate 1 (Telemetry) for analytics data shapes.
Coordinate with: Teammate 5 (HF Search) for search API response format.
Coordinate with: Teammate 2 (Data Layer) for fleet node response fields.

== TEAMMATE 5: HuggingFace Model Search API ==

Owner: src/api/ (new file: models.rs), Cargo.toml (if new deps needed)

Goal: Add a model search endpoint that queries HuggingFace for GGUF models,
and a model download endpoint that handles pulling GGUFs to target nodes.

Tasks:
1. GET /api/models/search?q={query}&quant={filter}&max_size_gb={limit}
   - Query HuggingFace API: https://huggingface.co/api/models?search={q}&filter=gguf
   - Parse results for GGUF files: extract quant type, file size, model card info
   - Return structured JSON:
     [{
       repo_id: "unsloth/gemma-4-26B-A4B-it-UD-Q4_K_M",
       author: "unsloth",
       model_name: "gemma-4-26B-A4B-it",
       quant: "Q4_K_M",
       file_name: "gemma-4-26B-A4B-it-UD-Q4_K_M.gguf",
       file_size_bytes: 15700000000,
       downloads: 12345,
       updated_at: "2026-04-01T...",
       fits_on: ["citadel-5090", "minipc-4080"]
     }]
   - The "fits_on" field compares file size to registered nodes' vram_mb
   - Cache search results briefly (5 min) to avoid hammering HF API

2. POST /api/nodes/{id}/models/download
   Body: { repo_id: "...", file_name: "...", target_path: "/models/..." }
   - For Ollama nodes: proxy to Ollama pull API (existing behavior)
   - For llama-server nodes with daemon: forward to node agent's download endpoint
   - For llama-server nodes without daemon: return 501 with helpful error
   - Stream progress back via SSE (NDJSON like existing pull endpoint)

3. GET /api/nodes/{id}/models
   - For Ollama nodes: proxy to GET /api/tags
   - For llama-server nodes: return model_paths from node registry + loaded status

4. DELETE /api/nodes/{id}/models/{model_name}
   - For Ollama nodes: proxy to DELETE /api/delete
   - For llama-server nodes with daemon: forward delete to node agent
   - For llama-server nodes without daemon: return 501

5. Model download with resume support:
   - Track partial downloads in SQLite (url, bytes_downloaded, total_bytes, target_path)
   - Resume from last byte using Range header
   - Handle HuggingFace Xet storage protocol if needed (some large GGUFs use it)

6. Tests: search result parsing, VRAM compatibility calculation, download resume logic.

Coordinate with: Teammate 4 (Dashboard) for API response shapes.
Coordinate with: Teammate 2 (Data Layer) for node VRAM data and model registry.
Coordinate with: Teammate 3 (herd-tune) for daemon download endpoint format.

== TEAMMATE 6: Router Integration ==

Owner: src/router/, src/backend/, src/server.rs, src/config.rs

Goal: Make the router fully backend-agnostic so llama-server and Ollama nodes
are treated identically at the routing layer, and wire token extraction into
the proxy handler for telemetry.

Tasks:
1. BackendType enum in config.rs:
   - Ensure Ollama, LlamaServer, and OpenAICompat variants exist
   - Add to YAML config parsing: backend: "ollama" | "llama-server" | "openai-compat"
   - Default to Ollama for backward compat

2. Health check abstraction in src/backend/:
   - HealthChecker trait or enum dispatch based on BackendType
   - Ollama: /api/ps for loaded models, /api/tags for available
   - llama-server: /health for status, /v1/models for loaded model
   - OpenAI-compat: /v1/models (generic)
   - ModelDiscovery should populate the same model list regardless of backend

3. Proxy handler token extraction (src/server.rs):
   - After proxying a request, inspect the response body for token counts
   - For non-streaming: parse JSON response before forwarding
   - For streaming: buffer only the final SSE chunk (contains usage data)
   - Pass extracted tokens to analytics.log_request()
   - CRITICAL: Do not buffer entire streaming responses. Only capture the last
     data: chunk for usage stats. The proxy must remain a true streaming proxy.

4. Model routing for llama-server nodes:
   - llama-server nodes serve one model at a time (static load)
   - model_aware strategy must check node registry for loaded model, not /api/ps
   - Add backend type to routing context so strategies can branch if needed

5. Config backward compat:
   - Static [[backends]] in YAML must still work for Ollama nodes
   - New optional fields: backend, gpu_vendor, capabilities per backend in YAML
   - Existing configs without these fields default to Ollama behavior

6. Tests: routing to llama-server node, routing to mixed fleet (Ollama + llama-server),
   health check dispatch by backend type, token extraction from mock responses,
   streaming proxy passthrough verification.

Coordinate with: Teammate 1 (Telemetry) for token extraction → analytics pipeline.
Coordinate with: Teammate 2 (Data Layer) for node registry integration with router.
```

## Shared Constraints

- All code in Rust (except dashboard.html and herd-tune scripts)
- No new crate dependencies without justification — prefer reqwest (already in tree)
  for HTTP calls to HuggingFace
- All new features default to enabled: false where sensible (zero overhead when off)
- All new config fields must have sensible defaults — don't break existing herd.yaml files
- JSONL analytics backward compat: new fields are Option, old logs still deserialize
- Tests for each feature: enabled/disabled, happy path, edge cases
- Conventional commits: feat:, fix:, refactor:, docs:, test:
- Target branch: main

## Definition of Done

- cargo test passes (baseline 111 + new tests)
- cargo build --release succeeds
- cargo clippy -- -D warnings passes
- Existing Ollama-only configs work unchanged
- Dashboard renders correctly with both Ollama and llama-server nodes
- ROADMAP.md updated with v1.0 items checked off

## File Ownership Summary

| Lane | Teammate | Primary Files |
|------|----------|---------------|
| Telemetry | 1 | analytics.rs, metrics.rs, server.rs (logging) |
| Data Layer | 2 | nodes/db.rs, nodes/types.rs, nodes/health.rs |
| herd-tune | 3 | scripts/herd-tune.ps1, scripts/herd-tune.sh |
| Dashboard | 4 | dashboard.html |
| HF Search | 5 | api/models.rs (new), api/mod.rs |
| Router | 6 | router/*, backend/*, server.rs (proxy), config.rs |

Potential merge conflicts: server.rs (Teammates 1, 6), api/mod.rs (Teammate 5 adds route).
Teammates 1 and 6 should coordinate on the proxy handler changes — Teammate 6 owns
the extraction logic, Teammate 1 owns the analytics structs it writes to.
