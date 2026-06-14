# Herd — Working TODO

> Scratchpad for in-flight work. Milestone tracking lives in `ROADMAP.md`;
> the v1.2 PR breakdown + acceptance checklist live in `tasks/HERD-V1.2-SPRINT.md`.

**Last updated:** 2026-06-13

---

## ACTIVE — Smart-routing scorer sprint (Phase 0 + Phase 1, two sequential PRs)

Foundation on `main` is `e51d80e` (#8 merge confirmed as tip). Full design lives in
`docs/specs/smart-routing-scorer-spec.md` (architect). 23-dimension / 4-phase rollout;
**this sprint ships Phase 0 + Phase 1 only**, phases 2–4 documented for later.

Baseline (on `main`, pre-sprint): lib **459** / total **491** (1 ignored). Build/clippy/fmt green.

Phases are SEQUENTIAL, **two separate PRs**. PR A (Phase 0) merges to main BEFORE PR B (Phase 1)
opens; PR B branches off updated main. **STOP at each open PR — no auto-merge.** Each PR also
gets an INDEPENDENT outside review (the in-session reviewer shares the builder's blind spots).

### Phase 0 (PR A) — telemetry to the pool boundary
Branch `feat/v1.2-scorer-pr-a-telemetry` off `main`.
- [x] ARCHITECT: wrote `docs/specs/smart-routing-scorer-spec.md` (full design, 23 dims / phases 0–4).
- [x] BUILDER: extended `BackendState` with the four `Option` fields (init `None` in `new()`/`add()`);
      `pool_sync.reconcile` populates queue_depth/ttft_p50_ms/vram_free_mb for `agent:` entries on BOTH
      add & update branches (add branch via new `set_agent_telemetry` setter); `max_concurrent` stays `None`.
      Routers untouched (`least_busy_cmp` tweak deliberately SKIPPED to keep "existing routers unchanged" literal).
- [x] Tests: +3 lib tests — new-agent carries fields (add branch), updated-agent refreshes (update branch),
      static/enrolled stay `None`. Anti-trivial (distinctive values: queue_depth 3/7, ttft 42/99, vram_free 30000/20000).
- [x] OPERATOR: gates green — build ✓, clippy `-D warnings` ✓, fmt ✓; lib 459→**462** (+3), total 491→**494** (+3).
- [x] REVIEWER: independent trace — CLEAN on all 7 hunt-list items, traced to file:line; router diff empty.
- [x] LEAD: committed (`46277b4`) + opened **PR #17** vs main. **STOPPED — no auto-merge; PR A awaits an INDEPENDENT outside review before merge. Phase 1 branches off main only AFTER #17 lands.**

### Phase 1 (PR B, off updated main AFTER PR A merges) — the Scored router
- [ ] `RouterEnum::Scored(ScoredRouter)` + `RoutingStrategy::Scored` ("scored") + `create_router` arm.
- [ ] `route_excluding` drop-in: GATE (hard-eliminate ineligible) → SCORE (weighted [0,1] per dimension) → SELECT (argmax + total tie-break).
- [ ] Phase-1 dimensions (pool + request only): model resident, GPU util (lower), VRAM headroom (higher),
      GPU temp (lower), operator priority (higher), tag affinity, backend-type affinity, prompt size vs capacity, model-fits-VRAM.
- [ ] `routing.scored.weights` config (sane defaults; omitted → default; active weights normalized to sum 1; missing dim → neutral 0.5).
- [ ] Debug-log per-candidate score breakdown (auditable routing). Empty candidate set → existing no-backend error → 503.
- [ ] Tests: picks right backend; determinism (run twice → identical); missing-telemetry neutrality; gate-before-score; total tie-break; 503-when-empty.

### Reviewer hunt list (any hit BLOCKS)
1. **DETERMINISM** — no RNG, no wall-clock in scoring path; fixed dimension iteration order; TOTAL tie-break
   (score desc → priority desc → name asc) so two backends never order ambiguously; same snapshot+request ⇒ identical route (asserted by test).
2. **MISSING-VALUE NEUTRALITY** — dimensions a static/enrolled backend can't report degrade to neutral 0.5, never penalize; assert static not disadvantaged vs agent.
3. **GATE-BEFORE-SCORE** — unhealthy/excluded/model-absent/circuit-open eliminated BEFORE scoring, not scored-to-zero.
4. **NO unwrap/expect in lib code** (test code may unwrap).

### Rules
conventional commits; commit trailer `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`.
Each phase: build + test (count grown) + clippy `-D warnings` + fmt --check green BEFORE its PR opens.

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
