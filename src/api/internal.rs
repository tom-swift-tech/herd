//! Agent-facing internal endpoints (v1.2 distributed-inference foundation).
//!
//! These endpoints are called by `herd agent` daemons, not by API clients. The
//! only v1.2 endpoint is the heartbeat — agents POST their capability snapshot on
//! a cadence (default 2s) and the gateway tracks live liveness/capabilities in the
//! in-memory `NodeRegistry`. Unknown `node_id` values are registered implicitly on
//! first heartbeat; there is no separate registration endpoint.
//!
//! Persistence: the handler also writes a `source='agent'` row to the SQLite
//! `nodes` table for Fleet-tab visibility — on registration and on material
//! capability change only (steady beats stay in-memory). The `NodeRegistry`
//! itself has no DB dependency; this glue lives here and in `server.rs`'s
//! evictor/reaper tasks.
//!
//! Auth: shared bearer token via the `HERD_AGENT_TOKEN` env var. The token is
//! required by default. Local self-tests can opt into an unauthenticated mode by
//! setting `HERD_ALLOW_UNAUTHENTICATED_AGENT_HEARTBEAT=true`.

use crate::nodes::{
    binary_store, AgentCapabilities, BinaryStore, HeartbeatOutcome, NodeDb, NodeRegistry,
};
use crate::server::{constant_time_eq, AppState};
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Default cadence the gateway asks agents to heartbeat at, in seconds.
const DEFAULT_HEARTBEAT_SECS: u64 = 2;
const MAX_HEARTBEAT_BODY_BYTES: usize = 64 * 1024;
const MAX_NODE_ID_LEN: usize = 128;
const MAX_AGENT_VERSION_LEN: usize = 128;
const MAX_ADDRESS_LEN: usize = 2048;
const MAX_MODELS_LOADED: usize = 256;
const MAX_MODEL_NAME_LEN: usize = 512;
static WARNED_UNAUTHENTICATED_HEARTBEAT: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Deserialize)]
pub struct HeartbeatRequest {
    pub capabilities: AgentCapabilities,
    /// Wall-clock timestamp from the agent. Accepted but ignored in v1.2 — the
    /// registry uses its own monotonic clock for freshness, so clock skew on
    /// agents cannot cause premature eviction.
    #[serde(default)]
    pub timestamp: Option<String>,
    /// The agent's announcement that it is about to restart for a self-update
    /// (PR #6b). Grants the node an eviction grace window and flips its Fleet
    /// row to status='updating'. Omitted by normal beats.
    #[serde(default)]
    pub updating: bool,
}

#[derive(Debug, Serialize)]
pub struct HeartbeatResponse {
    /// True if this heartbeat registered a previously-unknown node.
    pub registered: bool,
    /// Deployments the gateway wants this node to load. Always empty in v1.2
    /// (single-node only); populated when the DeploymentManager lands.
    pub deployments_assigned: Vec<serde_json::Value>,
    /// Cadence the agent should heartbeat at next, in seconds.
    pub next_heartbeat_secs: u64,
    /// Version the gateway wants agents to run (fleet version authority,
    /// PR #6). Always advertised; the agent decides whether to act.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_version: Option<String>,
    /// External download override: present ONLY when fleet.download_url_base
    /// is configured. In the default local case the gateway sends no URL at
    /// all — the agent constructs one from its own --gateway address, so it
    /// never fetches a URL derived from this request's Host header. Presence
    /// ⇔ external override.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub download_url: Option<String>,
    /// Hex sha256 of the published binary; the agent must verify before
    /// swapping. Present iff a binary is published for the agent's reported
    /// os/arch at the target version — this is what makes an offer actionable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

/// Resolved fleet-update inputs for one heartbeat: the version authority's
/// target plus where published binaries live. Built per-request from the
/// config snapshot (so config hot-reload takes effect without restart);
/// `None` in `process_heartbeat` means "no update advertising" (unit tests).
pub struct UpdateContext {
    pub target_version: String,
    pub publish_dir: PathBuf,
    pub download_url_base: Option<String>,
    pub store: Arc<BinaryStore>,
}

impl UpdateContext {
    pub fn from_config(
        fleet: &crate::config::FleetConfig,
        store: Arc<BinaryStore>,
        data_root: &std::path::Path,
    ) -> Self {
        Self {
            target_version: fleet.resolved_target_version(),
            publish_dir: fleet.resolved_publish_dir(data_root),
            download_url_base: fleet
                .download_url_base
                .as_deref()
                .map(|s| s.trim_end_matches('/').to_string())
                .filter(|s| !s.is_empty()),
            store,
        }
    }

