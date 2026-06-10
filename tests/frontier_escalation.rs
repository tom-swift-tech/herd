//! Sprint 3 integration tests for auto-mode -> frontier escalation.
//!
//! Exercises `herd::providers::frontier_route_if_applicable` decision logic
//! and the full proxy path using a minimal in-process HTTP listener as the
//! mock frontier provider.

use axum::http::HeaderMap;
use herd::api::openai::list_models;
use herd::classifier_auto::Classification;
use herd::config::{FrontierConfig, ProviderConfig};
use herd::providers::{
    cost_db::CostDb, frontier_route_if_applicable, rate_limit::ProviderRateLimiter,
};
use herd::server::AppState;
use herd::{
    agent::{AgentAudit, SessionStore},
    analytics::Analytics,
    backend::BackendPool,
    budget::BudgetTracker,
    classifier_auto::ClassificationCache,
    config::{Backend, Config},
    metrics::Metrics,
    nodes::{NodeDb, NodeRegistry},
    rate_limit::RateLimiter,
    router::create_router,
};
use rusqlite::Connection;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// -----------------------------------------------------------------------------
// Mock provider: accepts one request, returns a canned 200 JSON response.
// -----------------------------------------------------------------------------

async fn spawn_mock_provider(response_body: &'static str) -> (String, Arc<tokio::sync::Notify>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let notify = Arc::new(tokio::sync::Notify::new());
    let notify_clone = notify.clone();

    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let note = notify_clone.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let _ = socket.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    response_body.len(),
                    response_body
                );
                let _ = socket.write_all(resp.as_bytes()).await;
                let _ = socket.shutdown().await;
                note.notify_one();
            });
        }
    });

    (format!("http://{}", addr), notify)
}

fn provider(name: &str, url: &str, model: &str) -> ProviderConfig {
    ProviderConfig {
        name: name.to_string(),
        api_url: url.to_string(),
        api_key_env: format!("TEST_{}_KEY", name.to_uppercase()),
        models: vec![model.to_string()],
        priority: 100,
        monthly_budget: 100.0,
        ..Default::default()
    }
}

fn in_memory_cost_db() -> CostDb {
    let conn = Connection::open_in_memory().unwrap();
    CostDb::new(conn)
}

fn unlimited_rate_limiter() -> ProviderRateLimiter {
    // No providers registered => try_acquire always returns true (rate-limiting disabled).
    ProviderRateLimiter::new(&[])
}

fn frontier_tier() -> Classification {
    Classification {
        tier: "frontier".to_string(),
        capability: "reasoning".to_string(),
        needs_large_context: false,
        language: "en".to_string(),
    }
}

