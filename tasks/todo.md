# Herd — Working TODO

> Scratchpad for in-flight work. Milestone tracking lives in `ROADMAP.md`;
> the v1.2 PR breakdown + acceptance checklist live in `tasks/HERD-V1.2-SPRINT.md`.

**Last updated:** 2026-06-13

---

## PLANNED (awaiting go-ahead) — Containerized-gateway persistence (HERD_DATA_DIR)

Branch `feat/gateway-data-dir` off `main` (`e51d80e`). Independent of the scorer stack (PR #17) —
disjoint files (config.rs/server.rs/nodes/db.rs/analytics.rs/agent/audit.rs/Dockerfile), NO pool.rs/pool_sync.rs.

**Problem:** Dockerfile runs `herd` with `--home-dir /nonexistent`, so every `~/.herd` store is
non-writable/non-persistent → a containerized gateway loses fleet registry, analytics, costs, and
published binaries on restart.

### Store sites that root at `~/.herd` today (the full set the fix must cover)
1. `src/nodes/db.rs:62` — node registry SQLite `herd.db`
2. `src/analytics.rs:55` — analytics JSONL `requests.jsonl`
3. `src/agent/audit.rs:42` — agent audit JSONL `agent_audit.jsonl`
4. `src/server.rs:420` — agent session store dir `sessions/`
5. `src/server.rs:443` — frontier cost SQLite `frontier_costs.db`
6. `src/config.rs:744` — `publish_dir` **default** `~/.herd/binaries` (HERD_AGENT_PUBLISH_DIR override must KEEP precedence)
   - EXCLUDED (not Herd stores): `src/blob.rs:52` (`~/.ollama/models`, has OLLAMA_MODELS override), `src/updater.rs:68` (temp file in a passed-in dir).

### Plan (~10 lines)
1. **Resolver (config.rs):** add `data_dir_from(env, config) -> PathBuf` (env-injectable core, mirrors
   `publish_dir_from`): `HERD_DATA_DIR` env → config field → `dirs::home_dir().join(".herd")`. Default byte-identical to today.
2. **Config field:** top-level `Config.data_dir: Option<String>` (`#[serde(default)]`); `Config::resolved_data_dir()` wraps the core with the real env var.
3. **Single resolution point:** `server::run()` resolves the root once, threads a `&Path` into the store constructors.
4. **Thread it:** `NodeDb::open(&Path)`, `Analytics::new(&Path)`, `AgentAudit::new(&Path)` gain a data-root param (each joins its filename + `create_dir_all`); session_dir & `frontier_costs.db` (inline in server.rs) root under it; update the `#[cfg(test)]` AppState helper (~server.rs:2324) to pass a path.
5. **publish_dir default** re-rooted to `{data_dir}/binaries`; `HERD_AGENT_PUBLISH_DIR` and `fleet.publish_dir` still win.
6. **Startup creates** the data dir if absent (constructors already `create_dir_all`; add a top-level ensure so it's writable before any store opens).
7. **Dockerfile:** `mkdir -p /var/lib/herd && chown herd:herd`, `ENV HERD_DATA_DIR=/var/lib/herd`, `VOLUME /var/lib/herd` (keep `/nonexistent` home; HERD_DATA_DIR set explicitly so nothing falls back to home — documented).
8. **Docs:** `docker run`/compose snippet with the volume mount + a one-line README/skills note that a container gateway needs a persistent data volume.
9. **Tests:** default (no env/config) → every store under `~/.herd` unchanged; `HERD_DATA_DIR` set → all roots under it; `HERD_AGENT_PUBLISH_DIR` override still wins; data dir created when missing.
10. Gates: build + test (count grown) + clippy `-D warnings` + fmt `--check`. No unwrap/expect in lib code.

### Roles
ARCHITECT: confirm the resolver/threading design against the 6 sites (catch any store left scattered).
BUILDER: implement. REVIEWER (independent, blocking hunt list): (1) ALL 6 stores root under data dir — none left at `~/.herd`; (2) default byte-identical when unset; (3) data dir created + writable by non-root container user; (4) no unwrap/expect in lib. OPERATOR: gates, separate lib/total counts. LEAD: docs + PR, STOP at open PR (no auto-merge; independent outside review before merge).

### Status
- [x] STOP for go-ahead → approved
- [x] ARCHITECT confirm — 6 store KINDS correct, no 7th. Corrections: (1) `Analytics::new` 2nd caller server.rs:361;
  (2) out-of-crate tests `frontier_escalation.rs` + `fleet_routing.rs` call no-arg ctors → break build; (3) `frontier_costs.db`
  parent not self-created → single `create_dir_all` top of `run()`; (4) **`herd publish` CLI must honor HERD_DATA_DIR** (else
  binaries split from gateway); (5) `resolved_publish_dir` + `UpdateContext::from_config` thread data root; (6) `publish_dir_from`
  +3rd `data_root` param; (7) `BinaryStore::new` needs NO path (in-mem cache); (8) Dockerfile `ENV HERD_DATA_DIR` is the live delivery.
- [x] BUILDER implement — all 6 stores re-rooted via single resolution in `run()`; `data_dir_from`/`resolved_data_dir`;
  `publish_dir_from` +data_root; `herd publish` CLI honors HERD_DATA_DIR; Dockerfile ENV/VOLUME/chown; README Docker section.
  Callers fixed: server.rs:361 (prod Analytics), test helper, both out-of-crate tests. `frontier_escalation` NodeDb → in-memory.
- [x] OPERATOR gates — build ✓, clippy `-D warnings` ✓, fmt ✓; lib 459→**466** (+7), total 491→**498** (+7).
- [x] REVIEWER independent hunt list — CLEAN on all 8; no `~/.herd` residual in store paths, default byte-identical,
  CLI seam honored, no scorer collision, no `set_var` in tests, no unwrap in lib.
- [x] LEAD — committed (`6eb7726`) + opened **PR #18** vs main. **STOPPED — no auto-merge; awaits independent outside review before merge.**

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