    /// Compute the offer attachment for an agent's platform: the sha256 of
    /// the published binary, plus a download URL ONLY when an external
    /// download_url_base is configured. In the default local case no URL is
    /// sent — the agent constructs one from its own --gateway address
    /// (presence of download_url ⇔ external override; agents never fetch a
    /// URL derived from this request's Host header). Returns `None` when the
    /// agent didn't report os/arch or nothing is published for the target
    /// version on that platform. The sha always comes from the gateway's
    /// local publish dir — even with an external download base, the local
    /// copy is the source of truth for what was promoted.
    async fn offer_for(&self, caps: &AgentCapabilities) -> Option<(Option<String>, String)> {
        let os = caps.os.as_deref()?;
        let arch = caps.arch.as_deref()?;
        let path = binary_store::binary_path(&self.publish_dir, &self.target_version, os, arch)?;
        let sha256 = self.store.sha256_async(path).await?;
        // External base mirrors the publish-dir layout (static host, S3
        // sync, ...). Model B (e.g. GitHub release assets) swaps this for a
        // templated base later — agents treat the URL as opaque.
        let url = self.download_url_base.as_ref().map(|base| {
            let file = if os == "windows" { "herd.exe" } else { "herd" };
            format!("{base}/{}/{os}-{arch}/{file}", self.target_version)
        });
        Some((url, sha256))
    }
}

/// Validate the agent bearer token. Accepts `Authorization: Bearer <token>` or
/// `X-API-Key: <token>`. When `expected` is `None`, requests are rejected unless
/// `allow_unauthenticated` is true.
fn check_agent_token(
    headers: &HeaderMap,
    expected: Option<&str>,
    allow_unauthenticated: bool,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    let Some(expected) = expected else {
        if allow_unauthenticated {
            return Ok(());
        }
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": "HERD_AGENT_TOKEN is required for agent heartbeats"
            })),
        ));
    };
    let provided = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .or_else(|| headers.get("x-api-key").and_then(|v| v.to_str().ok()));
    match provided {
        Some(token) if constant_time_eq(token.as_bytes(), expected.as_bytes()) => Ok(()),
        _ => Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "Invalid or missing agent token"})),
        )),
    }
}

fn env_truthy(name: &str) -> bool {
    std::env::var(name)
        .map(|v| {
            matches!(
                v.as_str(),
                "1" | "true" | "TRUE" | "yes" | "YES" | "on" | "ON"
            )
        })
        .unwrap_or(false)
}

fn validate_capabilities(caps: &AgentCapabilities) -> Result<(), String> {
    if caps.node_id.is_empty() {
        return Err("capabilities.node_id is required".to_string());
    }
    if caps.node_id.len() > MAX_NODE_ID_LEN {
        return Err(format!(
            "capabilities.node_id exceeds {MAX_NODE_ID_LEN} bytes"
        ));
    }
    if !caps
        .node_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
    {
        return Err(
            "capabilities.node_id may only contain ASCII letters, digits, '-', '_', '.', or ':'"
                .to_string(),
        );
    }

    if caps.agent_version.is_empty() {
        return Err("capabilities.agent_version is required".to_string());
    }
    if caps.agent_version.len() > MAX_AGENT_VERSION_LEN {
        return Err(format!(
            "capabilities.agent_version exceeds {MAX_AGENT_VERSION_LEN} bytes"
        ));
    }

    if caps.address.len() > MAX_ADDRESS_LEN {
        return Err(format!(
            "capabilities.address exceeds {MAX_ADDRESS_LEN} bytes"
        ));
    }
    let parsed = reqwest::Url::parse(&caps.address)
        .map_err(|e| format!("capabilities.address is not a valid URL: {e}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("capabilities.address must use http or https".to_string());
    }
    if parsed.host_str().is_none() {
        return Err("capabilities.address must include a host".to_string());
    }

    if caps.vram_free_mb > caps.vram_total_mb {
        return Err("capabilities.vram_free_mb cannot exceed vram_total_mb".to_string());
    }
    if caps.models_loaded.len() > MAX_MODELS_LOADED {
        return Err(format!(
            "capabilities.models_loaded exceeds {MAX_MODELS_LOADED} entries"
        ));
    }
    if caps
        .models_loaded
        .iter()
        .any(|m| m.len() > MAX_MODEL_NAME_LEN)
    {
        return Err(format!(
            "capabilities.models_loaded entries must be <= {MAX_MODEL_NAME_LEN} bytes"
        ));
    }

    Ok(())
}

/// Core heartbeat logic, decoupled from axum extractors so it can be unit-tested
/// without constructing a full `AppState`. Checks auth, then records the
/// heartbeat in the registry (registering unknown nodes implicitly).
///
/// `node_db` is the SQLite write-through for Fleet visibility: rows are upserted
/// on registration and on material capability change (`models_loaded` set
/// differs), never on steady unchanged beats. A DB failure is logged and does
/// not fail the heartbeat — liveness must not depend on persistence.
async fn process_heartbeat(
    registry: &NodeRegistry,
    node_db: Option<&NodeDb>,
    update_ctx: Option<&UpdateContext>,
    expected_token: Option<&str>,
    allow_unauthenticated: bool,
    headers: &HeaderMap,
    req: HeartbeatRequest,
) -> Result<HeartbeatResponse, (StatusCode, Json<serde_json::Value>)> {
    check_agent_token(headers, expected_token, allow_unauthenticated)?;
    validate_capabilities(&req.capabilities).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e})),
        )
    })?;

    // Fleet version authority (PR #6): always advertise the target; attach a
    // sha (and, for an external base, a URL) only when a binary is published
    // for this agent's platform. Computed before the registry write so a slow
    // first-time hash can't hold state any longer than necessary.
    let mut target_version = None;
    let mut download_url = None;
    let mut sha256 = None;
    if let Some(ctx) = update_ctx {
        target_version = Some(ctx.target_version.clone());
        if let Some((url, sha)) = ctx.offer_for(&req.capabilities).await {
            download_url = url;
            sha256 = Some(sha);
        }
    }

    let node_id = req.capabilities.node_id.clone();
    let caps_for_db = node_db.map(|_| req.capabilities.clone());
    let outcome = registry
        .heartbeat_with(req.capabilities, req.updating)
        .await
        .map_err(|e| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": e.to_string()})),
            )
        })?;
    let registered = matches!(outcome, HeartbeatOutcome::Registered);
    if registered {
        tracing::info!("Agent node registered via heartbeat: {}", node_id);
    }

    // Persist on registration and on material change. version_changed covers
    // the restarted agent's first beat after a self-update with a new binary.
    // update_cleared covers a failed respawn: the agent keeps running the old
    // binary (same version, no model change) but the Fleet row must still
    // un-stick from 'updating' → 'online'.
    let persist = match outcome {
        HeartbeatOutcome::Registered => true,
        HeartbeatOutcome::Updated {
            models_changed,
            version_changed,
            update_cleared,
        } => models_changed || version_changed || update_cleared,
    };
    if persist {
        if let (Some(db), Some(caps)) = (node_db, caps_for_db) {
            if let Err(e) = db.upsert_agent_node(&caps) {
                tracing::error!("Failed to persist agent node {}: {}", node_id, e);
            }
        }
    }

    // An updating beat flips the Fleet row to 'updating' so the restart gap
    // reads as a deliberate update, not an outage. After the upsert above so
    // the final state of a (rare) updating+material-change beat is 'updating'.
    if req.updating {
        if let Some(db) = node_db {
            if let Err(e) = db.mark_agent_updating(&node_id) {
                tracing::error!("Failed to mark agent node {} updating: {}", node_id, e);
            }
        }
    }

    Ok(HeartbeatResponse {
        registered,
        deployments_assigned: Vec::new(),
        next_heartbeat_secs: DEFAULT_HEARTBEAT_SECS,
        target_version,
        download_url,
        sha256,
    })
}

