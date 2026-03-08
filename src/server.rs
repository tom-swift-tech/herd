use crate::analytics::{Analytics, RequestLog};
use crate::api::{admin, openai};
use crate::backend::{BackendPool, HealthChecker, ModelDiscovery};
use crate::config::{parse_duration, Config};
use crate::model_homing::ModelHoming;
use crate::router::{create_router, Router};
use anyhow::Result;
use chrono::Timelike;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tower_http::trace::TraceLayer;
use tracing::info;

const DEFAULT_ROUTING_TIMEOUT: Duration = Duration::from_secs(120);
const DEFAULT_CIRCUIT_TIMEOUT: Duration = Duration::from_secs(120);
const DEFAULT_RECOVERY_TIME: Duration = Duration::from_secs(60);
const MAX_PROXY_BODY_BYTES: usize = 10 * 1024 * 1024; // 10 MB

pub struct Server {
    config: Config,
    config_path: Option<PathBuf>,
}

#[derive(Clone)]
pub struct AppState {
    pub pool: Arc<BackendPool>,
    pub router: Arc<tokio::sync::RwLock<crate::router::RouterEnum>>,
    pub client: Arc<reqwest::Client>,
    pub config: Config,
    pub analytics: Arc<Analytics>,
    pub metrics: Arc<crate::metrics::Metrics>,
    pub routing_timeout_ms: Arc<AtomicU64>,
    pub routing_retry_count: Arc<AtomicU32>,
    pub config_path: Option<PathBuf>,
}

impl AppState {
    pub fn routing_timeout(&self) -> Duration {
        Duration::from_millis(self.routing_timeout_ms.load(Ordering::Relaxed))
    }

    pub fn retry_count(&self) -> u32 {
        self.routing_retry_count.load(Ordering::Relaxed)
    }

    pub async fn reload_config(&self) -> anyhow::Result<String> {
        let path = self
            .config_path
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No config file path (started with CLI args)"))?;

        let new_config = Config::from_file(path)?;

        // Sync backends: remove deleted, add new, update existing
        let existing = self.pool.all().await;
        let new_names: std::collections::HashSet<_> =
            new_config.backends.iter().map(|b| b.name.clone()).collect();

        for name in &existing {
            if !new_names.contains(name) {
                self.pool.remove(name).await;
                tracing::info!("Reload: removed backend {}", name);
            }
        }

        for backend in &new_config.backends {
            if existing.contains(&backend.name) {
                if let Some(mut state) = self.pool.get(&backend.name).await {
                    state.config = backend.clone();
                    self.pool.update(state).await;
                    tracing::info!("Reload: updated backend {}", backend.name);
                }
            } else {
                self.pool.add(backend.clone()).await;
                tracing::info!("Reload: added backend {}", backend.name);
            }
        }

        // Swap router (new router shares the same pool data)
        let new_router = create_router(new_config.routing.strategy.clone(), (*self.pool).clone());
        *self.router.write().await = new_router;

        // Update timeout and retry count
        let new_timeout =
            parse_duration(&new_config.routing.timeout).unwrap_or(DEFAULT_ROUTING_TIMEOUT);
        self.routing_timeout_ms
            .store(new_timeout.as_millis() as u64, Ordering::Relaxed);
        self.routing_retry_count
            .store(new_config.routing.retry_count, Ordering::Relaxed);

        let msg = format!(
            "Reloaded: {} backends, strategy={:?}",
            new_config.backends.len(),
            new_config.routing.strategy
        );
        tracing::info!("{}", msg);
        Ok(msg)
    }
}

impl Server {
    pub fn new(config: Config, config_path: Option<PathBuf>) -> Self {
        Self {
            config,
            config_path,
        }
    }

