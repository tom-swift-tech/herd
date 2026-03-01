use crate::backend::{BackendPool, HealthChecker, ModelDiscovery};
use crate::config::Config;
use crate::router::{create_router, Router};
use anyhow::Result;
use std::sync::Arc;
use tower_http::trace::TraceLayer;
use tracing::info;

pub struct Server {
    config: Config,
}

#[derive(Clone)]
pub struct AppState {
    pub pool: Arc<BackendPool>,
    pub router: crate::router::RouterEnum,
    pub client: Arc<reqwest::Client>,
}

impl Server {
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    pub async fn run(self) -> Result<()> {
        let addr = format!("{}:{}", self.config.server.host, self.config.server.port);
        info!("Starting Herd on {}", addr);

        // Create backend pool
        let pool = BackendPool::new(self.config.backends.clone());

        // Start health checker
        let health_checker = HealthChecker::new(10);
        health_checker.spawn(pool.clone());

        // Start model discovery
        let discovery = ModelDiscovery::new(300);
        discovery.spawn(pool.clone());

        // Create router
        let router = create_router(self.config.routing.strategy.clone(), pool.clone());

        // Wrap in Arc
        let pool = Arc::new(pool);
        let client = Arc::new(reqwest::Client::new());

        let state = AppState {
            pool: Arc::clone(&pool),
            router,
            client: Arc::clone(&client),
        };

        // Build app with routes
        let app = axum::Router::new()
            .route("/health", axum::routing::get(|| async { "OK" }))
            .route("/status", axum::routing::get(status_handler))
            .route("/metrics", axum::routing::get(metrics_handler))
            .fallback(proxy_handler)
            .layer(tower::ServiceBuilder::new().layer(TraceLayer::new_for_http()))
            .with_state(state);

        // Start server
        let listener = tokio::net::TcpListener::bind(&addr).await?;
        axum::serve(listener, app).await?;

        Ok(())
    }
}

async fn proxy_handler(
    axum::extract::State(state): axum::extract::State<AppState>,
    request: axum::extract::Request,
) -> Result<String, axum::http::StatusCode> {
    // Route request
    let backend_url = state
        .router
        .route(None)
        .await
        .map_err(|_| axum::http::StatusCode::SERVICE_UNAVAILABLE)?;

    let url = format!("{}{}", backend_url, request.uri().path());
    
    // Get method from original request
    let method = match *request.method() {
        axum::http::Method::GET => reqwest::Method::GET,
        axum::http::Method::POST => reqwest::Method::POST,
        axum::http::Method::PUT => reqwest::Method::PUT,
        axum::http::Method::DELETE => reqwest::Method::DELETE,
        _ => reqwest::Method::POST,
    };
    
    let body_bytes = axum::body::to_bytes(request.into_body(), usize::MAX)
        .await
        .map_err(|_| axum::http::StatusCode::BAD_REQUEST)?;

    let response = state
        .client
        .request(method, &url)
        .body(body_bytes)
        .send()
        .await
        .map_err(|_| axum::http::StatusCode::BAD_GATEWAY)?;

    response.text().await.map_err(|_| axum::http::StatusCode::BAD_GATEWAY)
}

async fn status_handler(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> axum::Json<serde_json::Value> {
    let all = state.pool.all().await;
    let mut healthy = Vec::new();
    let mut unhealthy = Vec::new();

    for name in all {
        if let Some(backend) = state.pool.get(&name).await {
            if backend.healthy {
                healthy.push(serde_json::json!({
                    "name": backend.config.name,
                    "url": backend.config.url,
                    "priority": backend.config.priority,
                    "models": backend.models,
                }));
            } else {
                unhealthy.push(name);
            }
        }
    }

    axum::Json(serde_json::json!({
        "healthy_backends": healthy,
        "unhealthy_backends": unhealthy,
    }))
}

async fn metrics_handler(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> String {
    let healthy = state.pool.all_healthy().await.len();
    let total = state.pool.all().await.len();
    
    format!(
        r#"# HELP herd_backends_total Total number of configured backends
# TYPE herd_backends_total gauge
herd_backends_total {}

# HELP herd_backends_healthy Number of healthy backends
# TYPE herd_backends_healthy gauge
herd_backends_healthy {}
"#,
        total, healthy
    )
}

pub async fn run(config: Config) -> Result<()> {
    let server = Server::new(config);
    server.run().await
}