fn test_state(config: Config) -> AppState {
    let pool = BackendPool::new(
        vec![Backend {
            name: "gpu-1".into(),
            url: "http://127.0.0.1:11434".into(),
            priority: 100,
            ..Default::default()
        }],
        config.circuit_breaker.failure_threshold,
        std::time::Duration::from_secs(30),
    );
    let router = create_router(config.routing.strategy.clone(), pool.clone());

    AppState {
        pool: Arc::new(pool),
        router: Arc::new(tokio::sync::RwLock::new(router)),
        client: Arc::new(reqwest::Client::new()),
        mgmt_client: Arc::new(reqwest::Client::new()),
        config: Arc::new(tokio::sync::RwLock::new(config.clone())),
        analytics: Arc::new(Analytics::new().unwrap()),
        metrics: Arc::new(Metrics::new()),
        session_store: Arc::new(SessionStore::new(10)),
        agent_audit: Arc::new(AgentAudit::new().unwrap()),
        node_db: Arc::new(NodeDb::open().unwrap()),
        node_registry: Arc::new(NodeRegistry::new(std::time::Duration::from_secs(30))),
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
        routing_timeout_ms: Arc::new(std::sync::atomic::AtomicU64::new(1_000)),
        routing_retry_count: Arc::new(std::sync::atomic::AtomicU32::new(1)),
        config_path: None,
    }
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[tokio::test]
async fn returns_none_when_frontier_disabled() {
    let client = reqwest::Client::new();
    let providers = vec![provider(
        "anthropic",
        "https://api.anthropic.com",
        "claude-sonnet-4",
    )];
    let cfg = FrontierConfig {
        enabled: false,
        ..Default::default()
    };
    let cost_db = in_memory_cost_db();
    let rate_limiter = unlimited_rate_limiter();
    let headers = HeaderMap::new();

    let result = frontier_route_if_applicable(
        &client,
        &cfg,
        &providers,
        &cost_db,
        &rate_limiter,
        Some("claude-sonnet-4"),
        &headers,
        Some(&frontier_tier()),
        b"{}",
        "req-1",
    )
    .await;

    assert!(
        result.is_none(),
        "helper must return None when frontier disabled"
    );
}

#[tokio::test]
async fn returns_none_for_non_frontier_model() {
    let client = reqwest::Client::new();
    let providers = vec![provider(
        "anthropic",
        "https://api.anthropic.com",
        "claude-sonnet-4",
    )];
    let cfg = FrontierConfig {
        enabled: true,
        ..Default::default()
    };
    let cost_db = in_memory_cost_db();
    let rate_limiter = unlimited_rate_limiter();
    let headers = HeaderMap::new();

    let result = frontier_route_if_applicable(
        &client,
        &cfg,
        &providers,
        &cost_db,
        &rate_limiter,
        Some("qwen3:8b"),
        &headers,
        None,
        b"{}",
        "req-1",
    )
    .await;

    assert!(
        result.is_none(),
        "helper must return None for local model names"
    );
}

#[tokio::test]
async fn blocks_auto_escalation_when_flag_disabled() {
    // Classifier said frontier, but allow_auto_escalation=false.
    // Helper must return None so the caller can fall back to fallback_model,
    // preventing an unintended cloud request.
    let client = reqwest::Client::new();
    let providers = vec![provider(
        "anthropic",
        "https://api.anthropic.com",
        "claude-sonnet-4",
    )];
    let cfg = FrontierConfig {
        enabled: true,
        allow_auto_escalation: false,
        require_header: false,
        ..Default::default()
    };
    let cost_db = in_memory_cost_db();
    let rate_limiter = unlimited_rate_limiter();
    let headers = HeaderMap::new();

    let result = frontier_route_if_applicable(
        &client,
        &cfg,
        &providers,
        &cost_db,
        &rate_limiter,
        Some("claude-sonnet-4"),
        &headers,
        Some(&frontier_tier()),
        b"{}",
        "req-1",
    )
    .await;

    assert!(
        result.is_none(),
        "auto-classified frontier tier with allow_auto_escalation=false must return None"
    );
}

#[tokio::test]
async fn rejects_with_403_when_header_required_and_missing() {
    let client = reqwest::Client::new();
    let providers = vec![provider(
        "anthropic",
        "https://api.anthropic.com",
        "claude-sonnet-4",
    )];
    let cfg = FrontierConfig {
        enabled: true,
        require_header: true,
        allow_auto_escalation: false,
        ..Default::default()
    };
    let cost_db = in_memory_cost_db();
    let rate_limiter = unlimited_rate_limiter();
    let headers = HeaderMap::new();

    // Explicit model (no auto classification), require_header=true, no header.
    let result = frontier_route_if_applicable(
        &client,
        &cfg,
        &providers,
        &cost_db,
        &rate_limiter,
        Some("claude-sonnet-4"),
        &headers,
        None,
        b"{}",
        "req-1",
    )
    .await;

    let response = result.expect("helper must return a 403 response");
    assert_eq!(
        response.status(),
        axum::http::StatusCode::FORBIDDEN,
        "missing X-Herd-Frontier header must yield 403"
    );
}

#[tokio::test]
async fn auto_escalation_bypasses_header_requirement() {
    // Classifier returned frontier + allow_auto_escalation=true + require_header=true.
    // The escalation should bypass the header check and hit the mock provider.
    let mock_response = r#"{"id":"chatcmpl-x","choices":[{"message":{"role":"assistant","content":"ok"}}],"usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}"#;
    let (mock_url, _notify) = spawn_mock_provider(mock_response).await;

    std::env::set_var("TEST_MOCK_KEY", "sk-test");

    let client = reqwest::Client::new();
    let providers = vec![ProviderConfig {
        name: "mock".to_string(),
        api_url: mock_url,
        api_key_env: "TEST_MOCK_KEY".to_string(),
        models: vec!["mock-frontier-model".to_string()],
        priority: 100,
        monthly_budget: 100.0,
        ..Default::default()
    }];
    let cfg = FrontierConfig {
        enabled: true,
        require_header: true,
        allow_auto_escalation: true,
        ..Default::default()
    };
    let cost_db = in_memory_cost_db();
    let rate_limiter = unlimited_rate_limiter();
    let headers = HeaderMap::new();

    let result = frontier_route_if_applicable(
        &client,
        &cfg,
        &providers,
        &cost_db,
        &rate_limiter,
        Some("mock-frontier-model"),
        &headers,
        Some(&frontier_tier()),
        br#"{"messages":[{"role":"user","content":"hi"}]}"#,
        "req-1",
    )
    .await;

    let response = result.expect("helper must return a response");
    assert_eq!(
        response.status(),
        axum::http::StatusCode::OK,
        "auto-escalation should bypass require_header and reach the provider"
    );

    let provider_header = response
        .headers()
        .get("x-herd-provider")
        .expect("response must include X-Herd-Provider");
    assert_eq!(provider_header.to_str().unwrap(), "mock");

    let tier_header = response
        .headers()
        .get("x-herd-auto-tier")
        .expect("auto-escalation must emit X-Herd-Auto-Tier");
    assert_eq!(tier_header.to_str().unwrap(), "frontier");
}

#[tokio::test]
async fn explicit_header_allows_direct_frontier_call() {
    // User sent X-Herd-Frontier: true header with an explicit frontier model.
    // No auto classification, allow_auto_escalation irrelevant.
    let mock_response = r#"{"id":"chatcmpl-y","choices":[],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;
    let (mock_url, _notify) = spawn_mock_provider(mock_response).await;

    std::env::set_var("TEST_MOCK2_KEY", "sk-test");

    let client = reqwest::Client::new();
    let providers = vec![ProviderConfig {
        name: "mock2".to_string(),
        api_url: mock_url,
        api_key_env: "TEST_MOCK2_KEY".to_string(),
        models: vec!["mock-explicit-model".to_string()],
        priority: 100,
        monthly_budget: 100.0,
        ..Default::default()
    }];
    let cfg = FrontierConfig {
        enabled: true,
        require_header: true,
        allow_auto_escalation: false,
        ..Default::default()
    };
    let cost_db = in_memory_cost_db();
    let rate_limiter = unlimited_rate_limiter();
    let mut headers = HeaderMap::new();
    headers.insert("x-herd-frontier", "true".parse().unwrap());

    let result = frontier_route_if_applicable(
        &client,
        &cfg,
        &providers,
        &cost_db,
        &rate_limiter,
        Some("mock-explicit-model"),
        &headers,
        None,
        br#"{"messages":[{"role":"user","content":"hi"}]}"#,
        "req-1",
    )
    .await;

    let response = result.expect("helper must return a response");
    assert_eq!(
        response.status(),
        axum::http::StatusCode::OK,
        "explicit X-Herd-Frontier: true header must allow direct call"
    );
}

// -----------------------------------------------------------------------------
// v1.1.2 — Per-provider rate limiting and cost recording
// -----------------------------------------------------------------------------

#[tokio::test]
async fn rate_limit_returns_429_when_exhausted() {
    // Build a limiter with rate_limit=1 so the second request trips the cap.
    let providers = vec![ProviderConfig {
        name: "ratelimited".to_string(),
        api_url: "https://api.example.com".to_string(),
        api_key_env: "TEST_RATELIMITED_KEY".to_string(),
        models: vec!["rl-model".to_string()],
        priority: 100,
        rate_limit: 1,
        ..Default::default()
    }];
    let limiter = ProviderRateLimiter::new(&providers);

    // First acquire consumes the only token.
    assert!(limiter.try_acquire("ratelimited"));

    // Now the helper call should get blocked with 429.
    let client = reqwest::Client::new();
    let cfg = FrontierConfig {
        enabled: true,
        require_header: false,
        ..Default::default()
    };
    let cost_db = in_memory_cost_db();
    let headers = HeaderMap::new();

    let result = frontier_route_if_applicable(
        &client,
        &cfg,
        &providers,
        &cost_db,
        &limiter,
        Some("rl-model"),
        &headers,
        None,
        b"{}",
        "req-rl-1",
    )
    .await;

    let response = result.expect("helper must return a 429 response");
    assert_eq!(
        response.status(),
        axum::http::StatusCode::TOO_MANY_REQUESTS,
        "exceeded rate limit must yield 429"
    );
}

#[tokio::test]
async fn non_streaming_response_records_cost_and_emits_header() {
    // Canned response with usage fields that billing::record_frontier_cost
    // can extract + price. gpt-4.1 has a built-in rate of $2 in / $8 out per Mtok.
    let mock_response = r#"{"id":"chatcmpl-a","choices":[{"message":{"role":"assistant","content":"ok"}}],"usage":{"prompt_tokens":1000000,"completion_tokens":500000,"total_tokens":1500000}}"#;
    let (mock_url, _notify) = spawn_mock_provider(mock_response).await;

    std::env::set_var("TEST_BILLING_KEY", "sk-test");

    let providers = vec![ProviderConfig {
        name: "billing".to_string(),
        api_url: mock_url,
        api_key_env: "TEST_BILLING_KEY".to_string(),
        models: vec!["gpt-4.1".to_string()],
        priority: 100,
        ..Default::default()
    }];
    let limiter = ProviderRateLimiter::new(&providers);
    let cfg = FrontierConfig {
        enabled: true,
        require_header: false,
        ..Default::default()
    };
    let cost_db = in_memory_cost_db();
    let client = reqwest::Client::new();
    let headers = HeaderMap::new();

    let result = frontier_route_if_applicable(
        &client,
        &cfg,
        &providers,
        &cost_db,
        &limiter,
        Some("gpt-4.1"),
        &headers,
        None,
        // Request body with stream omitted (defaults to non-streaming)
        br#"{"messages":[{"role":"user","content":"hi"}]}"#,
        "req-billing-1",
    )
    .await;

    let response = result.expect("helper must return a response");
    assert_eq!(response.status(), axum::http::StatusCode::OK);

    let cost_header = response
        .headers()
        .get("x-herd-cost-estimate")
        .expect("non-streaming response must emit X-Herd-Cost-Estimate");
    let cost: f32 = cost_header.to_str().unwrap().parse().unwrap();
    // 1M prompt * $2 + 0.5M completion * $8 = $6.00
    assert!(
        (cost - 6.0).abs() < 0.01,
        "expected cost ~6.00 USD, got {}",
        cost
    );

    // Cost row was actually persisted.
    let summary = cost_db.cost_summary().expect("cost summary query");
    let billing_row = summary
        .iter()
        .find(|r| r.provider == "billing")
        .expect("billing provider should have a row");
    assert_eq!(billing_row.total_tokens_in, 1_000_000);
    assert_eq!(billing_row.total_tokens_out, 500_000);
    assert_eq!(billing_row.request_count, 1);
}

#[tokio::test]
async fn streaming_response_passes_through_without_cost_recording() {
    // When the client requests stream=true, we don't buffer the response and
    // therefore skip cost recording. The response must still flow through.
    let mock_response =
        r#"{"id":"chatcmpl-s","choices":[{"message":{"role":"assistant","content":"ok"}}]}"#;
    let (mock_url, _notify) = spawn_mock_provider(mock_response).await;

    std::env::set_var("TEST_STREAM_KEY", "sk-test");

    let providers = vec![ProviderConfig {
        name: "streaming".to_string(),
        api_url: mock_url,
        api_key_env: "TEST_STREAM_KEY".to_string(),
        models: vec!["gpt-4.1".to_string()],
        priority: 100,
        ..Default::default()
    }];
    let limiter = ProviderRateLimiter::new(&providers);
    let cfg = FrontierConfig {
        enabled: true,
        require_header: false,
        ..Default::default()
    };
    let cost_db = in_memory_cost_db();
    let client = reqwest::Client::new();
    let headers = HeaderMap::new();

    let result = frontier_route_if_applicable(
        &client,
        &cfg,
        &providers,
        &cost_db,
        &limiter,
        Some("gpt-4.1"),
        &headers,
        None,
        br#"{"messages":[{"role":"user","content":"hi"}],"stream":true}"#,
        "req-stream-1",
    )
    .await;

    let response = result.expect("helper must return a response");
    assert_eq!(response.status(), axum::http::StatusCode::OK);

    // Streaming path explicitly does not emit X-Herd-Cost-Estimate.
    assert!(
        response.headers().get("x-herd-cost-estimate").is_none(),
        "streaming response must not emit X-Herd-Cost-Estimate"
    );
}

#[tokio::test]
async fn list_models_omits_frontier_provider_without_api_key() {
    std::env::remove_var("TEST_MISSING_FRONTIER_KEY");

    let state = test_state(Config {
        frontier: FrontierConfig {
            enabled: true,
            ..Default::default()
        },
        providers: vec![ProviderConfig {
            name: "missing".into(),
            api_url: "https://api.example.com".into(),
            api_key_env: "TEST_MISSING_FRONTIER_KEY".into(),
            models: vec!["gpt-missing".into()],
            ..Default::default()
        }],
        ..Default::default()
    });

    let axum::Json(body) = list_models(axum::extract::State(state)).await;
    let data = body["data"].as_array().unwrap();

    assert!(
        !data.iter().any(|item| item["id"] == "gpt-missing"),
        "models without a configured provider API key should not be advertised"
    );
}
