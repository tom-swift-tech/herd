# Herd-Tune: Node Auto-Registration for Herd

## Overview

Herd needs a frictionless way for operators to add backend nodes to their fleet. herd-tune supports two backend types:

- **Ollama** — probes existing Ollama instance, configures env vars, registers with Herd
- **llama-server** — detects GPU vendor, downloads correct llama-server binary, starts it, registers with Herd

The workflow:

1. Operator opens the Herd dashboard in a browser (from any machine, including the node itself)
2. Clicks "Add Node" — dashboard offers a download of the appropriate `herd-tune` script (PowerShell for Windows, bash for Linux), pre-configured with this Herd instance's registration endpoint
3. Operator runs the script on the target node
4. Script detects local GPU/VRAM/RAM, sets up the chosen backend, and POSTs a registration payload to Herd
5. Node appears in the dashboard fleet view immediately

No SSH. No file uploads. No manual config editing. The script is the only thing that touches the node. Herd only ever talks to nodes via their OpenAI-compatible HTTP API.

## Architecture

```
[Herd Dashboard]
  │
  ├── GET /dashboard/add-node          → "Add Node" page with download buttons
  ├── GET /api/nodes/script?os=windows → Returns herd-tune.ps1 with endpoint baked in
  ├── GET /api/nodes/script?os=linux   → Returns herd-tune.sh with endpoint baked in
  │
  ├── POST /api/nodes/register         → Receives node registration from herd-tune
  ├── GET /api/nodes                   → List all registered nodes
  ├── GET /api/nodes/:id               → Single node detail
  ├── PUT /api/nodes/:id               → Update node (priority, tags, enabled)
  ├── DELETE /api/nodes/:id            → Remove node from fleet
  │
  └── Background: health poller polls each node's API on a per-backend schedule
```

## Backend Detection Logic (herd-tune)

herd-tune runs the following detection sequence on the target node:

### 1. GPU Vendor Detection

| Vendor | Detection Method | Backend |
|--------|-----------------|---------|
| NVIDIA | `nvidia-smi` → parse GPU name, VRAM, driver version, CUDA version | CUDA build of llama-server |
| AMD | `rocm-smi` or `hipconfig` → parse GPU arch (gfx110X, gfx120X, etc.) | ROCm build of llama-server |
| Intel | `sycl-ls` or WMI/lspci for Arc GPUs | SYCL build of llama-server |
| None / CPU only | Fallback | Vulkan build or CPU-only llama-server |

### 2. CUDA Version Selection (NVIDIA only)

**Critical:** Blackwell GPUs (RTX 5090, 5080, etc.) require CUDA 13.x. CUDA 12.x silently falls back to CPU with no error — llama-server starts, loads the model to CPU RAM, and runs 10x slower. herd-tune must:

1. Parse CUDA version from `nvidia-smi` output
2. Parse GPU architecture from driver info
3. If Blackwell (compute capability 12.x or sm_120+) → require CUDA 13 build
4. Otherwise → CUDA 12.4 build is fine

### 3. llama-server Binary Selection

Download the correct pre-built binary from llama.cpp GitHub releases:

| Platform | GPU | Binary |
|----------|-----|--------|
| Windows x64 | NVIDIA (non-Blackwell) | `llama-b{ver}-bin-win-cuda-cu12.4-x64.zip` |
| Windows x64 | NVIDIA (Blackwell) | `llama-b{ver}-bin-win-cuda-cu13.x-x64.zip` |
| Windows x64 | AMD | `llama-b{ver}-bin-win-hip-x64.zip` or AMD's validated binary from rocm.docs.amd.com |
| Windows x64 | Intel | `llama-b{ver}-bin-win-sycl-x64.zip` |
| Windows x64 | CPU/Vulkan | `llama-b{ver}-bin-win-vulkan-x64.zip` |
| Linux x64 | NVIDIA | `llama-b{ver}-bin-ubuntu-x64-cuda-cu12.4.tar.gz` (or cu13) |
| Linux x64 | AMD | `llama-b{ver}-bin-ubuntu-x64-rocm-7.x.tar.gz` or lemonade-sdk nightly |
| Linux x64 | Intel | Build from source with `-DGGML_SYCL=ON` (no reliable pre-built Linux binaries) |

