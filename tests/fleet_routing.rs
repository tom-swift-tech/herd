//! In-process integration test for the v1.2 fleet routing chain (PR #8).
//!
//! Unit tests cover the pieces in isolation (heartbeat handler, registry TTL,
//! `AgentPoolSync::reconcile`, `ModelAwareRouter`). This test proves the WHOLE
//! chain end-to-end in one process, over real HTTP:
//!
//! ```text
//!   client → gateway /v1/chat/completions
//!                        │  router → BackendPool (agent: entry)
//!                        ▼
//!   agent heartbeat → NodeRegistry → AgentPoolSync::reconcile → BackendPool
//!                        ▼
//!                   stub upstream (the agent's llama-server)
//! ```
//!
//! The "gateway" is the two public axum handlers (`heartbeat`,
//! `chat_completions`) mounted on a bare router with a hand-built `AppState`
//! (in-memory `NodeDb`, ephemeral port) — NOT `server::run`, so it never binds a
//! privileged port or touches the user's `~/.herd` SQLite/analytics. The full
//! `server::run` two-box path stays the `#[ignore]`d self-test in
//! `agent_daemon.rs`; the network-reachability proof across two machines is
//! documented as manual acceptance in the PR.
//!
//! Determinism: freshness/TTL is driven by an injected monotonic clock
//! ([`TestClock`]) that the test advances explicitly, and reconciles are driven
//! by an explicit `AgentPoolSync::reconcile` call — never `sleep` + margin, so
//! the test cannot flake under CI load.

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use herd::api::internal::heartbeat;
use herd::api::openai::chat_completions;
use herd::config::{BackendType, Config, RoutingStrategy};
use herd::nodes::registry::Clock;
use herd::nodes::{AgentCapabilities, AgentPoolSync, NodeDb, NodeRegistry};
use herd::server::AppState;
use herd::{
    agent::{AgentAudit, SessionStore},
    analytics::Analytics,
    backend::BackendPool,
    budget::BudgetTracker,
    classifier_auto::ClassificationCache,
    metrics::Metrics,
    providers::{cost_db::CostDb, rate_limit::ProviderRateLimiter},
    rate_limit::RateLimiter,
    router::create_router,
};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU32, AtomicU64};
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};

/// Registry TTL used throughout: an agent past this age leaves `fresh_nodes()`.
const TTL: Duration = Duration::from_secs(30);

// -----------------------------------------------------------------------------
// Auth: the heartbeat handler reads `HERD_AGENT_TOKEN` from the environment and
// rejects unauthenticated beats by default. These tests opt into the
// unauthenticated mode. The env var is set exactly once, before any test issues
// an HTTP call (`Once` happens-before guarantees the single write is visible to
// every handler thread), so there is no set/read race across the parallel tests
// in this binary.
// -----------------------------------------------------------------------------

static INIT_ENV: Once = Once::new();

fn allow_unauthenticated_heartbeats() {
    INIT_ENV.call_once(|| {
        std::env::set_var("HERD_ALLOW_UNAUTHENTICATED_AGENT_HEARTBEAT", "true");
    });
}

// -----------------------------------------------------------------------------
// Controllable clock: a local mirror of the registry's in-crate `test_clock`
// helper (which is `#[cfg(test)]` and therefore invisible to this external test
// crate). Backed by a shared `Instant` we advance by hand.
// -----------------------------------------------------------------------------

#[derive(Clone)]
struct TestClock {
    now: Arc<Mutex<Instant>>,
}

impl TestClock {
    fn new() -> Self {
        Self {
            now: Arc::new(Mutex::new(Instant::now())),
        }
    }

    fn advance(&self, delta: Duration) {
        *self.now.lock().unwrap() += delta;
    }

    fn as_clock(&self) -> Clock {
        let now = self.now.clone();
        Arc::new(move || *now.lock().unwrap())
    }
}

// -----------------------------------------------------------------------------
// Stub upstream: a tiny axum server standing in for the agent's llama-server.
// It records every `/v1/chat/completions` body it receives so the test can
// assert the request was actually routed here (anti-trivial: a test that 200s
// regardless of routing would not see a recorded hit).
// -----------------------------------------------------------------------------

#[derive(Clone, Default)]
struct StubUpstream {
    received: Arc<Mutex<Vec<Value>>>,
}

impl StubUpstream {
    fn hits(&self) -> usize {
        self.received.lock().unwrap().len()
    }

    fn last_model(&self) -> Option<String> {
        self.received
            .lock()
            .unwrap()
            .last()
            .and_then(|b| b.get("model").and_then(|m| m.as_str()).map(String::from))
    }
}

async fn stub_chat(State(stub): State<StubUpstream>, body: axum::body::Bytes) -> Json<Value> {
    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    stub.received.lock().unwrap().push(parsed);
    Json(json!({
        "id": "chatcmpl-stub",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "pong"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    }))
}