    pub async fn run(self) -> Result<()> {
        let addr = format!("{}:{}", self.config.server.host, self.config.server.port);
        info!(
            "Starting Herd on {} with {} backends",
            addr,
            self.config.backends.len()
        );

        // Parse durations from config
        let routing_timeout =
            parse_duration(&self.config.routing.timeout).unwrap_or(DEFAULT_ROUTING_TIMEOUT);
        let circuit_timeout =
            parse_duration(&self.config.circuit_breaker.timeout).unwrap_or(DEFAULT_CIRCUIT_TIMEOUT);
        let recovery_time = parse_duration(&self.config.circuit_breaker.recovery_time)
            .unwrap_or(DEFAULT_RECOVERY_TIME);

        // Validate: admin_api requires api_key — disable gracefully if missing
        let admin_api_enabled = if self.config.observability.admin_api
            && self.config.server.api_key.is_none()
        {
            tracing::warn!(
                "observability.admin_api is enabled but server.api_key is not set — disabling admin API"
            );
            false
        } else {
            self.config.observability.admin_api
        };

        // Create backend pool with circuit breaker config
        let pool = BackendPool::new(
            self.config.backends.clone(),
            self.config.circuit_breaker.failure_threshold,
            recovery_time,
        );

        // Start health checker
        let health_checker = HealthChecker::new(Duration::from_secs(10));
        health_checker.spawn(pool.clone()).await;

        // Start model discovery (every 60 seconds)
        let discovery = ModelDiscovery::new(60);
        discovery.spawn(pool.clone()).await;

        // Start model homing (every 5 minutes)
        let homing = ModelHoming::new(self.config.routing.idle_timeout_minutes);
        homing.spawn(pool.clone()).await;

        // Initialize analytics
        let analytics = Arc::new(Analytics::new()?);

        // Initialize in-memory request metrics
        let metrics = Arc::new(crate::metrics::Metrics::new());

        // Start log rotation and cleanup task
        let analytics_clone = Arc::clone(&analytics);
        let retention_days = self.config.observability.log_retention_days as i64;
        let max_size_mb = self.config.observability.log_max_size_mb;
        let max_files = self.config.observability.log_max_files;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(3600)).await;

                // Check rotation every hour
                match analytics_clone
                    .rotate_if_needed(max_size_mb, max_files)
                    .await
                {
                    Ok(true) => tracing::info!("Log file rotated"),
                    Ok(false) => {}
                    Err(e) => tracing::error!("Log rotation failed: {}", e),
                }

