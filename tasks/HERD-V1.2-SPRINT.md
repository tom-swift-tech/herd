# HERD v1.2 Sprint — Agent/Gateway Foundation

**Spec:** `docs/specs/v2-distributed-inference-spec.md`
**Target:** v1.2 (foundation) — `herd agent` ships, single-node deployments only. No speculative, no pipeline.
**Status:** PRs #1–#5.1, #6a, #6b, #6c, and #7 landed; #8 next (last reconciled with implementation: 2026-06-13)

> This doc tracks the PR breakdown and acceptance checklist for the v1.2 milestone.
> The architecture, data structures, and rationale live in the spec — this is the
> sequencing/checklist companion the spec and `ROADMAP.md` reference.

---

## PR Breakdown

| PR | Title | Scope | Status |
|----|-------|-------|--------|
| #1 | Seed `Deployment` module | `Deployment::Single` variant in `src/router/deployment.rs`, `primary_backend()` accessor + unit tests | ✅ landed (`2960289`) |
| #2 | `NodeRegistry` with TTL eviction | In-memory `NodeRegistry` keyed by `node_id`, heartbeat-only protocol, injectable `Clock` for deterministic time tests, 10 unit tests | ✅ landed (`be6f24e`) |
| #3 | Gateway heartbeat ingestion | `NodeRegistry` onto `AppState`; stale-eviction background task; `POST /api/internal/nodes/heartbeat` with `HERD_AGENT_TOKEN` bearer auth; heartbeat protocol tests | ✅ landed |
| #4 | `herd agent` CLI + daemon | Restructure CLI into `serve`/`agent` subcommands; `src/daemon/` (heartbeat client, capability detection, lifecycle); single-node deployment | ✅ landed |
| #5 | Agent node persistence + Fleet | Migration v5 (`source`, `agent_version`); write-through on transition; soft-evict (mark offline) + reaper for stale agent rows; Fleet tab reads unified SQLite store | ✅ |
| #5.1 | Hard-exclude agent rows from poller + SQLite routing | Live-test fix: `get_pollable_nodes` and `get_routable_nodes` now filter `source != 'agent'` explicitly. Previously the health poller picked up agent rows (`enabled=1`), `update_health` flipped them 'online'→'healthy', and they entered the routable pool — decision 12 was implied by status convention, never enforced. Also closes the `update_node(enabled=true)` side door that sets `status='healthy'` on any row. | ✅ |
| #6a | Gateway version authority | Reshaped scope (supersedes "heartbeat protocol hardening" — the old "deployments-assigned plumbing" line is folded into this response-channel work). `fleet:` config (`target_agent_version`, `publish_dir`, `download_url_base`); agents report `os`/`arch`; `BinaryStore` (sha256 cache over `{publish_dir}/{version}/{os}-{arch}/herd[.exe]`); heartbeat response gains `target_version` + `download_url`/`sha256` when a binary is published for the agent's platform; authed `GET /api/internal/nodes/binary/:version/:platform`. | ✅ |
| #6b | Agent self-update | Agent acts on the heartbeat offer: download → verify sha256 (abort + keep running on mismatch) → self-replace → restart (Windows-safe swap via `self_replace`); URL source is presence-as-signal (decision 19, revised); respawn modes `self`/`supervised`; failed-offer memo (~5min suppression); `updating` registry state with eviction grace so the restart gap never looks like node death; `version_changed` beat persists the new `agent_version` and clears 'updating'. | ✅ |
| #6c | `herd publish` helper | Thin promote step: `herd publish [BINARY] --version <V> [--os --arch --publish-dir --config --force]` copies a binary into `{publish_dir}/{version}/{os}-{arch}/herd[.exe]` (the write side of `BinaryStore`), prints its sha256 (via `BinaryStore::sha256_of`, so it matches the gateway's advertised hash by construction) and a reminder to bump `fleet.target_agent_version`. Refuses to overwrite differing bytes without `--force`; identical bytes are idempotent. sha→stdout, narration→stderr. | ✅ |
| #7 | `BackendPool` integration | `AgentPoolSync` reconciler mirrors `registry.fresh_nodes()` (strict TTL, **no** grace) into the `BackendPool` under the `agent:{node_id}` key prefix every ~2s (`HERD_AGENT_POOL_SYNC_SECS`); agent nodes route identically to static/enrolled backends through the unchanged routers. Owns the `agent:` prefix only — never touches `node:`/static entries; the static `sync_to_pool` path and the 5.1 `get_routable_nodes`/`get_pollable_nodes` exclusions are byte-unchanged. Source = in-memory registry heartbeat freshness, never SQLite. `find_for_model()` was not needed — the existing model-aware router already selects from the pool. **Deferred to v1.3:** identity-match conflict-resolution dedup (see decision 6 / 14). | ✅ |
| #8 | Integration test + smoke | Gateway + 1 agent in same process; request routes through agent's (stub) llama-server end-to-end | ⬜ |