/// Spawn a stub upstream on an ephemeral port. Returns the handle (for hit
/// assertions) and its base URL (advertised by the agent heartbeat).
async fn spawn_upstream() -> (StubUpstream, String) {
    let stub = StubUpstream::default();
    let app = Router::new()
        .route("/v1/chat/completions", post(stub_chat))
        .with_state(stub.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (stub, format!("http://{addr}"))
}

// -----------------------------------------------------------------------------
// Gateway: the real public handlers over a hand-built AppState. The registry
// and pool are shared back to the test so it can drive reconciles and advance
// the clock deterministically.
// -----------------------------------------------------------------------------

struct Gateway {
    url: String,
    registry: Arc<NodeRegistry>,
    pool: Arc<BackendPool>,
}

fn build_state(registry: Arc<NodeRegistry>, pool: Arc<BackendPool>, config: Config) -> AppState {
    let router = create_router(config.routing.strategy.clone(), (*pool).clone());
    AppState {
        pool,
        router: Arc::new(tokio::sync::RwLock::new(router)),
        client: Arc::new(reqwest::Client::new()),
        mgmt_client: Arc::new(reqwest::Client::new()),
        config: Arc::new(tokio::sync::RwLock::new(config.clone())),
        analytics: Arc::new(Analytics::new().unwrap()),
        metrics: Arc::new(Metrics::new()),
        session_store: Arc::new(SessionStore::new(10)),
        agent_audit: Arc::new(AgentAudit::new().unwrap()),
        // In-memory so the test never touches the operator's ~/.herd node DB.
        node_db: Arc::new(NodeDb::open_in_memory().unwrap()),
        node_registry: registry,
        binary_store: Arc::new(herd::nodes::BinaryStore::new()),
        budget: BudgetTracker::new(config.budget.clone()),
        rate_limiter: Arc::new(tokio::sync::RwLock::new(RateLimiter::new(
            &config.rate_limiting,
        ))),
        frontier_rate_limiter: Arc::new(tokio::sync::RwLock::new(ProviderRateLimiter::new(
            &config.providers,
        ))),
        auto_cache: Arc::new(ClassificationCache::new(10)),
        cost_db: Arc::new(CostDb::new(Connection::open_in_memory().unwrap())),
        routing_timeout_ms: Arc::new(AtomicU64::new(2_000)),
        // Single attempt: a happy-path success must route to the stub exactly
        // once (so `hits == 1` is exact), and the drain case must 503 without
        // retrying against a backend that is no longer there.
        routing_retry_count: Arc::new(AtomicU32::new(0)),
        config_path: None,
    }
}

/// Stand up an in-process gateway with an empty pool and the given registry.
async fn spawn_gateway(registry: Arc<NodeRegistry>) -> Gateway {
    let pool = Arc::new(BackendPool::new(vec![], 3, TTL));
    let mut config = Config::default();
    config.routing.strategy = RoutingStrategy::ModelAware;

    let state = build_state(Arc::clone(&registry), Arc::clone(&pool), config);
    let app = Router::new()
        .route("/api/internal/nodes/heartbeat", post(heartbeat))
        .route("/v1/chat/completions", post(chat_completions))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    Gateway {
        url: format!("http://{addr}"),
        registry,
        pool,
    }
}

// -----------------------------------------------------------------------------
// Helpers for issuing the two HTTP calls the chain is built from.
// -----------------------------------------------------------------------------

fn caps(node_id: &str, address: &str, models: &[&str]) -> AgentCapabilities {
    AgentCapabilities {
        node_id: node_id.to_string(),
        backend: BackendType::LlamaServer,
        address: address.to_string(),
        gpu_model: Some("RTX 5090".to_string()),
        vram_total_mb: 32_768,
        vram_free_mb: 30_000,
        models_loaded: models.iter().map(|m| m.to_string()).collect(),
        queue_depth: 0,
        ttft_p50_ms: Some(42),
        rpc_capable: false,
        rpc_port: None,
        agent_version: "1.2.0".to_string(),
        os: Some(std::env::consts::OS.to_string()),
        arch: Some(std::env::consts::ARCH.to_string()),
    }
}

/// POST a heartbeat to the gateway; assert it was accepted (registration HTTP
/// path is healthy) before the test relies on the node being known.
async fn send_heartbeat(http: &reqwest::Client, gw: &Gateway, caps: &AgentCapabilities) {
    let resp = http
        .post(format!("{}/api/internal/nodes/heartbeat", gw.url))
        .json(&json!({ "capabilities": caps }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::OK,
        "heartbeat for {} must be accepted",
        caps.node_id
    );
}

/// POST a chat request for `model` to the gateway and return the HTTP response.
async fn send_chat(http: &reqwest::Client, gw: &Gateway, model: &str) -> reqwest::Response {
    http.post(format!("{}/v1/chat/completions", gw.url))
        .json(&json!({
            "model": model,
            "messages": [{"role": "user", "content": "ping"}]
        }))
        .send()
        .await
        .unwrap()
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

/// Happy path: heartbeat → reconcile → a chat request to the gateway routes
/// through to the agent's stub upstream, 200s, and the stub actually receives
/// the request (proving the route landed, not a blanket 200).
#[tokio::test]
async fn heartbeat_then_chat_routes_through_to_agent_upstream() {
    allow_unauthenticated_heartbeats();
    let http = reqwest::Client::new();

    let (stub, stub_url) = spawn_upstream().await;
    let registry = Arc::new(NodeRegistry::new(TTL));
    let gw = spawn_gateway(Arc::clone(&registry)).await;

    // Agent announces itself, loaded with `qwen3-32b`, reachable at the stub.
    send_heartbeat(&http, &gw, &caps("citadel-5090", &stub_url, &["qwen3-32b"])).await;

    // One reconcile mirrors the fresh agent into the pool as `agent:citadel-5090`.
    AgentPoolSync::reconcile(&gw.registry, &gw.pool).await;
    assert!(
        gw.pool
            .all()
            .await
            .contains(&"agent:citadel-5090".to_string()),
        "reconcile must add the agent to the pool"
    );

    // The assertion nothing makes today: a request to the GATEWAY round-trips
    // through the router and proxy to the agent's upstream.
    let resp = send_chat(&http, &gw, "qwen3-32b").await;
    assert_eq!(resp.status(), reqwest::StatusCode::OK, "gateway must 200");

    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["choices"][0]["message"]["content"], "pong",
        "the stub upstream's response must round-trip back through the gateway"
    );

    // Anti-trivial: the stub must have actually been hit, exactly once.
    assert_eq!(
        stub.hits(),
        1,
        "the routed request must reach the stub upstream"
    );
    assert_eq!(
        stub.last_model().as_deref(),
        Some("qwen3-32b"),
        "the forwarded request must carry the routed model"
    );
}

/// Drain → 503 through HTTP: once the agent goes stale (clock advanced past the
/// TTL, NOT slept) and is reconciled out, the pool is empty and a chat request
/// to the gateway returns a real 503 — no hidden fallback.
#[tokio::test]
async fn stale_agent_drains_and_gateway_returns_503() {
    allow_unauthenticated_heartbeats();
    let http = reqwest::Client::new();

    let (stub, stub_url) = spawn_upstream().await;
    let clock = TestClock::new();
    let registry = Arc::new(NodeRegistry::with_clock(TTL, clock.as_clock()));
    let gw = spawn_gateway(Arc::clone(&registry)).await;

    send_heartbeat(&http, &gw, &caps("citadel-5090", &stub_url, &["qwen3-32b"])).await;
    AgentPoolSync::reconcile(&gw.registry, &gw.pool).await;

    // Sanity: while fresh, the chain works and the stub is hit.
    let resp = send_chat(&http, &gw, "qwen3-32b").await;
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert_eq!(stub.hits(), 1, "fresh agent must serve the request");

    // Advance past the TTL so the agent leaves `fresh_nodes()`, then reconcile:
    // the `agent:` entry is removed and the pool empties.
    clock.advance(TTL + Duration::from_secs(1));
    AgentPoolSync::reconcile(&gw.registry, &gw.pool).await;
    assert!(
        gw.pool.all().await.is_empty(),
        "stale agent must be drained from the pool"
    );

    // The gateway now has nothing to route to → a real 503, and the (gone)
    // upstream is NOT hit again.
    let resp = send_chat(&http, &gw, "qwen3-32b").await;
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::SERVICE_UNAVAILABLE,
        "drained pool must surface as a 503 through HTTP"
    );
    assert_eq!(
        stub.hits(),
        1,
        "no request may reach a drained agent (no hidden fallback)"
    );
}

/// Model routing: two agents advertising different models. A request for one
/// model must land on exactly the agent that has it.
#[tokio::test]
async fn request_routes_to_the_agent_holding_the_model() {
    allow_unauthenticated_heartbeats();
    let http = reqwest::Client::new();

    let (stub_x, url_x) = spawn_upstream().await;
    let (stub_y, url_y) = spawn_upstream().await;
    let registry = Arc::new(NodeRegistry::new(TTL));
    let gw = spawn_gateway(Arc::clone(&registry)).await;

    send_heartbeat(&http, &gw, &caps("node-x", &url_x, &["model-x"])).await;
    send_heartbeat(&http, &gw, &caps("node-y", &url_y, &["model-y"])).await;
    AgentPoolSync::reconcile(&gw.registry, &gw.pool).await;

    // Request for model-x → must hit node-x, never node-y.
    let resp = send_chat(&http, &gw, "model-x").await;
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert_eq!(stub_x.hits(), 1, "model-x request must reach node-x");
    assert_eq!(stub_y.hits(), 0, "model-x request must NOT reach node-y");
    assert_eq!(stub_x.last_model().as_deref(), Some("model-x"));

    // Request for model-y → must hit node-y, leaving node-x untouched.
    let resp = send_chat(&http, &gw, "model-y").await;
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert_eq!(stub_y.hits(), 1, "model-y request must reach node-y");
    assert_eq!(stub_x.hits(), 1, "model-y request must NOT reach node-x");
    assert_eq!(stub_y.last_model().as_deref(), Some("model-y"));
}
