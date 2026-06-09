# Herd — Working TODO

> Scratchpad for in-flight work. Milestone tracking lives in `ROADMAP.md`;
> the v1.2 PR breakdown + acceptance checklist live in `tasks/HERD-V1.2-SPRINT.md`.

**Last updated:** 2026-06-05

---

## ✅ Verification complete (2026-06-05, post-toolchain-install)

PR #3 (`feat/v1.2-pr3-heartbeat-ingestion`) is now **compiler-verified**. Toolchain note:
the machine had Rust (msvc) but no C/C++ build tools — installed VS 2022 Build Tools
(VCTools + Windows SDK 10.0.26100) so linking and `ring` compile.

Results:
```
cargo build                 # ✓ clean
cargo test internal         # ✓ 8/8 heartbeat protocol tests pass
cargo test                  # ✓ full suite green — 323 lib + 15 + 10 integration, 0 failures
cargo fmt --check           # ✓ clean (applied fmt to src/api/internal.rs)
cargo clippy --all-targets  # ✓ new code clean; one PRE-EXISTING warning in
                            #   src/agent/store.rs:136 (sort_by → sort_by_key), unrelated to PR #3
```

**One real fix was needed** (the kind only a compiler catches): the `#[cfg(test)]`
`AppState` builder in `src/server.rs` (reload-config test, ~L2246) was missing the new
`node_registry` field — added `node_registry: Arc::new(NodeRegistry::new(Duration::from_secs(30)))`.
The `tests/frontier_escalation.rs` builder already had it.

Spot-checks from the original review all held up:
- `axum::body::Bytes` extractor is last in the `heartbeat` handler ✓
- `#[serde(default)]` on `HeartbeatRequest::timestamp` deserializes correctly ✓
- `constant_time_eq` is reachable as `pub(crate)` from `src/api/internal.rs` ✓

Ready to commit (`feat(nodes): wire NodeRegistry onto AppState + heartbeat endpoint`),
push, open PR #3.

---

## In flight

### v1.2 PR #3 — Gateway heartbeat ingestion ✅ (written, unverified)
- [x] `NodeRegistry` on `AppState` (30s TTL)
- [x] 10s stale-eviction background task in `server::run`
- [x] `POST /api/internal/nodes/heartbeat` (`src/api/internal.rs`) + `HERD_AGENT_TOKEN` auth
- [x] 8 heartbeat protocol unit tests
- [x] Docs: sprint doc created, v2 spec reconciled
- [x] **Compile + test verification** — green; fixed missing `node_registry` in test `AppState` builder

---

## Next — v1.2 PR #4: `herd agent` CLI + daemon

The big structural one. Scope from `tasks/HERD-V1.2-SPRINT.md`:

1. **CLI restructure** — `src/main.rs` is currently a flat `clap::Parser` (flags only).
   Convert to subcommands: `serve` (default, today's behavior) + `agent`. Keep existing
   flags (`--config`, `--port`, `--host`, `--backend`, `--update`) working under `serve`.
   ⚠️ The v2 spec wrongly claims "cli.rs already dispatches subcommands" — it does not.
   `src/cli.rs` is only `parse_backend_spec`. This restructure is real, unplanned-in-spec work.
2. **`src/daemon/` module** (named `daemon`, not `agent`, to avoid colliding with the
   existing agent-sessions `src/agent/`):
   - `client.rs` — heartbeat client: POST capability snapshot to `--gateway` every 2s
     (configurable), `HERD_AGENT_TOKEN` bearer. Reuse the `TestClock` pattern for timers.
   - `capabilities.rs` — GPU/VRAM/model detection. Reuse herd-tune logic where possible.
   - `lifecycle.rs` — spawn/supervise local `llama-server` (rpc-server deferred to v1.4).
3. **Node ID** — hostname-derived default (`hostname-gpu`, e.g. `citadel-5090`), `--node-id` override.
4. Tests: capability snapshot serialization round-trips against `AgentCapabilities`;
   heartbeat client retries/backoff on gateway-down.

## Then — remaining v1.2 PRs
- **#5** — Dashboard Fleet tab projects `NodeRegistry` live state alongside SQLite operator nodes.
- **#7** — `BackendPool` integration: agent nodes route like static backends; `NodeRegistry::find_for_model()`;
  conflict resolution (agent overrides static only on exact `node_id` + address match); 503 when all gone.
- **#8** — Integration test: gateway + 1 agent in-process, request routes through agent's stub llama-server.

---

## Backlog / loose ends noticed during review
- `tasks/HERD-V1.2-SPRINT.md` was missing entirely (referenced by ROADMAP + spec) — recreated 2026-06-05.
- v1.1.1 and v1.1.2 are in `Cargo.toml`/commits but **not git-tagged** (tags stop at `v1.1.0`).
  Consider `git tag v1.1.1 v1.1.2` retroactively, or tag at next release.
- New endpoint not in `skills.md`/dashboard per CLAUDE.md rule — intentional: it's an internal
  agent-protocol endpoint, not a client API. Dashboard surfacing is PR #5.