---

## v1.2 Acceptance Checklist

From the spec's "v1.2 — Agent/Gateway Foundation" acceptance block, annotated with PR ownership:

- [x] `herd agent --gateway <url> --node-id <id>` subcommand exists *(PR #4 — node_id defaults to hostname-gpu, e.g. `citadel-5090`)*
- [x] Agent sends heartbeat every 2s with full capability snapshot *(PR #4 — exponential backoff capped at 30s while gateway unreachable)*
- [x] `POST /api/internal/nodes/heartbeat` is the only v1.2 agent-control endpoint; unknown `node_id` values are implicitly registered on first heartbeat *(PR #3)*
- [x] Gateway maintains in-memory `NodeRegistry` keyed by `node_id` with TTL eviction (default 30s) *(struct in PR #2; on `AppState` + eviction task in PR #3)*
- [x] Agent-registered nodes appear in `BackendPool` and route identically to static backends *(PR #7 — `AgentPoolSync` mirrors fresh agents under the `agent:` prefix; routable via the unchanged model-aware router exactly like a static backend)*
- [x] Existing static-backend config path is unchanged *(maintained; verified PR #3)*
- [x] Dashboard Fleet tab shows agent-registered nodes with live state *(PR #5 — agent rows persisted to SQLite with `source='agent'`, online/offline status + source badge in the Fleet table; written on register/model-change/eviction, steady beats stay in-memory)*
- [~] Both modes can run on the same host (CITADEL self-test scenario) *(PR #4: guarded self-test in `tests/agent_daemon.rs` + manual smoke verified 2026-06-09; PR #8 extends to routed end-to-end)*
- [x] Auth: shared bearer token via env var (`HERD_AGENT_TOKEN`) *(PR #3)*
- [x] Gateway returns 503 with clear error if all healthy backends — agent and static — are gone (no hidden fallback) *(PR #7 — a stale agent leaves `fresh_nodes()`, the reconciler removes its `agent:` entry, and an empty pool makes the existing routers return the "No healthy backends" error → 503; no fallback to a removed entry)*
- [~] Tests: `NodeRegistry` unit tests *(PR #2: 10 tests)*, heartbeat protocol tests *(PR #3: 8 tests, verified green 2026-06-05)*, daemon unit + heartbeat-client integration tests *(PR #4: 39 unit tests + `tests/agent_daemon.rs`)*, `AgentPoolSync` reconciler tests *(PR #7: 4 tests — fresh→routable, drain→503, prefix-ownership, enrolled+agent coexist)*, integration test with gateway + 1 agent in same process *(PR #8)*

### Known limitations (v1.2)

- **Enrolled + agent on one host = two pool entries.** If a physical host is both operator-enrolled (`node:{hostname}`) and running `herd agent` (`agent:{node_id}`), it appears as two distinct, independently-routable `BackendPool` entries (decision 14 — coexist, no dedup in v1.2). Identity-match convergence (decision 6) is a v1.3 item. Not a bug: the two reconcilers own disjoint key prefixes by design.

---

## Decisions (locked for v1.2)

These resolve the spec's "Open Questions for Tom":

1. **Module naming** — node-side daemon lives under `src/daemon/` (avoids colliding with existing `src/agent/` sessions). User-facing CLI term stays `herd agent`.
2. **Auth** — shared bearer token in `HERD_AGENT_TOKEN` env var. mTLS documented as future hardening, not day-one. When the token is unset the gateway logs a warning and allows the heartbeat (mirrors `require_api_key`'s "no key configured = allow" precedent, so the CITADEL self-test works without setup); when set, it is required.
3. **Node ID** — human-readable, hostname-derived (`hostname-gpu`, e.g. `citadel-5090`), with `--node-id` override.
4. **Heartbeat cadence** — 2s default, configurable per-agent. Gateway TTL eviction default 30s.
5. **Deployment manifest source** — deferred beyond v1.2 (single-node only ships here). Recommended: top-level `deployments:` in `herd.yaml` when it lands (v1.3).
6. **Conflict resolution** — agent registration overrides a static backend only on exact logical-node identity match (`node_id` + advertised inference address); otherwise both remain visible and a duplication warning is logged *(**deferred to v1.3**: PR #7 ships the coexist half per decision 14 — an enrolled `node:{hostname}` entry and an agent `agent:{node_id}` entry for the same physical host are two distinct pool entries; identity-match dedup + the duplication warning land with v1.3 convergence)*.
7. **Gateway discovery** — explicit `--gateway <url>` required on the agent. `herd.starbase` (Tailscale DNS) documented as recommended value; no auto-discovery.

---

## PR #5 — Agent Node Persistence + Fleet Integration (revised scope)

Supersedes the original "project NodeRegistry alongside SQLite" framing. Agent nodes
are persisted to `node_db`, so the Fleet tab reads a single store and no render-time
merge of two stores is needed. The in-memory `NodeRegistry` remains the routing/liveness
layer (PR #7); SQLite persistence here is operator-visibility + housekeeping.

Locked decisions (extend the list above):

8.  **Persistence + discriminator.** Agent heartbeats persist to the SQLite `nodes`
    table, discriminated by a new `source` column ('enrolled' | 'agent'). Migration v5
    adds `source` (DEFAULT 'enrolled', so all existing rows are correctly tagged
    enrolled) and `agent_version`. Fleet tab reads SQLite only.
9.  **Soft eviction (mark-offline-then-reap).** In-memory TTL eviction sets the SQLite
    row's `status='offline'` — it does NOT delete. A separate reaper hard-deletes
    `source='agent'` rows offline beyond a grace window (default 24h, configurable via
    `HERD_AGENT_REAP_GRACE_SECS`). Enrolled rows are never auto-reaped. Routing ignores
    SQLite for agents, so an 'offline' row is never a routing hazard.
10. **Write-through on transitions only.** SQLite is written on: first heartbeat
    (register), material capability change (models_loaded set changes), and eviction.
    Steady unchanged 2s beats stay in-memory only and never touch the DB.
11. **NodeRegistry stays DB-free.** Persistence glue lives in the heartbeat handler
    (`api/internal.rs`) and the evictor + new reaper tasks (`server.rs`), which already
    hold `AppState` with both stores. The registry keeps its injectable-Clock
    determinism with no `NodeDb` dependency. The evictor must surface evicted node_ids
    so the glue can mark them offline.
12. **Fleet-visible, not SQLite-routable.** Agent rows use status values OUTSIDE the
    routable set ('online'/'offline', never 'healthy'/'degraded'), so
    `get_routable_nodes()` cannot pull an agent node into the static routing path before
    PR #7 wires it deliberately through the in-memory registry.
    *Hardened in PR #5.1:* the status convention alone didn't enforce this — the health
    poller polled agent rows and flipped them 'healthy'. Both `get_pollable_nodes()` and
    `get_routable_nodes()` now exclude `source='agent'` explicitly, so the exclusion holds
    by construction regardless of a row's status.
13. **Persist durable fields only.** Map node_id→node_id (and hostname, since hostname is
    NOT NULL UNIQUE and agents have no separate hostname), address→backend_url (on-disk
    column is the legacy `ollama_url` — mirror existing `upsert_node`), backend, gpu_model,
    vram_total_mb→vram_mb, models_loaded, agent_version→new column. Dynamic perf fields
    (vram_free, queue_depth, ttft_p50) stay in-memory only — routing inputs, not records.
14. **No agent/enrolled merge in v1.2.** If one physical host is both enrolled and running
    an agent, two rows coexist (different hostname/source). Dedup deferred.

---

## PR #6 — Gateway Version Authority + Agent Self-Update (reshaped scope)

Supersedes the original "heartbeat protocol hardening" framing. The gateway is the fleet's
version authority: it advertises a target agent version on every heartbeat and serves
published binaries; agents self-update to the declared target. Split into three stacked
PRs (#6a gateway, #6b agent, #6c publish helper).

Locked decisions (extend the list above):

15. **Model A with an opaque download source.** The gateway serves the binary itself
    (no GitHub-release dependency for dev), but the agent treats `download_url` as
    opaque — pointing `fleet.download_url_base` at an external host (Model B) later
    requires no agent change. With an external base, the sha256 still comes from the
    gateway's local publish dir: the local copy is the source of truth for what was
    promoted.
16. **The fleet target is a SET value.** `fleet.target_agent_version` defaults to the
    gateway's own version, overridable via config or `HERD_TARGET_AGENT_VERSION` (env
    wins). Advertising alone can't push an update: an offer is only attached when a
    binary is actually published under `publish_dir` for the target version and the
    agent's platform — promotion is the deliberate act of publishing, never a side
    effect of restarting `serve`.
17. **Gateway advertises, agent decides.** The response always carries `target_version`;
    the gateway never rejects heartbeats from out-of-date agents (that would brick the
    fleet during every update window). Version comparison happens agent-side (#6b).
18. **sha256 verify before swap is mandatory** (#6b). Covers corrupt/MITM transfer; does
    not pretend to solve a malicious gateway (out of scope on a self-hosted control
    plane). A mismatch aborts the update; the agent keeps running and heartbeating.
19. **Download URL derivation — presence-as-signal** *(revised in #6b; replaces the
    Host-header derivation shipped in #6a)*. In the local case the gateway sends
    `target_version` + `sha256` ONLY — no `download_url`. The agent constructs the
    binary URL itself from its own `--gateway` value
    (`{gateway}/api/internal/nodes/binary/{ver}/{os}-{arch}`), so it never fetches a
    URL derived from what the gateway saw in request headers. `download_url` is
    attached ONLY when `fleet.download_url_base` is configured (the external/Model-B
    override), and the agent then uses it verbatim. Presence ⇔ external override — a
    null check, not URL archaeology. Old agents that don't report `os`/`arch` get the
    target advertised but never an offer.
20. **'updating' registry state** (#6b). Before restarting for an update the agent flags
    its final beat (`updating: true`); the evictor grants a longer grace
    (`HERD_AGENT_UPDATE_GRACE_SECS`, default 180s) so the restart gap is never reported
    as node death, and the SQLite row flips to status='updating'. The re-registered
    beat's changed `agent_version` triggers a persist (`HeartbeatOutcome::Updated`
    gained `version_changed`) — without it the row would stay stuck at 'updating' with
    the old version, since models usually haven't changed.
21. **Final updating beat is fire-and-forget** (#6b). The restart proceeds even if that
    POST fails — the beat only suppresses eviction. Worst case the node cosmetically
    shows offline for under the grace window and re-registers on its first beat after
    restart.
22. **Respawn modes** (#6b). `self` (default): after `self_replace`, spawn
    `current_exe()` with inherited argv+env and exit(0) — for bare `herd agent` in a
    terminal. `supervised`: exit(0) only, letting the supervisor's Restart=always bring
    up the new binary (self-spawning under NSSM/systemd would double-run the agent).
    Agents read `--respawn-mode` / `HERD_RESPAWN_MODE`; `fleet.respawn_mode` in config
    documents the fleet-wide intent (reserved for heartbeat relay later). Respawn logic
    sits behind a `Respawner` trait so the loop is testable.
23. **Failed-offer memo** (#6b). A failed (version, sha256) download/verify pair is not
    retried for ~5min — a bad offer re-advertised every 2s must not hammer the
    download endpoint. A republished binary (same version, new sha) is eligible
    immediately. *(Hardened in #6b review-fix — see decision 24: a failed **respawn**
    after a successful swap now arms this memo too.)*
24. **Failed-respawn recovery** (#6b review-fix). Two coupled gaps in the
    self-respawn path are closed:
    (a) If `respawner.restart()` returns `Err` *after* a successful binary swap, the
    failure memo (decision 23) is now armed before logging — otherwise `should_apply()`
    stayed true and the agent re-downloaded + re-swapped the identical offer every ~2s
    (a download storm on the very node whose respawn is wedged). The post-swap step is
    factored into a synchronous `finish_update` fn so a mock `Respawner` returning `Err`
    drives it without exiting the test process.
    (b) The fire-and-forget `updating: true` beat (decision 21) already flipped the
    SQLite row to 'updating'. After a wedged respawn the agent keeps beating the SAME
    version, so the `version_changed` trigger from decision 20 never fires and the row
    stuck at 'updating' forever. `HeartbeatOutcome::Updated` gains a third field
    `update_cleared` (true exactly when a normal beat disarms a prior `updating_since`),
    and the gateway persists on it — clearing the row back to 'online' independent of any
    version change. Verified by `normal_beat_with_same_version_clears_stuck_updating_row`.
25. **`herd publish` is the write side of `BinaryStore`** (#6c). The promote subcommand
    copies a binary into the exact layout the gateway serves and prints the sha256 via
    `BinaryStore::sha256_of`, so the published hash matches the gateway's advertised hash
    by construction (no second hashing implementation to drift). Design choices:
    source defaults to `current_exe()` (publish the build you just made), positional
    `[BINARY]` overrides for cross-publishing; `--version` is **required** — publishing
    under the wrong version is the one silent mis-serve, so it is never defaulted from
    `CARGO_PKG_VERSION`. os/arch default to host `std::env::consts`. publish-dir reuses
    `FleetConfig::publish_dir_from` (`--publish-dir` > `HERD_AGENT_PUBLISH_DIR` > `--config`'s
    `fleet.publish_dir` > `~/.herd/binaries`). Overwriting differing bytes is **refused
    without `--force`** (a mid-flight sha change would break decision-18 verify); identical
    bytes are idempotent (`Outcome::Unchanged`). sha256→stdout (scriptable), narration→stderr.
    Thin promote only — no auto-bump of `target_agent_version` (a separate deliberate act,
    decision 16), no list/prune/GC, no remote upload, no config `validate()`, no async.

---

## Testing Infra Notes

`NodeRegistry` accepts an injectable `Clock = Arc<dyn Fn() -> Instant + Send + Sync>` via a private
`with_clock` constructor (established in PR #2). Production uses `Instant::now`; tests drive a
mutex-protected `TestClock` with `advance(Duration)`. This sidesteps `tokio::time::advance` (which
does not affect `Instant::now`) while keeping monotonic semantics in production. Reuse this pattern
for the heartbeat client, evictor task, and deployment health checks in PRs #4+.

## References

- Spec: `docs/specs/v2-distributed-inference-spec.md`
- Roadmap: `ROADMAP.md` → "v1.2.0+ — Distributed Inference (In Spec)"
- Lessons: `tasks/lessons.md`