/// POST /api/internal/nodes/heartbeat — called by `herd agent` daemons.
pub async fn heartbeat(
    State(state): State<AppState>,
    request: axum::extract::Request,
) -> Result<Json<HeartbeatResponse>, (StatusCode, Json<serde_json::Value>)> {
    let headers = request.headers().clone();
    let body = axum::body::to_bytes(request.into_body(), MAX_HEARTBEAT_BODY_BYTES)
        .await
        .map_err(|_| {
            (
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(serde_json::json!({"error": "Heartbeat payload too large"})),
            )
        })?;

    let expected = std::env::var("HERD_AGENT_TOKEN")
        .ok()
        .filter(|s| !s.is_empty());
    let allow_unauthenticated = env_truthy("HERD_ALLOW_UNAUTHENTICATED_AGENT_HEARTBEAT");
    if expected.is_none()
        && allow_unauthenticated
        && !WARNED_UNAUTHENTICATED_HEARTBEAT.swap(true, Ordering::Relaxed)
    {
        tracing::warn!(
            "HERD_AGENT_TOKEN not set — accepting agent heartbeat without authentication"
        );
    }

    // Parse manually so a malformed body yields a clean 400 rather than axum's
    // default rejection, and so we keep control over extractor ordering.
    let req: HeartbeatRequest = serde_json::from_slice(&body).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": format!("Invalid heartbeat payload: {}", e)})),
        )
    })?;

    let config_snap = state.config.read().await.clone();
    let data_root = config_snap.resolved_data_dir();
    let update_ctx =
        UpdateContext::from_config(&config_snap.fleet, state.binary_store.clone(), &data_root);

    process_heartbeat(
        &state.node_registry,
        Some(&state.node_db),
        Some(&update_ctx),
        expected.as_deref(),
        allow_unauthenticated,
        &headers,
        req,
    )
    .await
    .map(Json)
}

/// Core of the binary download endpoint, decoupled from axum extractors and
/// `AppState` so path validation / 404 / streaming behavior is unit-testable
/// (same split as `process_heartbeat`). Auth happens in the public handler.
async fn serve_binary(
    publish_dir: &std::path::Path,
    store: &Arc<BinaryStore>,
    version: &str,
    platform: &str,
) -> Result<axum::response::Response, (StatusCode, Json<serde_json::Value>)> {
    let (os, arch) = platform.split_once('-').ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "platform must be {os}-{arch}, e.g. linux-x86_64"})),
        )
    })?;

    let path = binary_store::binary_path(publish_dir, version, os, arch).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "invalid version or platform component"})),
        )
    })?;

    let not_found = || {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": format!("no published binary for version {version} on {platform}")
            })),
        )
    };
    let meta = tokio::fs::metadata(&path).await.map_err(|_| not_found())?;
    if !meta.is_file() {
        return Err(not_found());
    }
    let file = tokio::fs::File::open(&path)
        .await
        .map_err(|_| not_found())?;

    // Best-effort integrity header for manual/curl consumers; agents use the
    // sha from the heartbeat response.
    let sha256 = store.sha256_async(path.clone()).await;

    let body = axum::body::Body::from_stream(tokio_util::io::ReaderStream::new(file));
    let mut builder = axum::response::Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "application/octet-stream")
        .header(axum::http::header::CONTENT_LENGTH, meta.len());
    if let Some(sha) = sha256 {
        builder = builder.header("X-Herd-Sha256", sha);
    }
    builder.body(body).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("failed to build response: {e}")})),
        )
    })
}