### 4. Backend Mode Selection

herd-tune supports a `--backend` flag:

- `--backend ollama` — legacy mode, probes/configures existing Ollama (current behavior)
- `--backend llama-server` — downloads and starts llama-server
- `--backend auto` (default) — if Ollama is running locally, use it; otherwise set up llama-server

## API Endpoints

### POST /api/nodes/register

Called by `herd-tune` after local detection. Registers or updates a node.

**Request:**
```json
{
  "hostname": "citadel",
  "backend": "llama-server",
  "backend_version": "b8678",
  "backend_url": "http://192.168.1.100:8090",
  "backend_port": 8090,
  "gpu_vendor": "nvidia",
  "gpu_model": "NVIDIA GeForce RTX 5090",
  "gpu_backend": "cuda",
  "gpu_driver_version": "591.86",
  "cuda_version": "13.1",
  "vram_mb": 32768,
  "ram_mb": 131072,
  "models_loaded": ["gemma-4-26B-A4B-it-UD-Q4_K_M.gguf"],
  "model_paths": ["/models/gemma-4-26B-A4B-it-UD-Q4_K_M.gguf"],
  "capabilities": ["cuda", "flash_attn", "moe"],
  "max_context_len": 4096,
  "herd_tune_version": "0.4.0",
  "os": "windows",
  "registered_at": "2026-04-08T14:30:00Z"
}
```

For Ollama backends, the payload remains backward-compatible:
```json
{
  "hostname": "minipc",
  "backend": "ollama",
  "backend_url": "http://192.168.1.101:11434",
  "gpu_vendor": "nvidia",
  "gpu_model": "NVIDIA GeForce RTX 4080",
  "vram_mb": 16384,
  "ram_mb": 65536,
  "ollama_version": "0.16.1",
  "models_available": 42,
  "models_loaded": ["qwen3:32b", "gemma3:27b"],
  "recommended_config": {
    "num_parallel": 8,
    "max_loaded_models": 4,
    "flash_attention": true,
    "kv_cache_type": "q8_0",
    "context_length": 16384
  },
  "config_applied": true,
  "herd_tune_version": "0.4.0",
  "os": "linux",
  "registered_at": "2026-04-08T14:30:00Z"
}
```

**Response (201 Created or 200 OK if re-registering):**
```json
{
  "id": "node-uuid-here",
  "hostname": "citadel",
  "status": "registered",
  "message": "Node registered successfully. Health polling started."
}
```

