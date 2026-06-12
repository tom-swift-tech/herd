# Herd — Working TODO

> Scratchpad for in-flight work. Milestone tracking lives in `ROADMAP.md`;
> the v1.2 PR breakdown + acceptance checklist live in `tasks/HERD-V1.2-SPRINT.md`.

**Last updated:** 2026-06-12

---

## In flight — v1.2 PR #6c: `herd publish` (branch `feat/v1.2-pr6c-publish`)

Thin promote subcommand: copy a binary into `{publish_dir}/{version}/{os}-{arch}/herd[.exe]`,
print sha256 + reminder to bump `fleet.target_agent_version`. Design by architect-pr6c (locked).

**Decisions:** source = positional `[BINARY]`, default `current_exe()`; `--version` REQUIRED
(no default — wrong version is the one silent mis-serve); os/arch default to host consts,
overridable; publish-dir = `--publish-dir` > `HERD_AGENT_PUBLISH_DIR` env > `--config`'s
`fleet.publish_dir` > `~/.herd/binaries` (reuse `FleetConfig::publish_dir_from`); overwrite
refused on differing bytes without `--force`, identical bytes = idempotent; sha→stdout,
narration→stderr; logic in new `src/publish.rs` (sync `pub fn run`), `PublishArgs` in `cli.rs`.

### Build steps
- [ ] 1. `cli.rs`: add `PublishArgs` struct + `Command::Publish(PublishArgs)` variant.
- [ ] 2. `lib.rs`: `pub mod publish;` (keep alpha order).
- [ ] 3. `src/publish.rs`: `run()` (arg resolution + stdout/stderr) wrapping a testable
       `publish_inner(source, publish_dir, version, os, arch, force) -> Result<Outcome>`
       returning `Written(sha)` | `Unchanged(sha)`. Validate version/os/arch up front via
       `version_shaped`/`platform_shaped`; reuse `binary_path` + `BinaryStore::sha256_of`.
- [ ] 4. `publish.rs`: `publish_dir_from_config(&Path) -> Result<Option<String>>` (read-only
       `Config::from_file(...).fleet.publish_dir`, no `validate()`), called only when `--config` given.
- [ ] 5. `main.rs`: dispatch `Some(Command::Publish(args)) => herd::publish::run(args)` (SYNC, no await).
- [ ] 6. Tests: 11 unit (publish.rs, tempdir via `temp_dir()+process::id()` pattern — NO new dep) +
       4 CLI-parse (cli.rs `cli_tests`). Key: returned sha == `BinaryStore` sha (by-construction
       parity), refuse-overwrite, idempotent rehash, force-overwrite, malformed version/os/arch,
       missing source, create-parent-dirs, config resolution, `--version` required.
- [ ] 7. Docs: sprint-doc Decision 25 + flip #6c to ✅ in PR table; one README/fleet-docs line for
       the manual drop-in flow. NOT the dashboard Agent Guide tab (CLI cmd, not HTTP endpoint).

### Verification gate (operator, before PR)
- [ ] `cargo build` + `cargo test` (count grows from 468) + `cargo clippy --all-targets -- -D warnings` + `cargo fmt --check`

### Scope guard (OUT of #6c)
No auto-bump of target version; no list/prune/GC; no remote upload (local disk only);
no cross-compile/build invocation; no `latest` symlink; no full config `validate()`; no async.

### Done = green gate + reviewer CLEAN → commit + push → open PR vs `main`. Do NOT auto-merge.

---

## Backlog
- #7 — `BackendPool` integration (agent nodes route via in-memory registry freshness, not SQLite pool).
- #8 — Integration test: gateway + 1 agent in-process, request routes through agent's stub llama-server.
- v1.1.1 / v1.1.2 not git-tagged (tags stop at v1.1.0) — tag retroactively or at next release.
