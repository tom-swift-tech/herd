# Herd + llama.cpp: Backend Strategy

**Date:** 2026-04-08
**Status:** Implemented in v1.0.0

## Summary

Benchmarking on CITADEL (RTX 5090) proved that Ollama's Go serving layer adds massive overhead compared to raw llama-server. On identical hardware and the same model (Gemma 4 26B-A4B, Q4_K_M), llama-server delivers 44-80% faster TTFT and ~4x throughput. This validates Herd's long-term direction: replace Ollama as the per-node inference backend with llama-server, while keeping Herd as the fleet router.

## Benchmark Results

**Hardware:** RTX 5090 (32GB VRAM), CITADEL, Windows 11
**Model:** Gemma 4 26B-A4B (MoE, 4B active params) — unsloth UD-Q4_K_M (15.7GB)
**llama.cpp build:** b8678 (CUDA 13.1)
**Ollama:** warmed model, v1 API

| Test | Ollama TTFT | llama.cpp TTFT | Delta |
|------|-------------|----------------|-------|
| Cold single-shot | 6,812 ms | 3,781 ms | -44.5% |
| Heavy system prompt (~1.5k tokens) | 14,331 ms | 5,574 ms | -61.1% |
| Multi-turn (5 rounds) | 13,832 ms | 5,576 ms | -59.7% |
| Concurrent (4 clients) | 40,980 ms | 8,282 ms | -79.8% |
| Long generation (1024 tokens) | 20,474 ms | 8,257 ms | -59.7% |
| Code generation | 20,528 ms | 7,058 ms | -65.6% |

**Throughput (where measured):** Ollama ~35 tok/sec vs llama-server ~145 tok/sec (~4x)

**Average TTFT improvement: -61.7%**

Concurrent load is the killer stat for VALOR: 41 seconds vs 8 seconds average TTFT when 4 operatives hit the same node simultaneously.

Note: streaming token counter in the benchmark harness had a bug — several tests reported 0 tokens. TTFT measurements are valid (based on first SSE chunk timing). Benchmark script and raw JSON results are in `G:\Projects\SIT\llm-benchmark\`.

## Fox Engine Evaluation (ferrumox/fox)

Evaluated Fox as an intermediate step. Conclusion: **not ready, but architecturally interesting.**

**What worked:**
- `fox search` — HuggingFace model search from CLI is excellent UX
- `fox pull` — model download with quant selection (but no resume on failure)
- OpenAI-compatible API, Ollama-compatible API (dual compat)
- Prometheus metrics built in

**What didn't work:**
- Windows CUDA build from source fails (llama.dll linking issues)
- `fox pull` drops connection on large files (no resume, error 10054)
- Memory estimator is broken for MoE models — refused to load a 15.7GB model with 26.8GB free VRAM, even with --max-context-len 2048 and --gpu-memory-fraction 0.5
- llama.cpp submodule pinned at bc05a68 — likely predates Gemma 4 MoE support

**Fox ideas worth stealing for Herd:**
- Model search CLI (`herd search <query>`)
- Prometheus metrics at `/metrics` (already in Herd roadmap)
- Dual OpenAI + Ollama API compat at the server level

## Fleet Architecture: Herd + llama-server

### Before (v0.9.0)
```
[Herd Router] --HTTP--> [Ollama node 1]
              --HTTP--> [Ollama node 2]
              --HTTP--> [Ollama node 3]
```

### Now (v1.0.0)
```
[Herd Router] --HTTP/OpenAI--> [llama-server node 1 (CUDA)]
              --HTTP/OpenAI--> [llama-server node 2 (CUDA)]
              --HTTP/OpenAI--> [Ollama node 3]
              --HTTP/OpenAI--> [llama-server node 4 (ROCm)]
              --HTTP/OpenAI--> [llama-server node 5 (SYCL)]
              --HTTP/OpenAI--> [llama-server node 6 (Vulkan)]