**Behavior:**
- If a node with the same `hostname` already exists, update it (re-registration is idempotent)
- Start health polling immediately on successful registration
- Health check path depends on backend: Ollama uses `/api/ps`, llama-server uses `/v1/models`
- Store in SQLite (consistent with Herd's existing data layer)

### GET /api/nodes/script?os={windows|linux}&backend={ollama|llama-server|auto}

Returns the `herd-tune` script with the Herd registration endpoint and backend preference pre-configured.

### Other endpoints

Unchanged from current spec:
- `GET /api/nodes` — list all registered nodes
- `GET /api/nodes/:id` — single node detail
- `PUT /api/nodes/:id` — update node (priority, tags, enabled)
- `DELETE /api/nodes/:id` — remove node from fleet

## Health Polling

After registration, Herd polls each node on a configurable interval (default 10s). The health check path depends on the backend type:

### Ollama Nodes
1. `GET {backend_url}/api/ps` → loaded models, VRAM usage
2. `GET {backend_url}/api/tags` → available models (less frequent, every 60s)

### llama-server Nodes
1. `GET {backend_url}/v1/models` → loaded model(s)
2. `GET {backend_url}/health` → server health status (llama-server provides this)
3. `GET {backend_url}/metrics` → Prometheus metrics (optional, for detailed monitoring)

### Node Status States
- `healthy` — responding, models loaded
- `degraded` — responding but high latency or VRAM pressure
- `unreachable` — failed health checks (after 3 consecutive failures)
- `disabled` — operator manually disabled via dashboard

Unreachable nodes are not removed — they stay registered but excluded from routing. They automatically return to `healthy` when they start responding again.

## Data Storage

Updated `nodes` table schema to support both backends:

```sql
CREATE TABLE IF NOT EXISTS nodes (
    id TEXT PRIMARY KEY,
    hostname TEXT NOT NULL UNIQUE,
    backend TEXT NOT NULL DEFAULT 'ollama',       -- 'ollama' or 'llama-server'
    backend_version TEXT,                          -- ollama version or llama.cpp build number
    backend_url TEXT NOT NULL,                     -- unified: replaces ollama_url
    backend_port INTEGER DEFAULT 11434,
    gpu_vendor TEXT,                               -- 'nvidia', 'amd', 'intel', 'none'
    gpu_model TEXT,
    gpu_backend TEXT,                              -- 'cuda', 'rocm', 'sycl', 'vulkan', 'cpu'
    gpu_driver_version TEXT,
    cuda_version TEXT,                             -- NULL for non-NVIDIA
    vram_mb INTEGER DEFAULT 0,
    ram_mb INTEGER DEFAULT 0,
    max_concurrent INTEGER DEFAULT 1,
    max_context_len INTEGER DEFAULT 4096,
    os TEXT,
    status TEXT DEFAULT 'healthy',
    priority INTEGER DEFAULT 10,
    enabled INTEGER DEFAULT 1,
    tags TEXT DEFAULT '[]',
    models_available INTEGER DEFAULT 0,
    models_loaded TEXT DEFAULT '[]',
    model_paths TEXT DEFAULT '[]',                 -- llama-server: paths to GGUF files
    capabilities TEXT DEFAULT '[]',                -- ['cuda', 'flash_attn', 'moe', etc.]
    recommended_config TEXT DEFAULT '{}',
    config_applied INTEGER DEFAULT 0,
    last_health_check TEXT,
    registered_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
```

## Integration with Routing

The node registry is backend-agnostic at the routing layer. Herd's router:

1. Queries `nodes` table for `enabled = 1 AND status IN ('healthy', 'degraded')`
2. All nodes expose OpenAI-compatible `/v1/chat/completions` — Ollama at `/v1/chat/completions`, llama-server at `/v1/chat/completions` (same path)
3. Uses `max_concurrent` to know how many parallel slots each node has
4. Uses health poll data (loaded models, response latency) for model affinity and least-loaded routing
5. Respects `priority` for tie-breaking

Static `[[backends]]` in YAML config still works for non-herd-tune backends.

## Implementation Order

1. **SQLite `nodes` table migration** — add new columns (backend, gpu_vendor, gpu_backend, etc.)
2. **POST /api/nodes/register** — accept both Ollama and llama-server payloads
3. **GET /api/nodes** — return backend type in response
4. **Health poller** — branch on `backend` field for health check path
5. **Router integration** — read from `nodes` table, route to `backend_url + /v1/chat/completions`
6. **herd-tune scripts** — add `--backend` flag, GPU vendor detection, llama-server binary download
7. **Script template endpoint** — add `backend` query param
8. **Dashboard UI** — show backend type per node, add backend selector to "Add Node" flow
9. **PUT/DELETE endpoints** — unchanged
10. **Model management** — for llama-server nodes, `herd search` and model download commands

## Notes

- Ollama support is NOT being removed. Both backends coexist indefinitely.
- Registration is idempotent. Running `herd-tune` again updates the node entry.
- For llama-server nodes, model loading is static (specified at llama-server launch) vs Ollama's dynamic model loading. Herd needs to know this distinction for model routing.
- llama-server's prompt cache feature is highly relevant for VALOR operatives — repeated system prompts get cached, dramatically reducing TTFT on subsequent turns.
- Blackwell NVIDIA GPUs (RTX 5000-series) REQUIRE CUDA 13.x builds. CUDA 12.x silently falls back to CPU. herd-tune must detect this.
- Intel SYCL backend is functional but has rough edges (flash attention bugs on Xe2, volunteer-maintained). Treat as beta tier in herd-tune.
- The scripts should detect the backend URL on the local machine and prefer Tailscale IP > LAN IP > localhost for the `backend_url` field.
