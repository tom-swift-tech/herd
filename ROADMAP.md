# Herd Roadmap

**Updated:** March 7, 2026

## Vision

Herd is the **fastest way to route AI workloads across local Ollama backends**.

One fast, single Rust binary gives you:
- GPU-aware routing across multiple Ollama nodes
- Circuit breaker resilience with configurable failure thresholds
- Unified observability: metrics, analytics, and a live dashboard
- OpenAI-compatible API for drop-in compatibility

No cloud dependency. No API keys exposed. Full local control.

## Roadmap

### v0.2.1 — Security Hardening (Current)

- Configurable circuit breaker (failure threshold, recovery time)
- API key authentication for admin endpoints
- Proxy hardening (body size cap, header forwarding, query string preservation)
- Analytics race condition fix
- CLI backend specification parser
- Conditional route registration (admin API off by default)

### v0.3.0 — Routing & Reliability (Target: April 2026)

- ~~Retry loop with configurable attempt count~~ ✅ (shipped v0.2.1)
- ~~Request timeout enforcement per routing strategy~~ ✅ (shipped v0.2.1)
- ~~Weighted round-robin routing strategy~~ ✅
- ~~OpenAI `/v1/chat/completions` full compatibility layer~~ ✅ (pulled forward from v0.4.0)
- ~~Rate limiting (global token bucket)~~ ✅ (pulled forward from v0.5.0)
- ~~Model filter (regex-based per-backend)~~ ✅
- ~~Dashboard polish (stats, tabs, latency percentiles, mobile responsive)~~ ✅
- Backend tagging and tag-based routing
- Health check endpoint customization (configurable path and expected status)
- Hot-reload configuration without restart

### v0.4.0 — Observability & Operations (Q2 2026)

- Prometheus-native metrics export
- Request tracing with correlation IDs
- Log rotation and retention policies

### v0.5.0+ — Scale & Ecosystem (Q3 2026)

- Multi-node discovery (mDNS / static fleet config)
- TLS termination
- Rate limiting per client / API key
- Plugin system for custom routing strategies
- Distributed health consensus

## Get Involved

If you're interested in:
- Testing pre-release builds
- Contributing routing strategies or backend integrations
- Sharing real-world deployment patterns

...please open an issue or discussion.

— swift-innovate
