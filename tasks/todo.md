# Herd — Current Status

**Version:** 0.4.2
**Branch:** ollama-features
**Last pushed:** 2026-03-11

## Current Task: Ollama Node Management Features

### Features
1. **VRAM probing** — On first backend discovery, pull `llama3.2:3b`, run a small prompt, read VRAM usage from `/api/ps`, store as `vram_total` on BackendState
2. **Model listing in Edit modal** — Surface all models on the node, with delete buttons (calls Ollama `DELETE /api/delete`)
3. **Model pull UI** — Text input + "Pull" button in Edit modal, calls Ollama `POST /api/pull` with streaming progress

### Plan
- [ ] Add `vram_total_mb` field to BackendState and `vram_probed` flag
- [ ] Add VRAM probe logic in discovery: pull llama3.2:3b → generate small prompt → read VRAM from /api/ps → store
- [ ] Add admin API endpoints: `POST /admin/backends/:name/pull`, `DELETE /admin/backends/:name/models/:model`
- [ ] Update Edit modal in dashboard: show model list with delete buttons, add pull input with progress
- [ ] Run tests
- [ ] Commit

### Ollama API Reference
- `GET /api/tags` — list models (already used)
- `GET /api/ps` — running models with `size_vram` field
- `POST /api/pull` — `{"name":"model"}`, streams `{"status":"...","total":N,"completed":N}`
- `DELETE /api/delete` — `{"name":"model"}`
- `POST /api/generate` — `{"model":"...","prompt":"..."}` for VRAM test

## Parked: GitHub Sponsors → Herd-Pro Access
## Completed: v0.2.1, v0.3.0, v0.4.0/v0.4.1, v0.4.2
