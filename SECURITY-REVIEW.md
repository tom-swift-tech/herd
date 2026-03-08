# Security Review: Herd

**Project:** Herd — Intelligent Ollama Router
**Type:** Tier 3 (Experiments)
**Review Date:** 2026-03-01
**Reviewer:** Mira

## Executive Summary

Herd is a Rust-based HTTP proxy/router for Ollama nodes. It accepts HTTP requests and forwards them to Ollama backends. No secrets are stored, but it proxies requests that may contain user data.

**Verdict:** ⚠️ **NEEDS REVIEW** — Rust not installed, cannot run `cargo audit`. Manual review completed, automated scans pending.

---

## 1. Secrets & Credentials

### Code Review

| Check | Status | Notes |
|-------|--------|-------|
| Hardcoded API keys | ✅ PASS | No API keys in code |
| Hardcoded passwords | ✅ PASS | No passwords in code |
| .env files committed | ✅ PASS | No .env files |
| .gitignore excludes secrets | ✅ PASS | .gitignore includes `*.yaml` (excludes configs) |

### Findings

**✅ PASS** — No hardcoded secrets found in code review. Configuration files use YAML with placeholders.

---

## 2. Dependencies

### Automated Scan

| Check | Status | Notes |
|-------|--------|-------|
| `cargo audit` | ✅ PASS | 1 warning (unmaintained rustls-pemfile, not a vulnerability) |
| Dependency versions pinned | ✅ PASS | Cargo.lock generated |

### Action Required

Install Rust on build machine:
```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
cargo install cargo-audit
cd /home/mira/.openclaw/workspace/projects/herd
cargo audit
```

---

## 3. Authentication & Authorization

### Current State

| Check | Status | Notes |
|-------|--------|-------|
| Auth required for operations | ✅ PASS | API key auth via X-API-Key header or Bearer token |
| Rate limiting | ✅ PASS | Global token-bucket rate limiter (configurable) |
| Admin API protection | ✅ PASS | Requires API key (constant-time comparison) |

### Findings

**✅ PASS — Authentication Implemented (v0.2.1):**

Herd now requires API key authentication for admin endpoints:
- `/admin/backends/*` — CRUD operations
- `/admin/reload` — Config hot-reload
- Supports `X-API-Key` header and `Authorization: Bearer <key>`
- Uses constant-time comparison to prevent timing attacks
- Rate limiting via configurable token-bucket (requests/sec)

### Recommendations

1. ~~Add authentication~~ ✅ Done (v0.2.1)
2. ~~Add rate limiting~~ ✅ Done (v0.2.1)
3. **Network isolation** — Only expose on internal network (Tailscale)
4. **Future: Per-client rate limiting** — Rate limit per API key (planned v0.5.0)

---

## 4. Input Validation

| Check | Status | Notes |
|-------|--------|-------|
| URL validation | ⚠️ WARN | Backend URLs not validated |
| Request sanitization | ✅ PASS | Requests forwarded as-is (proxy behavior) |
| Model name validation | ✅ PASS | Model names passed through to Ollama |

### Findings

**⚠️ WARNING — No URL Validation:**

Backend URLs from config are used directly without validation. Malicious config could point to arbitrary URLs.

**Recommendation:** Validate URLs are HTTP(S) and to expected hosts.

---

## 5. Data Protection

| Check | Status | Notes |
|-------|--------|-------|
| PII in logs | ⚠️ WARN | Tracing enabled, may log request bodies |
| HTTPS enforcement | ❌ FAIL | **HTTP only** — runs on port 40114 unencrypted |
| CORS configuration | ✅ PASS | No CORS middleware (internal use only) |

### Findings

**⚠️ WARNING — HTTP Only:**

Herd runs HTTP on port 40114. For production:
- Put behind reverse proxy with TLS (Caddy/nginx)
- Or add TLS support directly

**⚠️ WARNING — Logging Request Data:**

`TraceLayer` may log request bodies containing user data.

**Recommendation:** Filter sensitive fields from logs.

**✅ PASS — Log Rotation (v0.4.0):**

Log files are now managed with configurable rotation and retention:
- Size-based rotation (`log_max_size_mb`, default 100 MB)
- Rotated file count limit (`log_max_files`, default 5)
- Time-based retention (`log_retention_days`, default 7 days)
- Prevents unbounded disk usage from request logging

**✅ PASS — Request Correlation (v0.4.0):**

All requests are tagged with `X-Request-Id` (UUID v4) for end-to-end tracing.
Request IDs are included in JSONL analytics logs. Note: correlation IDs may be
logged alongside request metadata — ensure log access is restricted.

---

## 6. Container Security

| Check | Status | Notes |
|-------|--------|-------|
| Trivy scan | ⏳ PENDING | No container built yet |
| Non-root user | ⚠️ WARN | **Dockerfile runs as root** |
| Secrets in Dockerfile | ✅ PASS | No secrets |
| Minimal base image | ⚠️ WARN | Using `debian:bookworm-slim` (larger than alpine) |

### Findings

**⚠️ WARNING — Running as Root:**

```dockerfile
ENTRYPOINT ["herd"]
# No USER directive — runs as root by default
```

**Recommendation:**
```dockerfile
RUN useradd -r -s /bin/false herd
USER herd
ENTRYPOINT ["herd"]
```

---

## 7. API Security

| Check | Status | Notes |
|-------|--------|-------|
| Rate limiting | ✅ PASS | Token-bucket, configurable requests/sec |
| Auth middleware | ✅ PASS | API key auth for admin endpoints |
| Error messages | ✅ PASS | Generic errors, no stack traces |
| CORS | ✅ PASS | Not configured (internal use) |

---

## Summary

| Category | Status | Blocker? |
|----------|--------|----------|
| Secrets | ✅ PASS | No |
| Dependencies | ⏳ PENDING | Needs `cargo audit` |
| Authentication | ✅ PASS | API key auth for admin/agent endpoints |
| Input Validation | ⚠️ WARN | Acceptable for MVP |
| Data Protection | ⚠️ WARN | HTTP only, logging concerns (log rotation added v0.4.0) |
| Container | ⚠️ WARN | Runs as root |
| API Security | ✅ PASS | Auth + rate limiting implemented |

---

## Verdict

**For Tier 3 (Experiments):** ✅ **ACCEPTABLE**

Herd is suitable for internal use on trusted networks (Tailscale). Do NOT expose to public internet.

**For Tier 1 (SaaS):** ❌ **BLOCKED**

Required before deployment:
1. ~~Install Rust, run `cargo audit`~~ ✅ DONE
2. ~~Add authentication for `/admin/*` endpoints~~ ✅ Done
3. Run as non-root user in container
4. ~~Add rate limiting~~ ✅ Done
5. Put behind TLS proxy

---

## Action Items

1. [x] Install Rust on build machine
2. [x] Run `cargo audit` — 1 warning (unmaintained dep, no vulns)
3. [ ] Add non-root user to Dockerfile
4. [ ] Deploy behind Caddy/nginx with TLS
5. [x] Add API key auth for admin endpoints ✅ Done (v0.2.1)
6. [x] Add rate limiting ✅ Done (v0.2.1)

---

**Reviewer:** Mira
**Date:** 2026-03-01
**Next Review:** Before promoting to Tier 2