                // Cleanup old entries daily at 3 AM
                let now = chrono::Local::now();
                let hour = now.hour();
                let minute = now.minute();
                if hour == 3 && minute < 5 {
                    if let Err(e) = analytics_clone.cleanup_old(retention_days).await {
                        tracing::error!("Failed to cleanup old analytics: {}", e);
                    } else {
                        tracing::info!(
                            "Cleaned up analytics logs older than {} days",
                            retention_days
                        );
                    }
                }
            }
        });

        // Create router
        let router = create_router(self.config.routing.strategy.clone(), pool.clone());

        // Wrap in Arc
        let pool = Arc::new(pool);
        let client = Arc::new(
            reqwest::Client::builder()
                .timeout(circuit_timeout)
                .build()?,
        );

        let state = AppState {
            pool: Arc::clone(&pool),
            router: Arc::new(tokio::sync::RwLock::new(router)),
            client: Arc::clone(&client),
            config: self.config.clone(),
            analytics,
            metrics,
            routing_timeout_ms: Arc::new(AtomicU64::new(routing_timeout.as_millis() as u64)),
            routing_retry_count: Arc::new(AtomicU32::new(self.config.routing.retry_count)),
            config_path: self.config_path.clone(),
        };

        // Build app with routes
        let mut app = axum::Router::new()
            // Health check (always available, no auth)
            .route("/health", axum::routing::get(|| async { "OK" }))
            // Status
            .route("/status", axum::routing::get(status_handler))
            // Dashboard
            .route("/dashboard", axum::routing::get(dashboard_handler))
            // OpenAI-compatible API
            .route("/v1/models", axum::routing::get(openai::list_models))
            .route(
                "/v1/chat/completions",
                axum::routing::post(openai::chat_completions),
            )
            // Update check
            .route("/update", axum::routing::get(update_check_handler))
            // GPU handler
            .route("/gpu", axum::routing::get(gpu_handler));

        // Conditionally mount metrics
        if self.config.observability.metrics {
            app = app
                .route("/metrics", axum::routing::get(metrics_handler))
                .route("/analytics", axum::routing::get(analytics_handler));
        }

        // Conditionally mount admin API (requires auth)
        if admin_api_enabled {
            let admin_routes = axum::Router::new()
                .route(
                    "/",
                    axum::routing::get(admin::list_backends).post(admin::add_backend),
                )
                .route(
                    "/:name",
                    axum::routing::get(admin::get_backend)
                        .put(admin::update_backend)
                        .delete(admin::remove_backend),
                )
                .layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    require_api_key,
                ));

            let reload_route = axum::Router::new()
                .route("/admin/reload", axum::routing::post(reload_handler))
                .route("/admin/update", axum::routing::post(update_self_handler))
                .layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    require_api_key,
                ));

            app = app
                .nest("/admin/backends", admin_routes)
                .merge(reload_route);
        }

        // Clone state for file watcher before it's consumed by with_state()
        let watcher_state = state.clone();

        // Proxy (catch-all) + middleware layers
        let app = if self.config.server.rate_limit > 0 {
            let limiter = Arc::new(RateLimiter::new(self.config.server.rate_limit));
            app.fallback(proxy_handler)
                .layer(tower::ServiceBuilder::new().layer(TraceLayer::new_for_http()))
                .layer(axum::middleware::from_fn(
                    move |req, next: axum::middleware::Next| {
                        let limiter = Arc::clone(&limiter);
                        async move {
                            if limiter.try_acquire() {
                                Ok(next.run(req).await)
                            } else {
                                Err(axum::http::StatusCode::TOO_MANY_REQUESTS)
                            }
                        }
                    },
                ))
                .with_state(state)
        } else {
            app.fallback(proxy_handler)
                .layer(tower::ServiceBuilder::new().layer(TraceLayer::new_for_http()))
                .with_state(state)
        };

        // Start config file watcher (polls every 30s)
        if let Some(ref config_path) = self.config_path {
            let watch_state = watcher_state;
            let watch_path = config_path.clone();
            let initial_mtime = std::fs::metadata(&watch_path)
                .ok()
                .and_then(|m| m.modified().ok());
            tokio::spawn(async move {
                let mut last_mtime = initial_mtime;
                let mut ticker = tokio::time::interval(Duration::from_secs(30));
                ticker.tick().await; // skip first immediate tick
                loop {
                    ticker.tick().await;
                    if let Ok(meta) = std::fs::metadata(&watch_path) {
                        if let Ok(mtime) = meta.modified() {
                            if last_mtime.is_some_and(|prev| mtime > prev) {
                                last_mtime = Some(mtime);
                                tracing::info!("Config file changed, reloading...");
                                if let Err(e) = watch_state.reload_config().await {
                                    tracing::error!("Config reload failed: {}", e);
                                }
                            } else if last_mtime.is_none() {
                                last_mtime = Some(mtime);
                            }
                        }
                    }
                }
            });
        }

        // Start server
        let listener = tokio::net::TcpListener::bind(&addr).await?;
        axum::serve(listener, app).await?;

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Token-bucket rate limiter
// ---------------------------------------------------------------------------

struct RateLimiter {
    tokens: Arc<std::sync::atomic::AtomicU64>,
}

