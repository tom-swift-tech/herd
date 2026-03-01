use crate::api::{admin, metrics, status};
use crate::backend::{BackendPool, HealthChecker, ModelDiscovery};
use crate::config::Config;
use anyhow::Result;
use axum::{
    extract::{Request, State},
    http::StatusCode,
    routing::{get, post},
};
use std::sync::Arc;
use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;
use tracing::info;

pub async fn run(config: Config) -> Result<()> {
    let addr = format!("{}:{}", config.server.host, config.server.port);
    info!("Starting Herd on {}", addr);

    // Create backend pool
    let pool = BackendPool::new(config.backends.clone());

    // Start health checker
    let health_checker = HealthChecker::new(10);
    health_checker.spawn(pool.clone()).await;

    // Start model discovery
    let discovery = ModelDiscovery::new(300);
    discovery.spawn(pool.clone()).await;

    let shared_pool = Arc::new(pool);

    // Build app — uses Arc<BackendPool> as state for all handlers
    let app = axum::Router::new()
        .route("/status", get(status::get_status))
        .route("/metrics", get(metrics::get_metrics))
        .route("/health", get(health_check))
        .route("/admin/backends", post(admin::add_backend))
        .route("/admin/backends/remove", post(admin::remove_backend))
        .route("/admin/backends/drain", post(admin::drain_backend))
        .fallback(proxy_handler)
        .layer(ServiceBuilder::new().layer(TraceLayer::new_for_http()))
        .with_state(shared_pool);

    // Start server
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

/// Proxy all unmatched requests to the highest priority healthy backend.
async fn proxy_handler(
    State(pool): State<Arc<BackendPool>>,
    request: Request,
) -> Result<axum::response::Response, StatusCode> {
    // Pick best backend (model-aware: check body for model field)
    let backend = pool
        .get_by_priority()
        .await
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    let backend_url = backend.config.url.clone();
    let uri = request.uri().clone();
    let method = request.method().clone();
    let path = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/");

    // Read body
    let body_bytes = axum::body::to_bytes(request.into_body(), 10 * 1024 * 1024) // 10MB limit
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?;

    // Build reqwest request
    let client = reqwest::Client::new();
    let target_url = format!("{}{}", backend_url, path);

    let reqwest_method = match method.as_str() {
        "GET" => reqwest::Method::GET,
        "POST" => reqwest::Method::POST,
        "PUT" => reqwest::Method::PUT,
        "DELETE" => reqwest::Method::DELETE,
        "PATCH" => reqwest::Method::PATCH,
        "HEAD" => reqwest::Method::HEAD,
        "OPTIONS" => reqwest::Method::OPTIONS,
        _ => reqwest::Method::GET,
    };

    let response = client
        .request(reqwest_method, &target_url)
        .body(body_bytes)
        .send()
        .await
        .map_err(|e| {
            tracing::error!("Backend request to {} failed: {}", target_url, e);
            StatusCode::BAD_GATEWAY
        })?;

    // Convert reqwest response back to axum response
    let status = axum::http::StatusCode::from_u16(response.status().as_u16())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

    let resp_bytes = response.bytes().await.map_err(|_| StatusCode::BAD_GATEWAY)?;

    Ok(axum::response::Response::builder()
        .status(status)
        .body(axum::body::Body::from(resp_bytes))
        .unwrap())
}

async fn health_check() -> &'static str {
    "OK"
}