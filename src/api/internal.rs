//! Agent-facing internal endpoints (v1.2 distributed-inference foundation).
//!
//! These endpoints are called by `herd agent` daemons, not by API clients. The
//! only v1.2 endpoint is the heartbeat — agents POST their capability snapshot on
//! a cadence (default 2s) and the gateway tracks live liveness/capabilities in the
//! in-memory `NodeRegistry`. Unknown `node_id` values are registered implicitly on
//! first heartbeat; there is no separate registration endpoint.
//!
//! Auth: shared bearer token via the `HERD_AGENT_TOKEN` env var. The token is
//! required by default. Local self-tests can opt into an unauthenticated mode by
//! setting `HERD_ALLOW_UNAUTHENTICATED_AGENT_HEARTBEAT=true`.

use crate::nodes::{AgentCapabilities, HeartbeatOutcome, NodeRegistry};
use crate::server::{constant_time_eq, AppState};
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};

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
async fn process_heartbeat(
    registry: &NodeRegistry,
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

    let node_id = req.capabilities.node_id.clone();
    let outcome = registry.heartbeat(req.capabilities).await.map_err(|e| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": e.to_string()})),
        )
    })?;
    let registered = matches!(outcome, HeartbeatOutcome::Registered);
    if registered {
        tracing::info!("Agent node registered via heartbeat: {}", node_id);
    }

    Ok(HeartbeatResponse {
        registered,
        deployments_assigned: Vec::new(),
        next_heartbeat_secs: DEFAULT_HEARTBEAT_SECS,
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

    process_heartbeat(
        &state.node_registry,
        expected.as_deref(),
        allow_unauthenticated,
        &headers,
        req,
    )
    .await
    .map(Json)
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
        };
        let res = process_heartbeat(&reg, Some("secret"), false, &HeaderMap::new(), req).await;
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
        };
        let res = process_heartbeat(&reg, Some("secret"), false, &bearer("wrong"), req).await;
        assert_eq!(res.unwrap_err().0, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn rejects_unconfigured_token_by_default() {
        let reg = NodeRegistry::new(Duration::from_secs(30));
        let req = HeartbeatRequest {
            capabilities: sample_caps("a", "1.2.0"),
            timestamp: None,
        };
        let res = process_heartbeat(&reg, None, false, &HeaderMap::new(), req).await;
        assert_eq!(res.unwrap_err().0, StatusCode::UNAUTHORIZED);
        assert_eq!(reg.len().await, 0);
    }

    #[tokio::test]
    async fn accepts_correct_bearer_token_and_registers() {
        let reg = NodeRegistry::new(Duration::from_secs(30));
        let req = HeartbeatRequest {
            capabilities: sample_caps("citadel-5090", "1.2.0"),
            timestamp: None,
        };
        let resp = process_heartbeat(&reg, Some("secret"), false, &bearer("secret"), req)
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
        };
        let resp = process_heartbeat(&reg, Some("secret"), false, &headers, req)
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
        };
        let resp = process_heartbeat(&reg, None, true, &HeaderMap::new(), req)
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
            };
            let _ = process_heartbeat(&reg, None, true, &HeaderMap::new(), req).await;
        }
        // First registers, second updates.
        let resp = process_heartbeat(
            &reg,
            None,
            true,
            &HeaderMap::new(),
            HeartbeatRequest {
                capabilities: sample_caps("a", "1.2.0"),
                timestamp: None,
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
        };
        let res = process_heartbeat(&reg, None, true, &HeaderMap::new(), req).await;
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
        };
        let res = process_heartbeat(&reg, None, true, &HeaderMap::new(), req).await;
        assert_eq!(res.unwrap_err().0, StatusCode::BAD_REQUEST);
        assert_eq!(reg.len().await, 0);
    }

    #[tokio::test]
    async fn rejects_when_registry_is_full() {
        let reg = NodeRegistry::with_max_nodes(Duration::from_secs(30), 1);
        let first = HeartbeatRequest {
            capabilities: sample_caps("a", "1.2.0"),
            timestamp: None,
        };
        process_heartbeat(&reg, None, true, &HeaderMap::new(), first)
            .await
            .unwrap();

        let second = HeartbeatRequest {
            capabilities: sample_caps("b", "1.2.0"),
            timestamp: None,
        };
        let res = process_heartbeat(&reg, None, true, &HeaderMap::new(), second).await;
        assert_eq!(res.unwrap_err().0, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(reg.len().await, 1);
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
