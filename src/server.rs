use crate::analytics::{Analytics, RequestLog};
use crate::api::admin;
use crate::backend::{BackendPool, HealthChecker, ModelDiscovery};
use crate::config::Config;
use crate::router::{create_router, Router};
use crate::model_homing::ModelHoming;
use anyhow::Result;
use std::sync::Arc;
use tower_http::trace::TraceLayer;
use tracing::info;
use chrono::Timelike;

pub struct Server {
    config: Config,
}

#[derive(Clone)]
pub struct AppState {
    pub pool: Arc<BackendPool>,
    pub router: crate::router::RouterEnum,
    pub client: Arc<reqwest::Client>,
    pub config: Config,
    pub analytics: Arc<Analytics>,
}

impl Server {
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    pub async fn run(self) -> Result<()> {
        let addr = format!("{}:{}", self.config.server.host, self.config.server.port);
        info!("Starting Herd on {} with {} backends", addr, self.config.backends.len());

        // Create backend pool
        let pool = BackendPool::new(self.config.backends.clone());

        // Start health checker
        let health_checker = HealthChecker::new(10);
        let pool_clone = pool.clone();
        tokio::spawn(async move {
            health_checker.spawn(pool_clone).await;
        });

        // Start model discovery (every 60 seconds)
        let discovery = ModelDiscovery::new(60);
        let pool_clone = pool.clone();
        tokio::spawn(async move {
            discovery.spawn(pool_clone).await;
        });

        // Start model homing (every 5 minutes)
        let homing = ModelHoming::new(self.config.routing.idle_timeout_minutes);
        let pool_clone = pool.clone();
        tokio::spawn(async move {
            homing.spawn(pool_clone).await;
        });

        // Initialize analytics
        let analytics = Arc::new(Analytics::new()?);
        
        // Start 7-day cleanup task (runs daily at 3 AM)
        let analytics_clone = Arc::clone(&analytics);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(3600)).await; // Every hour
                let now = chrono::Local::now();
                let hour = now.hour();
                let minute = now.minute();
                if hour == 3 && minute < 5 {
                    if let Err(e) = analytics_clone.cleanup_old(7) {
                        tracing::error!("Failed to cleanup old analytics: {}", e);
                    } else {
                        tracing::info!("Cleaned up analytics logs older than 7 days");
                    }
                }
            }
        });

        // Create router
        let router = create_router(self.config.routing.strategy.clone(), pool.clone());

        // Wrap in Arc
        let pool = Arc::new(pool);
        let client = Arc::new(reqwest::Client::new());

        let state = AppState {
            pool: Arc::clone(&pool),
            router,
            client: Arc::clone(&client),
            config: self.config.clone(),
            analytics,
        };

        // Build admin sub-router
        let admin_routes = axum::Router::new()
            .route("/", axum::routing::get(admin::list_backends).post(admin::add_backend))
            .route("/:name", axum::routing::get(admin::get_backend)
                .put(admin::update_backend)
                .delete(admin::remove_backend));

        // Build app with routes
        let app = axum::Router::new()
            // Health check
            .route("/health", axum::routing::get(|| async { "OK" }))
            // Status and metrics
            .route("/status", axum::routing::get(status_handler))
            .route("/metrics", axum::routing::get(metrics_handler))
            .route("/analytics", axum::routing::get(analytics_handler))
            .route("/update", axum::routing::get(update_check_handler))
            .route("/gpu", axum::routing::get(gpu_handler))
            // Dashboard
            .route("/dashboard", axum::routing::get(dashboard_handler))
            // Admin API
            .nest("/admin/backends", admin_routes)
            // Proxy (catch-all)
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
) -> Result<axum::response::Response, axum::http::StatusCode> {
    let start = std::time::Instant::now();
    
    // Extract model from request if it's a generation request
    let path = request.uri().path().to_string();
    let mut model_name: Option<String> = None;
    
    // Route request
    let backend = state
        .router
        .route(model_name.as_deref())
        .await
        .map_err(|_| axum::http::StatusCode::SERVICE_UNAVAILABLE)?;

    // Touch the backend to update last_request time
    state.pool.touch_request(&backend.name).await;

    let url = format!("{}{}", backend.url, path);
    
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

    // Try to extract model from body for logging
    if path.contains("/api/generate") || path.contains("/api/chat") {
        if let Ok(body_json) = serde_json::from_slice::<serde_json::Value>(&body_bytes) {
            if let Some(model) = body_json.get("model").and_then(|m| m.as_str()) {
                model_name = Some(model.to_string());
            }
        }
    }

    let response = state
        .client
        .request(method, &url)
        .body(body_bytes)
        .send()
        .await;

    let duration = start.elapsed();
    let status = if response.is_ok() { "success" } else { "error" };
    
    // Log request
    let log = RequestLog {
        timestamp: chrono::Utc::now().timestamp(),
        model: model_name,
        backend: backend.name.clone(),
        duration_ms: duration.as_millis() as u64,
        status: status.to_string(),
        path: path.clone(),
    };
    
    if let Err(e) = state.analytics.log_request(log).await {
        tracing::error!("Failed to log request: {}", e);
    }

    match response {
        Ok(r) => {
            let status_code = axum::http::StatusCode::from_u16(r.status().as_u16()).unwrap_or(axum::http::StatusCode::OK);
            let body = r.bytes().await.map_err(|_| axum::http::StatusCode::BAD_GATEWAY)?;
            
            let response = axum::response::Response::builder()
                .status(status_code)
                .body(axum::body::Body::from(body))
                .unwrap();
            Ok(response)
        },
        Err(_) => Err(axum::http::StatusCode::BAD_GATEWAY)
    }
}

