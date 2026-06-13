# Herd — Working TODO

> Scratchpad for in-flight work. Milestone tracking lives in `ROADMAP.md`;
> the v1.2 PR breakdown + acceptance checklist live in `tasks/HERD-V1.2-SPRINT.md`.

**Last updated:** 2026-06-13

---

## PLANNED (awaiting go-ahead) — v1.2 PR #8: in-process fleet routing integration test

Branch `feat/v1.2-pr8-integration-test` off `main` (`5a97eab`, PRs #1–#7 landed). Closes the
v1.2 fleet foundation: proves the WHOLE chain in one process (heartbeat HTTP → registry →
reconcile → pool → router → proxy → upstream) — the assertion no unit test makes today.

### Plan (~10 lines)
1. **Test seam (minimal, `#[doc(hidden)] pub`, no behavior change):** `tests/` is an external
   crate and can only see `pub`. Expose: `NodeRegistry::with_clock`, the `test_clock` module
   (`TestClock::new/advance/as_fn`) + the `Clock` alias, and `AgentPoolSync::reconcile`.
   Prompt mandates "advance test_clock" + "one reconcile" — these are the seam.
2. `tests/fleet_routing.rs`, mirroring `agent_daemon.rs` (stub server) + `frontier_escalation.rs`
   (hand-built `AppState`). Helper builds: stub upstream (ephemeral port) recording received
   `/v1/chat/completions` hits; `AppState` with in-memory `NodeDb`, ModelAware router, a shared
   registry+pool; serves `heartbeat` + `chat_completions` on an ephemeral port = "the gateway".
   No `server::run` (avoids real port/home-dir touch — that stays the `#[ignore]`d two-box test).
3. **T1 happy path:** POST heartbeat advertising the stub → `reconcile` → POST chat to gateway →
   assert 200 AND stub.received == 1 AND body round-trips (anti-trivial: stub MUST have been hit).
4. **T2 drain→503:** register, reconcile (present), `clock.advance(TTL+1)`, reconcile (empty) →
   POST chat → assert **503** status code (no hidden fallback, a real error).
5. **T3 model routing:** two agents / two models, reconcile, request model X → only stub-X received.
6. Determinism = `TestClock` + explicit `reconcile`, never sleep+margin. No `unwrap/expect` in any
   lib code touched (test code may unwrap). Existing tests stay green.
7. **Gates:** `cargo build` + `cargo test` (report lib subtotal vs all-binaries total separately —
   they differ, not a drop) + `clippy --all-targets -- -D warnings` + `fmt --check`.
8. **LEAD:** reconcile sprint doc (#8 ✅, v1.2 foundation complete) + ROADMAP; document the manual
   two-box acceptance in the PR body; open PR vs main. **STOP at the open PR — no auto-merge.**

### Roles
BUILDER: seam + `tests/fleet_routing.rs`. REVIEWER: independent pass — hunt list (any hit blocks):
(1) no wall-clock/margin timing; (2) anti-trivial — every test can FAIL (stub received assertion +
real 503); (3) no unwrap/expect in lib code touched. OPERATOR: gates, separate lib/total counts.
LEAD: docs + PR. Open PR then gets an INDEPENDENT review before merge.

### Rules
conventional commits; commit trailer
`Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`.

### Status
- [x] STOP for go-ahead → approved
- [x] Test seam (`#[doc(hidden)] pub`: `with_clock`/`Clock`, `reconcile`, `open_in_memory`)
- [x] tests/fleet_routing.rs (T1/T2/T3) — 3 tests green
- [x] Internal reviewer pass (hunt list) — CLEAN, no blocking findings, each test's failure mode traced
- [x] Gates green — build ✓, test 491 pass / 0 fail / 1 ignored (lib subtotal 459), clippy `-D warnings` ✓, fmt ✓
- [x] Docs reconciled (sprint #8 ✅ + v1.2 foundation complete; ROADMAP) + PR opened (no merge)
  - Open PR still gets an INDEPENDENT review before merge — internal pass is necessary, not sufficient.

---

## Backlog
- v1.1.1 / v1.1.2 not git-tagged (tags stop at v1.1.0); consider 1.2.0 bump + tags once #8 lands
  and the v1.2 foundation is complete.