```

Herd doesn't care about GPU vendor — it talks OpenAI-compatible HTTP to each node's llama-server. Backend complexity (CUDA vs ROCm vs SYCL) is pushed to node setup via herd-tune.

### Fleet Modes

**Standalone mode** (current): Single Herd instance routes to local or remote Ollama/llama-server endpoints.

**Fleet mode** (new): One Herd instance is the host. Each additional node runs llama-server with the appropriate GPU backend. herd-tune detects hardware, downloads the correct llama-server binary, and registers with the host.

### Node Setup (herd-tune changes)

herd-tune currently assumes Ollama. For llama-server backends, herd-tune needs to:

1. **Detect GPU vendor** — NVIDIA (nvidia-smi), AMD (rocm-smi / hipconfig), Intel (sycl-ls), or CPU-only
2. **Select correct llama-server binary:**
   - NVIDIA: CUDA 12.x or CUDA 13.x (Blackwell/5000-series needs 13.x — silent CPU fallback if wrong!)
   - AMD: ROCm build matching gfx arch (gfx110X for RDNA3, gfx120X for RDNA4, etc.)
   - Intel: SYCL build (functional but rough — see caveats below)
   - Fallback: Vulkan (universal, slower) or CPU
3. **Download and verify binary** — from llama.cpp GitHub releases or AMD's ROCm repo
4. **Probe VRAM** — determine which models fit, suggest quant levels
5. **Start llama-server** — with correct flags (-ngl 99, -c context, --port)
6. **Register with Herd host** — POST node capabilities including GPU vendor, VRAM, backend type

### Registration Payload Extension

Add to the existing POST /api/nodes/register:

```json
{
  "hostname": "minipc",
  "backend": "llama-server",
  "backend_version": "b8678",
  "gpu_vendor": "nvidia",
  "gpu_model": "NVIDIA GeForce RTX 4080",
  "gpu_backend": "cuda",
  "cuda_version": "12.4",
  "vram_mb": 16384,
  "ram_mb": 65536,
  "llama_server_url": "http://192.168.1.101:8090",
  "llama_server_port": 8090,
  "models_loaded": ["gemma-4-26B-A4B-it-UD-Q4_K_M.gguf"],
  "model_paths": ["/models/gemma-4-26B-A4B-it-UD-Q4_K_M.gguf"],
  "capabilities": ["cuda", "flash_attn", "moe"]
}
```

## GPU Backend Support Matrix

| GPU Vendor | Backend | llama.cpp Support | Maturity | Notes |
|------------|---------|-------------------|----------|-------|
| NVIDIA (Ampere, Ada, Blackwell) | CUDA | Full, pre-built binaries | Production | Blackwell (5000-series) requires CUDA 13.x — 12.x silently falls back to CPU |
| AMD Radeon (RDNA3+) | ROCm/HIP | Full, pre-built binaries (Windows + Linux) | Solid | Nightly builds from lemonade-sdk. AMD ships validated binaries. RPC multi-node tested at scale. |
| AMD Instinct (MI300X, MI325X) | ROCm/HIP | Full, Docker images from AMD | Production | Officially supported by AMD ROCm team |
| Intel Arc (A-series, B-series) | SYCL | Supported, pre-built Windows binaries | Beta | Maintained by volunteer contributors. B580 performance well below theoretical. Flash attention broken on Xe2 iGPUs. IPEX-LLM archived Jan 2026. |
| Intel Data Center (Max, Flex) | SYCL | Supported | Functional | Better optimized than consumer Arc |
| Any (universal fallback) | Vulkan | Supported | Functional | ~25% slower than native backends but zero vendor-specific setup |

## Model Download Lessons Learned

From the Fox eval and HuggingFace download testing:

1. **Xet Storage is the new default** — major GGUF providers host on HF Xet. Need `hf_xet` package or Xet protocol support in Rust.
2. **Resume is mandatory** — 17GB files will drop. Fox's `fox pull` failed twice. Track partial downloads, resume from last byte.
3. **hf_transfer is fragile** — Rust-based fast downloader, but wheel availability lags Python releases (broken on 3.13). Feature-flag it.
4. **Model path flexibility** — don't assume cache layouts. Take a direct GGUF path. Normalize on filename or SHA256, not display names.
5. **Ollama blob extraction** — existing Ollama models can be reused: read manifest at `{OLLAMA_MODELS}/manifests/registry.ollama.ai/library/{model}/{tag}`, find the layer with `mediaType: application/vnd.ollama.image.model`, blob file is the raw GGUF.
6. **Quant naming is a mess** — `UD-Q4_K_M` vs `Q4_K_M` vs Ollama's internal tags. Store quant metadata as structured fields, don't trust display names.

## VALOR Relevance

Prefix caching (available in llama-server) is directly relevant to VALOR operatives. Every operative hits the same system prompts and Engram context on every call — that's exactly the workload where prefix caching pays off hardest. The benchmark's multi-turn test showed a 60% TTFT reduction with context reuse.

## TODO

- [x] Update herd-tune spec to support llama-server backend detection and setup
- [x] Add `backend` field to node registration payload
- [x] Implement GPU vendor detection in herd-tune (nvidia-smi / rocm-smi / sycl-ls)
- [x] Build llama-server binary download + verification into herd-tune
- [x] Evaluate CUDA 13.x detection logic for Blackwell GPUs in herd-tune
- [x] Add `herd search` API endpoint (inspired by Fox's model search UX) -- available at `GET /api/models/search`
- [ ] Test ROCm backend on an AMD node (minipc candidate if swapped to AMD GPU)
- [ ] Fix streaming token counter in benchmark harness for complete throughput data
- [ ] Investigate llama.cpp RPC for tensor-parallel sharding across fleet nodes (v2.0+)
