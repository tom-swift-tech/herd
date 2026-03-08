# Herd — Lessons

## Never crash on configuration errors
**Trigger:** `anyhow::bail!` in server startup caused restart loop when `admin_api: true` but no `api_key` set.
**Rule:** Startup validation must degrade gracefully — warn and disable the feature, never crash. Users running Herd as a service (systemd, Windows service) will get stuck in a restart loop if startup panics or bails on a config issue.
**Pattern:** Replace `bail!` with `tracing::warn!` + fallback behavior for any non-fatal config validation.
