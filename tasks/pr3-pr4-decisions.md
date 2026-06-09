# PR #3 + PR #4 — Decision Grids

**Sprint:** tasks/HERD-V1.2-SPRINT.md (PRs #3 and #4 of 8)
**Status:** Draft — awaiting human sign-off
**Scouted against:** `2960289` (main tip: `refactor(router): seed deployment module with Single variant`)
**PR #2 artifact scouted:** worktree `pr2-node-registry` — `src/nodes/registry.rs` defines `NodeRegistry`, `AgentCapabilities`, `HeartbeatOutcome::{Registered,Updated}`, and `heartbeat(AgentCapabilities) -> HeartbeatOutcome` (async).

Key prior-art facts (cited once, reused below):

- **Route registration** — all routes live on a single `axum::Router` built in `src/server.rs:509` inside `Server::run`. Auth is applied by merging sub-routers that have a `middleware::from_fn_with_state(state.clone(), require_api_key)` layer (e.g. model-mgmt routes at `src/server.rs:586`, admin routes at `src/server.rs:628`, agent routes at `src/server.rs:668`).
- **Middleware-based auth** — `require_api_key` at `src/server.rs:981` reads `state.config.server.api_key`; if `None` the route is open; otherwise it calls `extract_api_key` (`src/server.rs:954`) which accepts `X-API-Key` *or* `Authorization: Bearer <key>` and compares via `constant_time_eq` (`src/server.rs:970`, exported `pub(crate)`).
- **Alternative inline auth** — `src/api/nodes.rs:23` (`register_node`) hand-rolls its check inside the handler against `config.server.enrollment_key`, reading both `?enrollment_key=` query and `x-enrollment-key` header. Used because the herd-tune script hits this without any bearer.
- **AppState** — defined at `src/server.rs:126`, constructed at `src/server.rs:467`. `#[derive(Clone)]`, all fields `Arc<...>`. A `node_db: Arc<crate::nodes::NodeDb>` (SQLite fleet store) already exists — the new `Arc<NodeRegistry>` is a separate field for the in-memory agent registry.
- **CLI** — lives in `src/main.rs` (there is NO subcommand dispatch today). `Cli` is a flat `#[derive(Parser)]` struct at `src/main.rs:7` with `--config`, `--port`, `--host`, `--backend`, `--update`. `--update` is handled as a top-of-main branch at `src/main.rs:43`, `server::run(...)` is called unconditionally otherwise at `src/main.rs:108`. `src/cli.rs` contains only `parse_backend_spec` — it is NOT a subcommand dispatcher despite the filename.
- **Test infra** — `tests/classifier.rs` and `tests/frontier_escalation.rs` are in-process integration tests that import `herd::*` directly (`tests/classifier.rs:8`). No `assert_cmd` / `escargot` in `Cargo.toml` — testing CLI via subprocess is not set up.

---

## PR #3 — feat(api): /api/internal/nodes/heartbeat

### Overlap with existing code

- `src/server.rs:126` — `AppState` struct: new `node_registry: Arc<NodeRegistry>` field goes here.
- `src/server.rs:467` — `AppState` construction in `Server::run`: add `node_registry: Arc::new(NodeRegistry::new(ttl))`. Spec-suggested TTL = 30s; hard-code for v1.2, promote to config later.
- `src/server.rs:509` — route assembly: add `POST /api/internal/nodes/heartbeat` here.
- `src/server.rs:981` — `require_api_key` middleware pattern. Agent auth does NOT fit this — it reads `config.server.api_key`, whereas sprint specifies `HERD_AGENT_TOKEN` env var (distinct secret).
- `src/api/nodes.rs:23` — inline auth pattern is the closer precedent (hand-rolled check in handler). Use `constant_time_eq` from `src/server.rs:970`.
- `src/api/mod.rs:1` — add `pub mod internal;` alongside existing `admin`, `agent`, `models`, `nodes`, `openai`, `profiles`, `status`.
- `src/nodes/registry.rs` (PR #2, worktree) — `AgentCapabilities` already `#[derive(Deserialize)]`; can be used directly as the request body type. `heartbeat(caps) -> HeartbeatOutcome` is the call. No new types needed for PR #3 if `AgentCapabilities` is acceptable as the wire payload.
- No existing route under `/api/internal/*` — no collision (verified via grep).

### Decisions needed

#### D3.1 — Auth validation mechanism

- **Option A — handler-inline check** (mirrors `register_node` at `src/api/nodes.rs:30-47`): read `HERD_AGENT_TOKEN` from env at handler entry, pull `Authorization: Bearer <token>`, compare with `constant_time_eq`.
- **Option B — dedicated middleware** `require_agent_token` (mirrors `require_api_key` at `src/server.rs:981`): added as a layer on the internal sub-router, reads env once at layer construction OR per-request.
- **Option C — axum extractor** (`AgentAuth(())` via `FromRequestParts`): most idiomatic but no precedent in this codebase.
- **Proposed: Option B (middleware)** — the sprint will grow multiple internal endpoints post-v1.2 (Speculative/Pipeline coord), and a sub-router with a shared auth layer scales cleanly. Reading the env per-request (not at startup) lets operators rotate tokens by `systemctl restart` without a rebuild. Handler-inline is precedent-correct but doesn't scale. The extractor approach is cleaner Rust but introduces an idiom not used anywhere else in this repo — skip.
- **Precedent:** both exist (`require_api_key` middleware for admin/agent; inline for `register_node`). Internal endpoints are closer to admin (auth-gated, multiple routes expected) than to enrollment (single public-ish entry point).

#### D3.2 — Route mount path & registration

- Path `/api/internal/nodes/heartbeat` has no collision (no existing `/api/internal/*`).
- **Proposed:** create a new sub-router in `src/server.rs` around line 567 (just after the profiles public routes merge, before the admin routes block). Structure mirrors the admin-routes pattern at `src/server.rs:628-664`:
  ```rust
  let internal_routes = axum::Router::new()
      .route("/api/internal/nodes/heartbeat", axum::routing::post(crate::api::internal::heartbeat))
      .layer(axum::middleware::from_fn_with_state(state.clone(), require_agent_token));
  app = app.merge(internal_routes);
  ```
- **Always mounted**, no feature flag — agent control plane is core to v1.2. `HERD_AGENT_TOKEN` unset means the layer rejects all requests (fail-closed).

#### D3.3 — Success response shape

- **Option A (minimal):** `{ "outcome": "registered" | "updated" }` — HTTP 200 for both (status code distinction is `register_node`'s pattern at `src/api/nodes.rs:58`, but heartbeat is high-frequency and clients shouldn't branch on code).
- **Option B (richer):** `{ "outcome": "...", "server_time_ms": u64, "next_heartbeat_ms": u64 }` — gives the agent backoff guidance.
- **Proposed: Option B, minus `next_heartbeat_ms`** — emit `{ "outcome": "registered" | "updated", "server_time_ms": <unix-ms> }`. `server_time_ms` is cheap and helps future agent-side clock-skew debugging. `next_heartbeat_ms` implies the server drives cadence; v1.2 sprint has the agent drive it (PR #5). Adding it later is non-breaking.
- **HTTP code:** 200 for both outcomes. (Deviates from `register_node`'s 201/200 split, but heartbeat cadence makes code-based branching noisy.)

#### D3.4 — Error response shapes

- **401 (bad/missing token):** bare `StatusCode::UNAUTHORIZED` from the middleware (mirrors `require_api_key` at `src/server.rs:994`). No body. Simple and matches existing admin behavior.
- **400 (malformed payload):** axum's default serde rejection — returns `text/plain` with the parse error. Consistent with most existing handlers (e.g. `admin::add_backend` at `src/api/admin.rs:112` — no wrapped serde error).
- **500 (unexpected):** `(StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "..."})))` — mirrors `src/api/nodes.rs:52`. But for PR #3, `NodeRegistry::heartbeat` is infallible (returns `HeartbeatOutcome`, not `Result`) per `src/nodes/registry.rs:66`, so no 500 path is needed.
- **Proposed:** follow existing precedent verbatim — 401 bare, 400 default serde, no 500 path.

#### D3.5 — Where does `NodeRegistry` live in AppState?

- New field `pub node_registry: Arc<crate::nodes::NodeRegistry>` on `AppState` at `src/server.rs:126`.
- Construction at `src/server.rs:467` (inside `Server::run`): `node_registry: Arc::new(crate::nodes::NodeRegistry::new(Duration::from_secs(30)))`.
- Spawn a background evictor task alongside the existing reapers (pattern at `src/server.rs:493`): `tokio::spawn` a 10-second-interval loop that calls `node_registry.evict_stale().await` and logs evictions at `info`. **Proposed:** include the evictor in PR #3 so the registry doesn't accumulate ghosts once the heartbeat endpoint is live. Tiny addition, avoids a dangling "why are there dead nodes in memory" in PR #5.
- **Open for lead:** TTL value. Sprint implies 30s (`src/nodes/registry.rs:155` tests use 30s). Spec default in `docs/specs/v2-distributed-inference-spec.md` may differ — if spec says otherwise, honor that. Recommend hard-coding for v1.2, deferring config exposure to v1.3.

### Scope

**Files to create:**
- `src/api/internal.rs` — handler `heartbeat(State<AppState>, Json<AgentCapabilities>) -> (StatusCode, Json<HeartbeatResponse>)`.

**Files to modify:**
- `src/api/mod.rs` — add `pub mod internal;`.
- `src/server.rs` — (a) add `node_registry` field on `AppState`, (b) construct it, (c) spawn evictor task, (d) add `require_agent_token` middleware fn, (e) mount internal sub-router.

**Test list (inside the new file):**
- Happy path: valid token + valid payload on unknown node_id → 200 + `{outcome: "registered"}`.
- Idempotency: repeated heartbeat same node_id → 200 + `{outcome: "updated"}`.
- Auth: missing `Authorization` → 401.
- Auth: wrong token → 401.
- Auth: `HERD_AGENT_TOKEN` unset → 401 (fail-closed).
- Payload: malformed JSON → 400.
- Payload: missing required field (`node_id`) → 400.

Tests should use `axum::Router` in-process with `tower::ServiceExt::oneshot` — no port binding. No precedent for this pattern in `tests/` so PR #3 establishes it; module-local `#[cfg(test)] mod tests` inside `src/api/internal.rs` keeps scope tight.

**Env-var testing caveat:** `HERD_AGENT_TOKEN` is process-global. Use `std::env::set_var` with `#[serial_test]` OR pass the expected token through `AppState` (add `agent_token: Option<String>` alongside — read env at construction in `Server::run` only). **Proposed:** read env into `AppState.agent_token: Option<String>` at startup — tests can then inject arbitrary values without env-var contention. The middleware reads from `AppState`, not env.

### Verification

- `cargo build` / `cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` (same pipeline as PR #1 and PR #2).

### Suggested commit message

```
feat(api): add POST /api/internal/nodes/heartbeat endpoint

Gateway-side endpoint that accepts agent heartbeats, registers unknown
node_ids on first contact, and updates known ones via NodeRegistry.
Authenticated with HERD_AGENT_TOKEN (shared bearer). Includes a 10s
stale-node evictor task so the in-memory registry self-cleans at the
30s TTL boundary.
```

---

## PR #4 — feat(daemon): herd agent subcommand skeleton

### Overlap with existing code

- `src/main.rs:7` — `Cli` is a flat derive struct, NOT using `#[command(subcommand)]`. PR #4 MUST migrate this to a subcommand-shaped CLI — non-trivial breaking change to the argument surface.
- `src/main.rs:43-69` — `--update` is handled as a top-level flag branch. When migrating to subcommands, this becomes a subcommand (`herd update`) OR stays as a global flag. See D4.2.
- `src/main.rs:86` — `cli.backend` iterating `parse_backend_spec` — currently tied to serve mode; needs to move under the serve subcommand's args.
- `src/cli.rs:3` — `parse_backend_spec` is the only content; this file is misnamed for "CLI" since it's just a parser. PR #4 can either keep it as-is (subcommand dispatch in `main.rs`) or rename/repurpose. **Proposed: leave `src/cli.rs` as `parse_backend_spec` only**; put subcommand enum in `main.rs` alongside the existing `Cli` struct.
- `src/lib.rs:9` exports `pub mod cli;` — keep.
- No existing `src/daemon/` directory — create fresh.

### Decisions needed

#### D4.1 — Module layout for `src/daemon/`

- **Option A:** `src/daemon/mod.rs` with everything inline — one `pub async fn run(args: AgentArgs) -> anyhow::Result<()>` that prints "herd agent: not yet implemented" and returns `Ok(())`.
- **Option B:** `src/daemon/mod.rs` re-exports from `src/daemon/agent.rs`. Consistent with `src/backend/mod.rs` layout (which re-exports `pool`, `health`, `warmer`, `discovery`).
- **Proposed: Option B** — `src/daemon/mod.rs` with `pub mod agent;` and `pub use agent::run;`, and `src/daemon/agent.rs` containing the (skeleton) `run` fn. Matches the `src/backend/` / `src/nodes/` precedent. Future PRs (#5 heartbeat client, #6 llama-server supervisor) will each add a file to `src/daemon/` — this layout scales without refactor.
- Also add `pub mod daemon;` to `src/lib.rs:13` (alphabetical, between `config` and `discovery`).

#### D4.2 — CLI argument surface (subcommand migration)

This is the most consequential decision — it reshapes the top-level CLI.

- **Option A — clean subcommand migration:**
  ```rust
  #[derive(Parser)]
  struct Cli {
      #[command(subcommand)]
      command: Option<Command>,
      // global flags (--update stays at top-level)
      #[arg(long)] update: bool,
  }
  #[derive(Subcommand)]
  enum Command {
      Serve(ServeArgs),   // all current flags move here
      Agent(AgentArgs),   // new
  }
  ```
  With `command: Option<Command>` defaulting to `Serve` for backward compat when no subcommand is given.
- **Option B — keep flat Cli, add `--agent` flag:** minimal churn, ugly for a user-facing command.
- **Proposed: Option A with `command: Option<Command>`** — when `None`, the existing flat flags drive `serve`. This preserves the `herd -c herd.yaml` invocation exactly as it works today (critical — users' systemd units depend on it). Adding `herd agent --gateway ...` is purely additive.
- **`AgentArgs` for PR #4 skeleton:**
  - `--gateway <URL>` — required, `String` (no URL validation in PR #4; PR #5 adds `reqwest::Url` parsing when it actually makes requests).
  - `--node-id <ID>` — optional in PR #4, `Option<String>`. PR #5 defaults to hostname. Leaving it optional in PR #4 lets the skeleton compile without forcing a hostname crate dep.
  - No `--port`, no `--backend-url` — those belong to PR #5 (agent-side llama-server wiring) or #6.
- **`--update` flag:** keep as a top-level flag (do NOT make it a subcommand). Reason: v1.1.2 docs and user muscle memory use `herd --update`. Turning it into `herd update` breaks user workflows. Handle it before subcommand dispatch, same as today (`src/main.rs:43`).

#### D4.3 — Env var `HERD_AGENT_TOKEN` wiring

- **Proposed: do NOT read `HERD_AGENT_TOKEN` in PR #4.** PR #4 is a compile-and-exit skeleton; no HTTP requests happen. PR #5 reads the env var inside the heartbeat client.
- The skeleton's `run` fn should accept `AgentArgs` by value and print `"herd agent: not yet implemented (gateway={}, node_id={:?})"` — echoing the args proves plumbing works.

#### D4.4 — Exit code semantics

- **Proposed: 0.** Sprint explicitly says "exits 0". This is a scaffolding commit; PR #5 is where the agent actually runs. Non-zero would (correctly) cause systemd/supervisord restart loops if someone ships the skeleton.
- Print the "not yet implemented" line on **stdout**, not stderr — it's informational, not an error. (Matches the spirit of `--update`'s stdout prints at `src/main.rs:46-61`.)

### Scope

**Files to create:**
- `src/daemon/mod.rs` — `pub mod agent; pub use agent::{run, AgentArgs};`.
- `src/daemon/agent.rs` — `#[derive(clap::Args)] pub struct AgentArgs { ... }` and `pub async fn run(args: AgentArgs) -> anyhow::Result<()>` (prints + Ok).

**Files to modify:**
- `src/main.rs` — migrate `Cli` to subcommand-enabled shape, add `Command::Agent(AgentArgs)` dispatch, preserve `Command::Serve` as default-when-absent. The `--update` branch stays as-is.
- `src/lib.rs` — add `pub mod daemon;`.

### Test list

- `cargo build` compiles (table stakes).
- `cargo build --release` compiles (sanity check on the `#[command]` derive).
- Inside `src/daemon/agent.rs`: a `#[tokio::test]` that calls `run(AgentArgs { gateway: "http://x".into(), node_id: None })` and asserts it returns `Ok(())`. This is the contract PR #5 will fill in.
- **Subprocess CLI test:** no precedent in this repo (`assert_cmd` not in `Cargo.toml`). **Proposed: skip** — the `cargo build` + the in-process `run()` Ok-return test covers the claim ("prints 'not yet implemented' and exits 0"). Adding `assert_cmd` as a dev-dep for a skeleton PR is overkill; PR #5's integration tests (mock gateway) will need broader infra anyway.
- **Regression test for current CLI:** confirm `herd --config foo.yaml` still parses the same way after the subcommand migration. A unit test on `Cli::try_parse_from(["herd", "-c", "foo.yaml"])` would catch breakage. **Proposed: include** — the subcommand migration is the riskiest part of PR #4, and a 5-line test is cheap insurance.

### Verification

- `cargo build`, `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check`.
- Manual smoke: `cargo run -- agent --gateway http://localhost:40114` → prints "not yet implemented", exits 0. `cargo run -- --config herd.yaml` still starts the server.

### Suggested commit message

```
feat(daemon): add herd agent subcommand skeleton

Migrates the CLI to a subcommand-shaped clap derive (Serve, Agent)
while preserving the flat-flag invocation (`herd -c herd.yaml`) as
the default when no subcommand is given. The `agent` subcommand
accepts --gateway and --node-id, prints "not yet implemented", and
exits 0. Wiring only — the real heartbeat client lands in PR #5.
```

---

## Open questions for the team lead

- **PR #3 — `NodeRegistry` TTL value.** Worktree tests use 30s. Is there a spec-dictated value in `docs/specs/v2-distributed-inference-spec.md` that should override? If yes, hard-code to that; otherwise 30s.
- **PR #3 — `AgentCapabilities` as wire payload.** The struct already derives `Deserialize` but includes runtime-ish fields (`ttft_p50_ms`, `queue_depth`, `vram_free_mb`) that an agent skeleton may not populate. Acceptable for the agent to send zeros/`None` for these in v1.2, OR do you want a separate `HeartbeatPayload` type with a narrower surface? Proposed: accept `AgentCapabilities` as-is; `#[serde(default)]` on optional-flavored fields lets agents omit them.
- **PR #3 — endpoint discoverability.** CLAUDE.md rule: "New endpoints must appear in `skills.md` and the dashboard Agent Guide tab." For an *internal* agent-control endpoint, should it be listed in `skills.md` (which is the LLM-facing skills reference) or only in a new "Internal API" section? Proposed: add to `skills.md` under a new `## Internal (agent control)` heading with a note "not for end-user use". Lead should confirm.
- **PR #4 — `--update` location.** Confirm it stays top-level and is NOT migrated to `herd update`. (Breaking change risk for users.) Proposed: stay top-level.
- **PR #4 — subcommand migration scope.** This is the one place PR #4 expands beyond a pure "skeleton". A clean subcommand tree requires moving all current `--config`/`--port`/`--host`/`--backend` flags into a `ServeArgs` struct. That's ~30 lines of churn in `main.rs`. Acceptable, or prefer Option B's ugly-but-minimal `--agent` flag? Proposed: Option A (clean migration) — doing it here keeps PR #5 focused on heartbeat-client logic.
