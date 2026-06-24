//! Integration tests for the `herd agent` daemon (v1.2 PR #4).
//!
//! The non-guarded tests run the real `HeartbeatClient` against a stub
//! gateway speaking the heartbeat contract, over real HTTP on an ephemeral
//! port. The CITADEL "both modes on one host" self-test boots the full
//! gateway (`server::run`) plus the daemon loop and is `#[ignore]`d — run it
//! with `cargo test --test agent_daemon -- --ignored`.

use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::post;
use axum::{Json, Router};
use herd::config::BackendType;
use herd::daemon::capabilities::{GpuStatic, GpuVendor, SnapshotBuilder};
use herd::daemon::client::{BeatOutcome, HeartbeatClient};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// (authorization header, request body) per received heartbeat.
type ReceivedBeats = Arc<Mutex<Vec<(Option<String>, serde_json::Value)>>>;

#[derive(Clone, Default)]
struct StubGateway {
    received: ReceivedBeats,
}

async fn stub_heartbeat(
    State(stub): State<StubGateway>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let mut received = stub.received.lock().unwrap();
    let registered = received.is_empty();
    received.push((auth, body));
    Json(serde_json::json!({
        "registered": registered,
        "deployments_assigned": [],
        "next_heartbeat_secs": 2
    }))
}

async fn spawn_stub_gateway() -> (StubGateway, SocketAddr) {
    let stub = StubGateway::default();
    let app = Router::new()
        .route("/api/internal/nodes/heartbeat", post(stub_heartbeat))
        .with_state(stub.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (stub, addr)
}

fn test_snapshot(node_id: &str) -> herd::nodes::AgentCapabilities {
    let builder = SnapshotBuilder::new(
        node_id.to_string(),
        GpuStatic {
            vendor: GpuVendor::Nvidia,
            model: Some("NVIDIA GeForce RTX 5090".into()),
            vram_total_mb: 32607,
            driver_version: Some("572.83".into()),
        },
    );
    builder.snapshot(
        BackendType::LlamaServer,
        "http://127.0.0.1:8080".into(),
        30_000,
        vec!["qwen3-32b.gguf".into()],
        Some(1),
        Some(4),
        None,
    )
}

#[tokio::test]
async fn heartbeat_round_trip_with_bearer_token() {
    let (stub, addr) = spawn_stub_gateway().await;
    let client =
        HeartbeatClient::new(&format!("http://{addr}"), Some("test-token".into())).unwrap();
    let caps = test_snapshot("citadel-5090");

    let first = client.send(&caps, false).await;
    assert_eq!(
        first,
        BeatOutcome::Success {
            registered: true,
            next_heartbeat_secs: Some(2),
            update_offer: None
        }
    );

    let second = client.send(&caps, false).await;
    assert_eq!(
        second,
        BeatOutcome::Success {
            registered: false,
            next_heartbeat_secs: Some(2),
            update_offer: None
        }
    );

    let received = stub.received.lock().unwrap();
    assert_eq!(received.len(), 2);
    let (auth, body) = &received[0];
    assert_eq!(auth.as_deref(), Some("Bearer test-token"));
    assert_eq!(body["capabilities"]["node_id"], "citadel-5090");
    assert_eq!(body["capabilities"]["backend"], "llama-server");
    assert_eq!(body["capabilities"]["vram_total_mb"], 32607);
    assert_eq!(body["capabilities"]["models_loaded"][0], "qwen3-32b.gguf");
    assert!(
        body["timestamp"].is_string(),
        "timestamp field must be sent"
    );
    assert!(
        body.get("updating").is_none(),
        "normal beats must not carry the updating flag"
    );
}

#[tokio::test]
async fn final_updating_beat_carries_the_flag_on_the_wire() {
    let (stub, addr) = spawn_stub_gateway().await;
    let client = HeartbeatClient::new(&format!("http://{addr}"), None).unwrap();

    let outcome = client.send(&test_snapshot("citadel-5090"), true).await;
    assert!(matches!(outcome, BeatOutcome::Success { .. }));

    let received = stub.received.lock().unwrap();
    assert_eq!(received[0].1["updating"], true);
}

#[tokio::test]
async fn heartbeat_omits_authorization_header_when_token_unset() {
    let (stub, addr) = spawn_stub_gateway().await;
    let client = HeartbeatClient::new(&format!("http://{addr}"), None).unwrap();

    let outcome = client.send(&test_snapshot("minipc"), false).await;
    assert!(matches!(outcome, BeatOutcome::Success { .. }));

    let received = stub.received.lock().unwrap();
    assert_eq!(received[0].0, None, "no Authorization header expected");
}

#[tokio::test]
async fn unreachable_gateway_reports_unreachable() {
    // Bind-then-drop to get a port with nothing listening.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let client = HeartbeatClient::new(&format!("http://{addr}"), None).unwrap();
    let outcome = client.send(&test_snapshot("ghost"), false).await;
    assert!(matches!(outcome, BeatOutcome::Unreachable(_)));
}

/// CITADEL self-test: real gateway (`herd serve` internals) and the real
/// daemon loop in one process on one host. Touches the user's herd SQLite/
/// analytics paths and binds a real port, hence `#[ignore]`.
#[tokio::test]
#[ignore]
async fn both_modes_on_one_host_self_test() {
    std::env::set_var("HERD_AGENT_TOKEN", "self-test-token");

    // Reserve an ephemeral port for the gateway.
    let probe_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = probe_listener.local_addr().unwrap().port();
    drop(probe_listener);

    let mut config = herd::config::Config::default();
    config.server.host = "127.0.0.1".into();
    config.server.port = port;
    tokio::spawn(async move {
        let _ = herd::server::run(config, None).await;
    });

    // Wait for the gateway to accept connections.
    let gateway = format!("http://127.0.0.1:{port}");
    let http = reqwest::Client::new();
    let mut up = false;
    for _ in 0..50 {
        if http.get(&gateway).send().await.is_ok() {
            up = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(up, "gateway did not come up on {gateway}");

    // Run the real daemon loop against it for a few beats.
    let args = herd::daemon::AgentArgs {
        gateway: gateway.clone(),
        node_id: Some("self-test-daemon".into()),
        heartbeat_secs: 1,
        backend_url: "http://127.0.0.1:1".into(), // no local backend in this test
        advertise_url: Some("http://127.0.0.1:8080".into()),
        backend: Some(BackendType::LlamaServer),
        respawn_mode: herd::config::RespawnMode::SelfSpawn,
    };
    tokio::spawn(herd::daemon::run(args));
    tokio::time::sleep(Duration::from_millis(2500)).await;

    // The daemon must have registered the node: a manual heartbeat with the
    // same node_id now reports registered=false (known node).
    let client = HeartbeatClient::new(&gateway, Some("self-test-token".into())).unwrap();
    let outcome = client.send(&test_snapshot("self-test-daemon"), false).await;
    assert!(
        matches!(
            outcome,
            BeatOutcome::Success {
                registered: false,
                ..
            }
        ),
        "daemon should already have registered this node_id, got {outcome:?}"
    );
}