impl RateLimiter {
    fn new(requests_per_second: u64) -> Self {
        let tokens = Arc::new(std::sync::atomic::AtomicU64::new(requests_per_second));
        // Spawn refill task
        let tokens_clone = Arc::clone(&tokens);
        let max = requests_per_second;
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(1));
            loop {
                ticker.tick().await;
                tokens_clone.store(max, std::sync::atomic::Ordering::Relaxed);
            }
        });
        Self { tokens }
    }

    fn try_acquire(&self) -> bool {
        loop {
            let current = self.tokens.load(std::sync::atomic::Ordering::Relaxed);
            if current == 0 {
                return false;
            }
            if self
                .tokens
                .compare_exchange_weak(
                    current,
                    current - 1,
                    std::sync::atomic::Ordering::Relaxed,
                    std::sync::atomic::Ordering::Relaxed,
                )
                .is_ok()
            {
                return true;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// API key authentication middleware
// ---------------------------------------------------------------------------

fn extract_api_key(req: &axum::extract::Request) -> Option<String> {
    // Try X-API-Key header first
    if let Some(val) = req.headers().get("x-api-key") {
        return val.to_str().ok().map(|s| s.to_string());
    }
    // Try Authorization: Bearer <key>
    if let Some(val) = req.headers().get("authorization") {
        if let Ok(s) = val.to_str() {
            if let Some(token) = s.strip_prefix("Bearer ") {
                return Some(token.to_string());
            }
        }
    }
    None
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

pub async fn require_api_key(
    axum::extract::State(state): axum::extract::State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, axum::http::StatusCode> {
    let expected = match &state.config.server.api_key {
        Some(key) => key,
        None => return Ok(next.run(req).await), // no key configured = allow
    };

    let provided = extract_api_key(&req).ok_or(axum::http::StatusCode::UNAUTHORIZED)?;

    if !constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
        return Err(axum::http::StatusCode::UNAUTHORIZED);
    }

    Ok(next.run(req).await)
}

// ---------------------------------------------------------------------------
// Proxy handler
// ---------------------------------------------------------------------------

fn copy_request_headers(
    src: &axum::http::HeaderMap,
    builder: reqwest::RequestBuilder,
) -> reqwest::RequestBuilder {
    let mut builder = builder;
    for (name, value) in src.iter() {
        // Skip host and connection-level headers
        if name == axum::http::header::HOST
            || name == axum::http::header::CONNECTION
            || name == axum::http::header::TRANSFER_ENCODING
        {
            continue;
        }
        if let (Ok(rname), Ok(rval)) = (
            reqwest::header::HeaderName::from_bytes(name.as_ref()),
            reqwest::header::HeaderValue::from_bytes(value.as_ref()),
        ) {
            builder = builder.header(rname, rval);
        }
    }
    builder
}

async fn proxy_handler(
    axum::extract::State(state): axum::extract::State<AppState>,
    request: axum::extract::Request,
) -> Result<axum::response::Response, axum::http::StatusCode> {
    let start = std::time::Instant::now();

    // Preserve full path + query string
    let path_and_query = request
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| request.uri().path().to_string());
    let path = request.uri().path().to_string();
    let mut model_name: Option<String> = None;

    // Bridge HTTP method (axum http 1.x → reqwest http 0.2)
    let method = reqwest::Method::from_bytes(request.method().as_str().as_bytes())
        .unwrap_or(reqwest::Method::POST);

    // Collect request headers before consuming body
    let mut headers = request.headers().clone();

    // Get or generate correlation ID
    let request_id = headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            let id = uuid::Uuid::new_v4().to_string();
            // Insert into headers so it gets forwarded to upstream
            if let Ok(val) = axum::http::HeaderValue::from_str(&id) {
                headers.insert("x-request-id", val);
            }
            id
        });

    // Cap body size to prevent DoS
    let body_bytes = axum::body::to_bytes(request.into_body(), MAX_PROXY_BODY_BYTES)
        .await
        .map_err(|_| axum::http::StatusCode::PAYLOAD_TOO_LARGE)?;

    // Try to extract model from body for routing and logging.
    if path.contains("/api/generate")
        || path.contains("/api/chat")
        || path.contains("/v1/chat/completions")
        || path.contains("/v1/completions")
    {
        if let Ok(body_json) = serde_json::from_slice::<serde_json::Value>(&body_bytes) {
            if let Some(model) = body_json.get("model").and_then(|m| m.as_str()) {
                model_name = Some(model.to_string());
            }
        }
    }

    // Extract tags from X-Herd-Tags header (comma-separated)
    let tags: Option<Vec<String>> = headers
        .get("x-herd-tags")
        .and_then(|v| v.to_str().ok())
        .map(|s| {
            s.split(',')
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect()
        });

    // Retry loop: try routing to different backends on failure
    let mut response = None;
    let mut selected_backend: Option<String> = None;

    for _ in 0..=state.retry_count() {
        let backend = state
            .router
            .read()
            .await
            .route(model_name.as_deref(), tags.as_deref())
            .await
            .map_err(|_| axum::http::StatusCode::SERVICE_UNAVAILABLE)?;

        state.pool.touch_request(&backend.name).await;
        selected_backend = Some(backend.name.clone());
        let url = format!("{}{}", backend.url, path_and_query);

        let req_builder = state
            .client
            .request(method.clone(), &url)
            .timeout(state.routing_timeout())
            .body(body_bytes.clone());
        let req_builder = copy_request_headers(&headers, req_builder);

        match req_builder.send().await {
            Ok(r) => {
                state.pool.mark_healthy(&backend.name).await;
                response = Some(r);
                break;
            }
            Err(e) => {
                tracing::warn!(
                    "Upstream request to {} failed via {}: {}",
                    url,
                    backend.name,
                    e
                );
                state.pool.mark_unhealthy(&backend.name).await;
            }
        }
    }

    let duration = start.elapsed();
    let status = if response.is_some() {
        "success"
    } else {
        "error"
    };

    // Log request
    let log = RequestLog {
        timestamp: chrono::Utc::now().timestamp(),
        model: model_name,
        backend: selected_backend.unwrap_or_else(|| "none".to_string()),
        duration_ms: duration.as_millis() as u64,
        status: status.to_string(),
        path: path.clone(),
        request_id: Some(request_id.clone()),
    };

    state
        .metrics
        .record_request(&log.backend, &log.status, log.duration_ms)
        .await;

    if let Err(e) = state.analytics.log_request(log).await {
        tracing::error!("Failed to log request: {}", e);
    }

    match response {
        Some(r) => {
            let status_code = axum::http::StatusCode::from_u16(r.status().as_u16())
                .unwrap_or(axum::http::StatusCode::OK);

            let mut builder = axum::response::Response::builder()
                .status(status_code)
                .header("x-request-id", &request_id);

            // Forward response headers (bridge reqwest http 0.2 → axum http 1.x)
            for (name, value) in r.headers() {
                if let (Ok(aname), Ok(aval)) = (
                    axum::http::header::HeaderName::from_bytes(name.as_ref()),
                    axum::http::header::HeaderValue::from_bytes(value.as_ref()),
                ) {
                    builder = builder.header(aname, aval);
                }
            }

            // Stream the body instead of buffering
            let body = axum::body::Body::from_stream(r.bytes_stream());
            builder
                .body(body)
                .map_err(|_| axum::http::StatusCode::BAD_GATEWAY)
        }
        None => {
            let body = axum::body::Body::from(format!(
                "{{\"error\":\"Bad Gateway\",\"request_id\":\"{}\"}}",
                request_id
            ));
            Ok(axum::response::Response::builder()
                .status(axum::http::StatusCode::BAD_GATEWAY)
                .header("x-request-id", &request_id)
                .header("content-type", "application/json")
                .body(body)
                .unwrap_or_default())
        }
    }
}

// ---------------------------------------------------------------------------
// Status / metrics / analytics / other handlers
// ---------------------------------------------------------------------------

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

async fn metrics_handler(axum::extract::State(state): axum::extract::State<AppState>) -> String {
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
                backend.config.name, backend.config.priority, backend.healthy
            );
            metrics.push_str(&format!("herd_backend_info{{{}}} 1\n", labels));

            if let Some(gpu) = &backend.gpu_metrics {
                metrics.push_str(&format!(
                    r#"herd_backend_gpu_utilization{{name="{}"}} {}
herd_backend_gpu_memory_used{{name="{}"}} {}
herd_backend_gpu_memory_total{{name="{}"}} {}
herd_backend_gpu_temperature{{name="{}"}} {}
"#,
                    backend.config.name,
                    gpu.utilization,
                    backend.config.name,
                    gpu.memory_used,
                    backend.config.name,
                    gpu.memory_total,
                    backend.config.name,
                    gpu.temperature
                ));
            }
        }
    }

    // Append request counters and latency histogram
    metrics.push('\n');
    metrics.push_str(&state.metrics.render().await);

    metrics
}