/// GET /api/internal/nodes/binary/:version/:platform — serves a published
/// agent binary from the fleet publish dir (`herd agent` self-update
/// download path). `:platform` is `{os}-{arch}`, e.g. `windows-x86_64`.
/// Same bearer auth as the heartbeat.
pub async fn download_binary(
    State(state): State<AppState>,
    axum::extract::Path((version, platform)): axum::extract::Path<(String, String)>,
    headers: HeaderMap,
) -> Result<axum::response::Response, (StatusCode, Json<serde_json::Value>)> {
    let expected = std::env::var("HERD_AGENT_TOKEN")
        .ok()
        .filter(|s| !s.is_empty());
    let allow_unauthenticated = env_truthy("HERD_ALLOW_UNAUTHENTICATED_AGENT_HEARTBEAT");
    check_agent_token(&headers, expected.as_deref(), allow_unauthenticated)?;

    let config_snap = state.config.read().await.clone();
    let data_root = config_snap.resolved_data_dir();
    serve_binary(
        &config_snap.fleet.resolved_publish_dir(&data_root),
        &state.binary_store,
        &version,
        &platform,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BackendType;
    use std::time::Duration;

    fn sample_caps(node_id: &str, version: &str) -> AgentCapabilities {
        AgentCapabilities {
            node_id: node_id.to_string(),
            backend: BackendType::LlamaServer,
            address: "http://127.0.0.1:8080".to_string(),
            gpu_model: Some("RTX 5090".to_string()),
            vram_total_mb: 32_768,
            vram_free_mb: 30_000,
            models_loaded: vec!["llama-3-8b".to_string()],
            queue_depth: 0,
            ttft_p50_ms: Some(42),
            rpc_capable: false,
            rpc_port: None,
            agent_version: version.to_string(),
            os: Some(std::env::consts::OS.to_string()),
            arch: Some(std::env::consts::ARCH.to_string()),
        }
    }

    fn bearer(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("authorization", format!("Bearer {token}").parse().unwrap());
        h
    }

    #[tokio::test]
    async fn rejects_missing_token_when_configured() {
        let reg = NodeRegistry::new(Duration::from_secs(30));
        let req = HeartbeatRequest {
            capabilities: sample_caps("a", "1.2.0"),
            timestamp: None,
            updating: false,
        };
        let res = process_heartbeat(
            &reg,
            None,
            None,
            Some("secret"),
            false,
            &HeaderMap::new(),
            req,
        )
        .await;
        let err = res.unwrap_err();
        assert_eq!(err.0, StatusCode::UNAUTHORIZED);
        assert_eq!(reg.len().await, 0, "rejected heartbeat must not register");
    }

    #[tokio::test]
    async fn rejects_wrong_token() {
        let reg = NodeRegistry::new(Duration::from_secs(30));
        let req = HeartbeatRequest {
            capabilities: sample_caps("a", "1.2.0"),
            timestamp: None,
            updating: false,
        };
        let res = process_heartbeat(
            &reg,
            None,
            None,
            Some("secret"),
            false,
            &bearer("wrong"),
            req,
        )
        .await;
        assert_eq!(res.unwrap_err().0, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn rejects_unconfigured_token_by_default() {
        let reg = NodeRegistry::new(Duration::from_secs(30));
        let req = HeartbeatRequest {
            capabilities: sample_caps("a", "1.2.0"),
            timestamp: None,
            updating: false,
        };
        let res = process_heartbeat(&reg, None, None, None, false, &HeaderMap::new(), req).await;
        assert_eq!(res.unwrap_err().0, StatusCode::UNAUTHORIZED);
        assert_eq!(reg.len().await, 0);
    }

    #[tokio::test]
    async fn accepts_correct_bearer_token_and_registers() {
        let reg = NodeRegistry::new(Duration::from_secs(30));
        let req = HeartbeatRequest {
            capabilities: sample_caps("citadel-5090", "1.2.0"),
            timestamp: None,
            updating: false,
        };
        let resp = process_heartbeat(
            &reg,
            None,
            None,
            Some("secret"),
            false,
            &bearer("secret"),
            req,
        )
        .await
        .unwrap();
        assert!(resp.registered);
        assert_eq!(resp.next_heartbeat_secs, DEFAULT_HEARTBEAT_SECS);
        assert!(resp.deployments_assigned.is_empty());
        assert_eq!(reg.len().await, 1);
    }

    #[tokio::test]
    async fn accepts_x_api_key_header() {
        let reg = NodeRegistry::new(Duration::from_secs(30));
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", "secret".parse().unwrap());
        let req = HeartbeatRequest {
            capabilities: sample_caps("a", "1.2.0"),
            timestamp: None,
            updating: false,
        };
        let resp = process_heartbeat(&reg, None, None, Some("secret"), false, &headers, req)
            .await
            .unwrap();
        assert!(resp.registered);
    }

    #[tokio::test]
    async fn allows_when_explicitly_unsecured() {
        let reg = NodeRegistry::new(Duration::from_secs(30));
        let req = HeartbeatRequest {
            capabilities: sample_caps("a", "1.2.0"),
            timestamp: None,
            updating: false,
        };
        let resp = process_heartbeat(&reg, None, None, None, true, &HeaderMap::new(), req)
            .await
            .unwrap();
        assert!(resp.registered);
        assert_eq!(reg.len().await, 1);
    }

    #[tokio::test]
    async fn second_heartbeat_is_not_registered() {
        let reg = NodeRegistry::new(Duration::from_secs(30));
        for _ in 0..2 {
            let req = HeartbeatRequest {
                capabilities: sample_caps("a", "1.2.0"),
                timestamp: None,
                updating: false,
            };
            let _ = process_heartbeat(&reg, None, None, None, true, &HeaderMap::new(), req).await;
        }
        // First registers, second updates.
        let resp = process_heartbeat(
            &reg,
            None,
            None,
            None,
            true,
            &HeaderMap::new(),
            HeartbeatRequest {
                capabilities: sample_caps("a", "1.2.0"),
                timestamp: None,
                updating: false,
            },
        )
        .await
        .unwrap();
        assert!(
            !resp.registered,
            "known node_id should report registered=false"
        );
        assert_eq!(reg.len().await, 1);
    }

    #[tokio::test]
    async fn rejects_invalid_node_id() {
        let reg = NodeRegistry::new(Duration::from_secs(30));
        let req = HeartbeatRequest {
            capabilities: sample_caps("bad/id", "1.2.0"),
            timestamp: None,
            updating: false,
        };
        let res = process_heartbeat(&reg, None, None, None, true, &HeaderMap::new(), req).await;
        assert_eq!(res.unwrap_err().0, StatusCode::BAD_REQUEST);
        assert_eq!(reg.len().await, 0);
    }

    #[tokio::test]
    async fn rejects_invalid_address() {
        let reg = NodeRegistry::new(Duration::from_secs(30));
        let mut caps = sample_caps("a", "1.2.0");
        caps.address = "file:///tmp/socket".to_string();
        let req = HeartbeatRequest {
            capabilities: caps,
            timestamp: None,
            updating: false,
        };
        let res = process_heartbeat(&reg, None, None, None, true, &HeaderMap::new(), req).await;
        assert_eq!(res.unwrap_err().0, StatusCode::BAD_REQUEST);
        assert_eq!(reg.len().await, 0);
    }

    #[tokio::test]
    async fn rejects_when_registry_is_full() {
        let reg = NodeRegistry::with_max_nodes(Duration::from_secs(30), 1);
        let first = HeartbeatRequest {
            capabilities: sample_caps("a", "1.2.0"),
            timestamp: None,
            updating: false,
        };
        process_heartbeat(&reg, None, None, None, true, &HeaderMap::new(), first)
            .await
            .unwrap();

        let second = HeartbeatRequest {
            capabilities: sample_caps("b", "1.2.0"),
            timestamp: None,
            updating: false,
        };
        let res = process_heartbeat(&reg, None, None, None, true, &HeaderMap::new(), second).await;
        assert_eq!(res.unwrap_err().0, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(reg.len().await, 1);
    }

    fn beat(caps: AgentCapabilities) -> HeartbeatRequest {
        HeartbeatRequest {
            capabilities: caps,
            timestamp: None,
            updating: false,
        }
    }

    #[tokio::test]
    async fn first_heartbeat_persists_agent_row() {
        let reg = NodeRegistry::new(Duration::from_secs(30));
        let db = NodeDb::open_in_memory().unwrap();
        let resp = process_heartbeat(
            &reg,
            Some(&db),
            None,
            None,
            true,
            &HeaderMap::new(),
            beat(sample_caps("citadel-5090", "1.2.0")),
        )
        .await
        .unwrap();
        assert!(resp.registered);

        let nodes = db.list_nodes().unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].source, "agent");
        assert_eq!(nodes[0].status, "online");
        assert_eq!(nodes[0].agent_version.as_deref(), Some("1.2.0"));
    }

    #[tokio::test]
    async fn unchanged_beat_does_not_touch_db() {
        let reg = NodeRegistry::new(Duration::from_secs(30));
        let db = NodeDb::open_in_memory().unwrap();
        process_heartbeat(
            &reg,
            Some(&db),
            None,
            None,
            true,
            &HeaderMap::new(),
            beat(sample_caps("a", "1.2.0")),
        )
        .await
        .unwrap();

        // Sentinel: if the handler wrote on this beat, the upsert would flip
        // the row back to 'online'.
        db.mark_agent_offline("a").unwrap();

        // Same capability snapshot — steady-state beat must not write.
        let mut caps = sample_caps("a", "1.2.0");
        caps.vram_free_mb = 1; // dynamic field churn is not material
        process_heartbeat(
            &reg,
            Some(&db),
            None,
            None,
            true,
            &HeaderMap::new(),
            beat(caps),
        )
        .await
        .unwrap();

        let nodes = db.list_nodes().unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(
            nodes[0].status, "offline",
            "unchanged beat must not write to SQLite"
        );
    }

    #[tokio::test]
    async fn model_change_beat_updates_db() {
        let reg = NodeRegistry::new(Duration::from_secs(30));
        let db = NodeDb::open_in_memory().unwrap();
        process_heartbeat(
            &reg,
            Some(&db),
            None,
            None,
            true,
            &HeaderMap::new(),
            beat(sample_caps("a", "1.2.0")),
        )
        .await
        .unwrap();

        let mut caps = sample_caps("a", "1.2.0");
        caps.models_loaded = vec!["llama-3-8b".to_string(), "qwen3-32b".to_string()];
        process_heartbeat(
            &reg,
            Some(&db),
            None,
            None,
            true,
            &HeaderMap::new(),
            beat(caps),
        )
        .await
        .unwrap();

        let nodes = db.list_nodes().unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(
            nodes[0].models_loaded,
            vec!["llama-3-8b".to_string(), "qwen3-32b".to_string()]
        );
    }

    #[tokio::test]
    async fn rejected_heartbeat_does_not_persist() {
        let reg = NodeRegistry::new(Duration::from_secs(30));
        let db = NodeDb::open_in_memory().unwrap();
        let res = process_heartbeat(
            &reg,
            Some(&db),
            None,
            Some("secret"),
            false,
            &HeaderMap::new(),
            beat(sample_caps("a", "1.2.0")),
        )
        .await;
        assert!(res.is_err());
        assert!(db.list_nodes().unwrap().is_empty());
    }

    // ---- self-update beats and version-change persistence (PR #6b) ----

    fn beat_updating(caps: AgentCapabilities) -> HeartbeatRequest {
        HeartbeatRequest {
            capabilities: caps,
            timestamp: None,
            updating: true,
        }
    }

    #[tokio::test]
    async fn updating_beat_marks_row_updating() {
        let reg = NodeRegistry::new(Duration::from_secs(30));
        let db = NodeDb::open_in_memory().unwrap();
        process_heartbeat(
            &reg,
            Some(&db),
            None,
            None,
            true,
            &HeaderMap::new(),
            beat(sample_caps("a", "1.2.0")),
        )
        .await
        .unwrap();

        process_heartbeat(
            &reg,
            Some(&db),
            None,
            None,
            true,
            &HeaderMap::new(),
            beat_updating(sample_caps("a", "1.2.0")),
        )
        .await
        .unwrap();

        let nodes = db.list_nodes().unwrap();
        assert_eq!(nodes[0].status, "updating");
        assert!(
            reg.get("a").await.unwrap().updating_since.is_some(),
            "registry must arm the eviction grace window"
        );
    }

    #[tokio::test]
    async fn version_change_beat_persists_and_clears_updating() {
        // The full restart round-trip: updating beat, then the restarted
        // agent's first beat with the new version. models_loaded is unchanged,
        // so only version_changed can trigger the persist — without it the
        // row would stay stuck at 'updating' with the old agent_version.
        let reg = NodeRegistry::new(Duration::from_secs(30));
        let db = NodeDb::open_in_memory().unwrap();
        for req in [
            beat(sample_caps("a", "1.2.0")),
            beat_updating(sample_caps("a", "1.2.0")),
        ] {
            process_heartbeat(&reg, Some(&db), None, None, true, &HeaderMap::new(), req)
                .await
                .unwrap();
        }
        assert_eq!(db.list_nodes().unwrap()[0].status, "updating");

        process_heartbeat(
            &reg,
            Some(&db),
            None,
            None,
            true,
            &HeaderMap::new(),
            beat(sample_caps("a", "1.3.0")),
        )
        .await
        .unwrap();

        let nodes = db.list_nodes().unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(
            nodes[0].status, "online",
            "version beat must clear 'updating'"
        );
        assert_eq!(nodes[0].agent_version.as_deref(), Some("1.3.0"));
        assert!(
            reg.get("a").await.unwrap().updating_since.is_none(),
            "normal beat must disarm the grace window"
        );
    }

    #[tokio::test]
    async fn updating_beat_with_unchanged_caps_does_not_upsert() {
        // The updating flag flips status only — it must not count as a
        // material change that rewrites the whole row.
        let reg = NodeRegistry::new(Duration::from_secs(30));
        let db = NodeDb::open_in_memory().unwrap();
        process_heartbeat(
            &reg,
            Some(&db),
            None,
            None,
            true,
            &HeaderMap::new(),
            beat(sample_caps("a", "1.2.0")),
        )
        .await
        .unwrap();

        // Sentinel: an upsert would flip status back to 'online'; the
        // mark_agent_updating path must set 'updating' instead.
        db.mark_agent_offline("a").unwrap();
        process_heartbeat(
            &reg,
            Some(&db),
            None,
            None,
            true,
            &HeaderMap::new(),
            beat_updating(sample_caps("a", "1.2.0")),
        )
        .await
        .unwrap();
        assert_eq!(db.list_nodes().unwrap()[0].status, "updating");
    }

    #[tokio::test]
    async fn normal_beat_with_same_version_clears_stuck_updating_row() {
        // Failed-respawn scenario: the agent sent an updating beat, then
        // restart failed and the OLD binary kept running. The agent resumes
        // normal beats with the same version — no version_changed, no
        // models_changed. The Fleet row must still flip from 'updating' back
        // to 'online' (update_cleared trigger).
        let reg = NodeRegistry::new(Duration::from_secs(30));
        let db = NodeDb::open_in_memory().unwrap();
        for req in [
            beat(sample_caps("a", "1.2.0")),
            beat_updating(sample_caps("a", "1.2.0")),
        ] {
            process_heartbeat(&reg, Some(&db), None, None, true, &HeaderMap::new(), req)
                .await
                .unwrap();
        }
        assert_eq!(
            db.list_nodes().unwrap()[0].status,
            "updating",
            "sanity: row must be 'updating' before the fix beats"
        );

        // Same version as before the update attempt — failed respawn kept old binary.
        process_heartbeat(
            &reg,
            Some(&db),
            None,
            None,
            true,
            &HeaderMap::new(),
            beat(sample_caps("a", "1.2.0")),
        )
        .await
        .unwrap();

        let nodes = db.list_nodes().unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(
            nodes[0].status, "online",
            "normal beat with same version must clear 'updating' (failed-respawn fix)"
        );
        assert!(
            reg.get("a").await.unwrap().updating_since.is_none(),
            "registry grace window must also be cleared"
        );
    }

    // ---- fleet version-authority response fields (PR #6) ----

    fn temp_publish_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("herd-pub-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Drop a fake published binary into the store layout for this host's
    /// platform; returns its path.
    fn publish_binary(dir: &std::path::Path, version: &str, contents: &[u8]) -> PathBuf {
        let path = crate::nodes::binary_store::binary_path(
            dir,
            version,
            std::env::consts::OS,
            std::env::consts::ARCH,
        )
        .unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn update_ctx(target: &str, dir: PathBuf, base: Option<&str>) -> UpdateContext {
        UpdateContext {
            target_version: target.to_string(),
            publish_dir: dir,
            download_url_base: base.map(|s| s.trim_end_matches('/').to_string()),
            store: Arc::new(BinaryStore::new()),
        }
    }

    fn host(name: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(axum::http::header::HOST, name.parse().unwrap());
        h
    }

    #[tokio::test]
    async fn advertises_target_without_offer_when_nothing_published() {
        let reg = NodeRegistry::new(Duration::from_secs(30));
        let ctx = update_ctx("9.9.9", temp_publish_dir("empty"), None);
        let resp = process_heartbeat(
            &reg,
            None,
            Some(&ctx),
            None,
            true,
            &host("gw.example:40114"),
            beat(sample_caps("a", "1.2.0")),
        )
        .await
        .unwrap();
        assert_eq!(resp.target_version.as_deref(), Some("9.9.9"));
        assert!(resp.download_url.is_none());
        assert!(resp.sha256.is_none());
    }

    #[tokio::test]
    async fn local_offer_sends_sha_only_and_ignores_host_header() {
        // PR #6b presence-as-signal: in the local case the gateway attaches
        // the sha but NO download_url — even with a Host header available.
        // The agent constructs the URL from its own --gateway address;
        // download_url presence is reserved for the external override.
        let reg = NodeRegistry::new(Duration::from_secs(30));
        let dir = temp_publish_dir("offer");
        publish_binary(&dir, "9.9.9", b"abc");
        let ctx = update_ctx("9.9.9", dir, None);

        let resp = process_heartbeat(
            &reg,
            None,
            Some(&ctx),
            None,
            true,
            &host("gw.example:40114"),
            beat(sample_caps("a", "1.2.0")),
        )
        .await
        .unwrap();

        assert_eq!(resp.target_version.as_deref(), Some("9.9.9"));
        assert!(
            resp.download_url.is_none(),
            "local case must never send a download_url, Host header or not"
        );
        // sha256("abc")
        assert_eq!(
            resp.sha256.as_deref(),
            Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
    }

    #[tokio::test]
    async fn omits_offer_for_agent_without_os_arch() {
        // Pre-PR#6 agents don't report os/arch — they must still get a normal
        // response (with the target advertised) and never a download offer.
        let reg = NodeRegistry::new(Duration::from_secs(30));
        let dir = temp_publish_dir("noplatform");
        publish_binary(&dir, "9.9.9", b"abc");
        let ctx = update_ctx("9.9.9", dir, None);

        let mut caps = sample_caps("legacy", "1.1.0");
        caps.os = None;
        caps.arch = None;
        let resp = process_heartbeat(
            &reg,
            None,
            Some(&ctx),
            None,
            true,
            &host("gw.example:40114"),
            beat(caps),
        )
        .await
        .unwrap();
        assert!(resp.registered);
        assert_eq!(resp.target_version.as_deref(), Some("9.9.9"));
        assert!(resp.download_url.is_none());
        assert!(resp.sha256.is_none());
    }

    #[tokio::test]
    async fn local_offer_works_without_host_header() {
        // The local case no longer depends on the Host header at all: the
        // sha is attached regardless, and the agent supplies its own URL.
        let reg = NodeRegistry::new(Duration::from_secs(30));
        let dir = temp_publish_dir("nohost");
        publish_binary(&dir, "9.9.9", b"abc");
        let ctx = update_ctx("9.9.9", dir, None);

        let resp = process_heartbeat(
            &reg,
            None,
            Some(&ctx),
            None,
            true,
            &HeaderMap::new(),
            beat(sample_caps("a", "1.2.0")),
        )
        .await
        .unwrap();
        assert_eq!(resp.target_version.as_deref(), Some("9.9.9"));
        assert!(resp.download_url.is_none());
        assert!(resp.sha256.is_some(), "sha must not require a Host header");
    }

    #[tokio::test]
    async fn uses_external_download_base_when_configured() {
        let reg = NodeRegistry::new(Duration::from_secs(30));
        let dir = temp_publish_dir("extbase");
        publish_binary(&dir, "9.9.9", b"abc");
        let ctx = update_ctx("9.9.9", dir, Some("https://cdn.example/herd/"));

        let resp = process_heartbeat(
            &reg,
            None,
            Some(&ctx),
            None,
            true,
            &HeaderMap::new(), // external base needs no Host header
            beat(sample_caps("a", "1.2.0")),
        )
        .await
        .unwrap();

        let file = if std::env::consts::OS == "windows" {
            "herd.exe"
        } else {
            "herd"
        };
        assert_eq!(
            resp.download_url.as_deref(),
            Some(
                format!(
                    "https://cdn.example/herd/9.9.9/{}-{}/{file}",
                    std::env::consts::OS,
                    std::env::consts::ARCH
                )
                .as_str()
            )
        );
        // The sha still comes from the local publish dir — source of truth.
        assert_eq!(
            resp.sha256.as_deref(),
            Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
    }

    #[tokio::test]
    async fn legacy_response_shape_without_update_context() {
        // With no UpdateContext (pre-PR#6 shape) the new fields must not
        // appear on the wire at all.
        let reg = NodeRegistry::new(Duration::from_secs(30));
        let resp = process_heartbeat(
            &reg,
            None,
            None,
            None,
            true,
            &HeaderMap::new(),
            beat(sample_caps("a", "1.2.0")),
        )
        .await
        .unwrap();
        let wire = serde_json::to_value(&resp).unwrap();
        assert!(wire.get("target_version").is_none());
        assert!(wire.get("download_url").is_none());
        assert!(wire.get("sha256").is_none());
        assert!(wire.get("registered").is_some());
    }

    // ---- binary download endpoint core (PR #6) ----

    #[tokio::test]
    async fn serve_binary_streams_published_file_with_integrity_headers() {
        let dir = temp_publish_dir("serve");
        publish_binary(&dir, "9.9.9", b"abc");
        let store = Arc::new(BinaryStore::new());
        let platform = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);

        let resp = serve_binary(&dir, &store, "9.9.9", &platform)
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "application/octet-stream"
        );
        assert_eq!(resp.headers().get("content-length").unwrap(), "3");
        assert_eq!(
            resp.headers().get("X-Herd-Sha256").unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );

        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], b"abc");
    }

    #[tokio::test]
    async fn serve_binary_rejects_malformed_platform_and_traversal() {
        let dir = temp_publish_dir("serve-bad");
        let store = Arc::new(BinaryStore::new());

        let err = serve_binary(&dir, &store, "9.9.9", "noseparator")
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);

        let err = serve_binary(&dir, &store, "../../etc", "linux-x86_64")
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);

        let err = serve_binary(&dir, &store, "9.9.9", "linux-x86_64/..")
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn serve_binary_404s_when_nothing_published() {
        let dir = temp_publish_dir("serve-missing");
        let store = Arc::new(BinaryStore::new());
        let err = serve_binary(&dir, &store, "9.9.9", "linux-x86_64")
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::NOT_FOUND);
    }

    #[test]
    fn malformed_payload_fails_to_deserialize() {
        let err = serde_json::from_slice::<HeartbeatRequest>(b"{ not valid json ");
        assert!(err.is_err());
    }

    #[test]
    fn tolerates_version_skew_and_unknown_fields() {
        // A future agent version plus an unknown field must still deserialize —
        // the gateway accepts newer agents (forward-compatible heartbeats).
        let json = serde_json::json!({
            "capabilities": {
                "node_id": "future-node",
                "backend": "llama-server",
                "address": "http://10.0.0.5:8080",
                "vram_total_mb": 24576,
                "vram_free_mb": 20000,
                "agent_version": "9.9.9-future",
                "some_unknown_future_field": {"nested": true}
            },
            "timestamp": "2026-06-05T00:00:00Z",
            "another_unknown_top_level": 1
        });
        let req: HeartbeatRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.capabilities.agent_version, "9.9.9-future");
        assert_eq!(req.capabilities.node_id, "future-node");
    }
}
