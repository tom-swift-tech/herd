# Pending Work — Herd + Herd-Pro

## Step 1: Herd — `/v1/chat/completions` endpoint
- [x] Read herd-pro's working implementation for reference
- [x] Implement POST /v1/chat/completions in src/api/openai.rs
- [x] Wire route in src/server.rs
- [x] Validate build (clean compile)
- [x] All 6 existing tests pass

## Step 2: Herd — Test coverage expansion (6 → 23 tests)
- [x] Router strategy tests (priority, model_aware, least_busy) — 8 tests
- [x] Backend pool tests (add/remove, health tracking, circuit breaker) — 5 tests
- [x] Weighted round-robin tests — 4 tests
- [x] All 23 tests pass

## Step 3: Herd — Weighted round-robin router (v0.3.0)
- [x] Implement weighted round-robin strategy (src/router/weighted_round_robin.rs)
- [x] Add to RouterEnum and RoutingStrategy
- [x] Add config support (`weighted_round_robin`)
- [x] Tests (4 tests: distribution, single backend, unhealthy skip, no healthy error)
- [x] Validate build

## Step 4: Herd-Pro — Dashboard sessions tab polish
- [x] Add authFetch helper for API key auth in dashboard
- [x] Add API key input to header (persisted in localStorage)
- [x] Fix session count label (active vs total)
- [x] Replace 9 fetch calls with authFetch for admin/agent endpoints
- [x] Validate build + all 61 tests pass

## Step 5: Validation & commit
- [x] Herd: 23/23 tests pass, clean build
- [x] Herd-Pro: 61/61 tests pass, clean build
- [ ] Commit changes

## Review
- Herd test coverage: 6 → 23 tests (3.8x increase)
- Herd features added: /v1/chat/completions, weighted_round_robin router
- Herd-Pro fix: dashboard auth for session/admin API calls
- Both repos compile and pass all tests