async fn analytics_handler(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> axum::Json<serde_json::Value> {
    let hours = params
        .get("hours")
        .and_then(|h| h.parse::<i64>().ok())
        .unwrap_or(24)
        .clamp(1, 168); // Cap at 7 days

    let seconds = hours * 3600;

    match state.analytics.get_stats(seconds).await {
        Ok(stats) => axum::Json(
            serde_json::to_value(&stats)
                .unwrap_or_else(|_| serde_json::json!({"error": "serialization failed"})),
        ),
        Err(e) => axum::Json(serde_json::json!({
            "error": format!("Failed to get analytics: {}", e)
        })),
    }
}

async fn reload_handler(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Result<axum::Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    match state.reload_config().await {
        Ok(msg) => Ok(axum::Json(serde_json::json!({
            "status": "ok",
            "message": msg,
        }))),
        Err(e) => Err((
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("Reload failed: {}", e),
        )),
    }
}

async fn update_check_handler() -> axum::Json<serde_json::Value> {
    match tokio::task::spawn_blocking(crate::updater::check_for_update).await {
        Ok(Ok(info)) => axum::Json(serde_json::json!({
            "current": info.current,
            "latest": info.latest,
            "update_available": info.update_available,
        })),
        Ok(Err(e)) => axum::Json(serde_json::json!({
            "current": env!("CARGO_PKG_VERSION"),
            "error": format!("Failed to check for updates: {}", e),
        })),
        Err(e) => axum::Json(serde_json::json!({
            "current": env!("CARGO_PKG_VERSION"),
            "error": format!("Update check task failed: {}", e),
        })),
    }
}

async fn update_self_handler(
) -> Result<axum::Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    // Check first
    let info = tokio::task::spawn_blocking(crate::updater::check_for_update)
        .await
        .map_err(|e| {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("Task failed: {}", e),
            )
        })?
        .map_err(|e| {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("Check failed: {}", e),
            )
        })?;

    if !info.update_available {
        return Ok(axum::Json(serde_json::json!({
            "status": "up_to_date",
            "current": info.current,
        })));
    }

    // Perform update (no progress bar for API)
    let version = tokio::task::spawn_blocking(|| crate::updater::perform_update(false))
        .await
        .map_err(|e| {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("Task failed: {}", e),
            )
        })?
        .map_err(|e| {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("Update failed: {}", e),
            )
        })?;

    tracing::info!("Binary updated to v{}. Restart required.", version);

    Ok(axum::Json(serde_json::json!({
        "status": "updated",
        "previous": info.current,
        "updated_to": version,
        "message": "Binary updated. Restart the server to use the new version.",
    })))
}

