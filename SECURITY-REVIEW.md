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
| Auth required for operations | ⚠️ WARN | **No authentication implemented** |
| Rate limiting | ❌ FAIL | **No rate limiting implemented** |
| Admin API protection | ⚠️ WARN | `/admin/*` endpoints are open |

### Findings

**⚠️ WARNING — No Authentication:**

Herd currently has NO authentication. Anyone who can reach the server can:
- Route requests to any backend
- Add/remove backends via `/admin/backends`
- View all backend status via `/status`

### Recommendations

1. **Add authentication** — Require API key for `/admin/*` endpoints
2. **Add rate limiting** — Prevent abuse of proxy
3. **Network isolation** — Only expose on internal network (Tailscale)
4. **Future: Add auth middleware** — Tower middleware for bearer tokens

**For Tier 3 (Experiments):** Acceptable for now. **For Tier 1 (SaaS):** BLOCKER.

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
| Rate limiting | ❌ FAIL | Not implemented |
| Auth middleware | ❌ FAIL | Not implemented |
| Error messages | ✅ PASS | Generic errors, no stack traces |
| CORS | ✅ PASS | Not configured (internal use) |

---

## Summary

| Category | Status | Blocker? |
|----------|--------|----------|
| Secrets | ✅ PASS | No |
| Dependencies | ⏳ PENDING | Needs `cargo audit` |
| Authentication | ⚠️ WARN | **No auth** — OK for internal, blocker for public |
| Input Validation | ⚠️ WARN | Acceptable for MVP |
| Data Protection | ⚠️ WARN | HTTP only, logging concerns |
| Container | ⚠️ WARN | Runs as root |
| API Security | ❌ FAIL | No rate limiting, no auth |

---

## Verdict

**For Tier 3 (Experiments):** ✅ **ACCEPTABLE**

Herd is suitable for internal use on trusted networks (Tailscale). Do NOT expose to public internet.

**For Tier 1 (SaaS):** ❌ **BLOCKED**

Required before deployment:
1. ~~Install Rust, run `cargo audit`~~ ✅ DONE
2. Add authentication for `/admin/*` endpoints
3. Run as non-root user in container
4. Add rate limiting
5. Put behind TLS proxy

---

## Action Items

1. [x] Install Rust on build machine
2. [x] Run `cargo audit` — 1 warning (unmaintained dep, no vulns)
3. [ ] Add non-root user to Dockerfile
4. [ ] Deploy behind Caddy/nginx with TLS
5. [ ] Add API key auth for admin endpoints (when promoted to Tier 1/2)

---

**Reviewer:** Mira
**Date:** 2026-03-01
**Next Review:** Before promoting to Tier 2