async fn status_handler(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> axum::Json<serde_json::Value> {
    let all = state.pool.all().await;
    let mut healthy = Vec::new();
    let mut unhealthy = Vec::new();

    for name in all {
        if let Some(backend) = state.pool.get(&name).await {
            let idle_secs = backend.last_request.elapsed().as_secs();
            let mut backend_json = serde_json::json!({
                "name": backend.config.name,
                "url": backend.config.url,
                "priority": backend.config.priority,
                "models": backend.models,
                "model_count": backend.models.len(),
                "current_model": backend.current_model,
                "default_model": backend.config.default_model,
                "idle_seconds": idle_secs,
                "healthy": backend.healthy,
            });

            if let Some(gpu) = &backend.gpu_metrics {
                backend_json["gpu"] = serde_json::json!({
                    "utilization": gpu.utilization,
                    "memory_used": gpu.memory_used,
                    "memory_total": gpu.memory_total,
                    "temperature": gpu.temperature,
                });
            }

            if backend.healthy {
                healthy.push(backend_json);
            } else {
                unhealthy.push(backend_json);
            }
        }
    }

    axum::Json(serde_json::json!({
        "healthy_backends": healthy,
        "unhealthy_backends": unhealthy,
        "routing_strategy": format!("{:?}", state.config.routing.strategy),
        "idle_timeout_minutes": state.config.routing.idle_timeout_minutes,
    }))
}

async fn metrics_handler(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> String {
    let healthy = state.pool.all_healthy().await.len();
    let total = state.pool.all().await.len();
    
    let mut metrics = format!(
        r#"# HELP herd_backends_total Total number of configured backends
# TYPE herd_backends_total gauge
herd_backends_total {}

# HELP herd_backends_healthy Number of healthy backends
# TYPE herd_backends_healthy gauge
herd_backends_healthy {}

# HELP herd_backend_info Backend information
# TYPE herd_backend_info gauge
"#,
        total, healthy
    );

    for name in state.pool.all().await {
        if let Some(backend) = state.pool.get(&name).await {
            let labels = format!(
                r#"name="{}",priority="{}",healthy="{}""#,
                backend.config.name,
                backend.config.priority,
                backend.healthy
            );
            metrics.push_str(&format!("herd_backend_info{{{}}} 1\n", labels));
            
            if let Some(gpu) = &backend.gpu_metrics {
                metrics.push_str(&format!(
                    r#"herd_backend_gpu_utilization{{name="{}"}} {}
herd_backend_gpu_memory_used{{name="{}"}} {}
herd_backend_gpu_memory_total{{name="{}"}} {}
herd_backend_gpu_temperature{{name="{}"}} {}
"#,
                    backend.config.name, gpu.utilization,
                    backend.config.name, gpu.memory_used,
                    backend.config.name, gpu.memory_total,
                    backend.config.name, gpu.temperature
                ));
            }
        }
    }

    metrics
}

async fn analytics_handler(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> axum::Json<serde_json::Value> {
    let hours = params.get("hours")
        .and_then(|h| h.parse::<i64>().ok())
        .unwrap_or(24);
    
    let seconds = hours * 3600;
    
    match state.analytics.get_stats(seconds) {
        Ok(stats) => axum::Json(serde_json::to_value(&stats).unwrap()),
        Err(e) => axum::Json(serde_json::json!({
            "error": format!("Failed to get analytics: {}", e)
        }))
    }
}

async fn update_check_handler() -> axum::Json<serde_json::Value> {
    const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
    const REPO: &str = "swift-innovate/herd";
    
    let client = reqwest::Client::new();
    let url = format!("https://api.github.com/repos/{}/releases/latest", REPO);
    
    match client.get(&url)
        .header("User-Agent", "Herd")
        .send()
        .await
    {
        Ok(response) => {
            if let Ok(release) = response.json::<serde_json::Value>().await {
                let latest = release["tag_name"].as_str().unwrap_or(CURRENT_VERSION).trim_start_matches('v');
                axum::Json(serde_json::json!({
                    "current": CURRENT_VERSION,
                    "latest": latest,
                    "update_available": latest != CURRENT_VERSION,
                    "download_url": release["html_url"].as_str(),
                }))
            } else {
                axum::Json(serde_json::json!({
                    "current": CURRENT_VERSION,
                    "error": "Failed to parse release info"
                }))
            }
        },
        Err(e) => {
            axum::Json(serde_json::json!({
                "current": CURRENT_VERSION,
                "error": format!("Failed to check for updates: {}", e)
            }))
        }
    }
}

async fn gpu_handler(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> axum::Json<serde_json::Value> {
    // Try to fetch GPU data from hot-gpu at 100.107.157.73:1312
    match state
        .client
        .get("http://100.107.157.73:1312/api/gpu-data")
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            match resp.json::<serde_json::Value>().await {
                Ok(data) => axum::Json(data),
                Err(_) => axum::Json(serde_json::json!({"available": false, "error": "Failed to parse GPU data"})),
            }
        }
        _ => axum::Json(serde_json::json!({"available": false, "error": "hot-gpu not reachable"})),
    }
}

async fn dashboard_handler() -> axum::response::Html<&'static str> {
    axum::response::Html(include_str!("../dashboard.html"))
}

pub async fn run(config: Config) -> Result<()> {
    let server = Server::new(config);
    server.run().await
}