async fn gpu_handler(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> axum::Json<serde_json::Value> {
    let backends = state.pool.all_healthy().await;
    let mut gpu_data = std::collections::HashMap::new();

    for name in backends {
        if let Some(backend) = state.pool.get(&name).await {
            let gpu_url = if let Some(ref base) = backend.config.gpu_hot_url {
                let base = base.trim_end_matches('/');
                format!("{}/api/gpu-data", base)
            } else {
                let host = backend
                    .config
                    .url
                    .trim_start_matches("http://")
                    .trim_start_matches("https://")
                    .split(':')
                    .next()
                    .unwrap_or("");
                if host.is_empty() {
                    continue;
                }
                format!("http://{}:1312/api/gpu-data", host)
            };

            match state
                .client
                .get(&gpu_url)
                .timeout(std::time::Duration::from_secs(2))
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    if let Ok(data) = resp.json::<serde_json::Value>().await {
                        gpu_data.insert(name.clone(), data);
                    }
                }
                _ => {
                    tracing::trace!("No gpu-hot data from {}", name);
                }
            }
        }
    }

    if gpu_data.is_empty() {
        axum::Json(serde_json::json!({
            "available": false,
            "error": "No gpu-hot endpoints available"
        }))
    } else {
        axum::Json(serde_json::json!({
            "available": true,
            "backends": gpu_data
        }))
    }
}

async fn dashboard_handler() -> axum::response::Html<&'static str> {
    axum::response::Html(include_str!("../dashboard.html"))
}

pub async fn run(config: Config, config_path: Option<PathBuf>) -> Result<()> {
    let server = Server::new(config, config_path);
    server.run().await
}
