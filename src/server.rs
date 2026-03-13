use crate::analytics::{Analytics, RequestLog};
use crate::api::{admin, openai};
use crate::backend::{BackendPool, HealthChecker, ModelDiscovery, ModelWarmer};
use crate::config::{parse_duration, Config};
use crate::router::{create_router, Router};
use anyhow::Result;
use chrono::Timelike;
use std::collections::HashSet;
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
    /// Long-timeout client for management operations (e.g. model pulls).
    pub mgmt_client: Arc<reqwest::Client>,
    pub config: Arc<tokio::sync::RwLock<Config>>,
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

    pub async fn config_snapshot(&self) -> Config {
        self.config.read().await.clone()
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
        *self.config.write().await = new_config.clone();

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

        let warmer = ModelWarmer::new(self.config.model_warmer.interval_secs);
        warmer.spawn(pool.clone()).await;

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
        let mgmt_client = Arc::new(
            reqwest::Client::builder()
                .timeout(Duration::from_secs(3600))
                .build()?,
        );

        let state = AppState {
            pool: Arc::clone(&pool),
            router: Arc::new(tokio::sync::RwLock::new(router)),
            client: Arc::clone(&client),
            mgmt_client,
            config: Arc::new(tokio::sync::RwLock::new(self.config.clone())),
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
            .route("/gpu", axum::routing::get(gpu_handler))
            // Agent skills reference
            .route("/skills", axum::routing::get(skills_handler))
            .route("/skills.md", axum::routing::get(skills_md_handler));

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
                .route("/:name/models", axum::routing::get(admin::list_backend_models))
                .route("/:name/models/:model", axum::routing::delete(admin::delete_model))
                .route("/:name/pull", axum::routing::post(admin::pull_model))
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
    let expected = match state.config.read().await.server.api_key.clone() {
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

/// Injects `keep_alive` into an Ollama-native request body.
/// Only applies to /api/generate and /api/chat; all other paths and
/// invalid JSON bodies are returned unchanged.
fn inject_keep_alive(body: &[u8], path: &str, keep_alive: &str) -> axum::body::Bytes {
    let is_ollama_endpoint =
        path.contains("/api/generate") || path.contains("/api/chat");
    if !is_ollama_endpoint {
        return axum::body::Bytes::copy_from_slice(body);
    }
    let Ok(mut payload) = serde_json::from_slice::<serde_json::Value>(body) else {
        return axum::body::Bytes::copy_from_slice(body);
    };
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("keep_alive".to_string(), serde_json::Value::String(keep_alive.to_string()));
    }
    match serde_json::to_vec(&payload) {
        Ok(modified) => axum::body::Bytes::from(modified),
        Err(_) => axum::body::Bytes::copy_from_slice(body),
    }
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

    // Inject keep_alive for Ollama-native endpoints
    let keep_alive_value = state.config.read().await.routing.default_keep_alive.clone();
    let forward_bytes = inject_keep_alive(&body_bytes, &path, &keep_alive_value);

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
    let mut excluded = HashSet::new();

    for _ in 0..=state.retry_count() {
        let backend = state
            .router
            .read()
            .await
            .route_excluding(model_name.as_deref(), tags.as_deref(), &excluded)
            .await
            .map_err(|_| axum::http::StatusCode::SERVICE_UNAVAILABLE)?;

        state.pool.touch_request(&backend.name).await;
        selected_backend = Some(backend.name.clone());
        let url = format!("{}{}", backend.url, path_and_query);

        let req_builder = state
            .client
            .request(method.clone(), &url)
            .timeout(state.routing_timeout())
            .body(forward_bytes.clone());
        let req_builder = copy_request_headers(&headers, req_builder);

        match req_builder.send().await {
            Ok(r) => {
                // Treat 404 on model endpoints as retryable — model was likely
                // evicted by Ollama, another backend may still have it warm.
                let is_model_endpoint = path.contains("/api/generate")
                    || path.contains("/api/chat")
                    || path.contains("/v1/chat/completions")
                    || path.contains("/v1/completions");

                if r.status().as_u16() == 404 && is_model_endpoint {
                    tracing::warn!(
                        "Backend {} returned 404 for {} — model likely evicted, retrying",
                        backend.name,
                        path
                    );
                    excluded.insert(backend.name.clone());
                    continue;
                }

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
                excluded.insert(backend.name.clone());
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
    let config = state.config_snapshot().await;
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
                "hot_models": backend.config.hot_models,
                "idle_seconds": idle_secs,
                "healthy": backend.healthy,
                "vram_total_mb": backend.vram_total_mb,
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
        "routing_strategy": format!("{:?}", config.routing.strategy),
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

async fn skills_handler(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> axum::Json<serde_json::Value> {
    let config = state.config_snapshot().await;
    let strategy = format!("{:?}", config.routing.strategy);

    axum::Json(serde_json::json!({
        "herd_version": env!("CARGO_PKG_VERSION"),
        "routing_strategy": strategy,
        "endpoints": {
            "chat": {
                "method": "POST",
                "path": "/v1/chat/completions",
                "description": "OpenAI-compatible chat completions (streaming supported)",
                "auth": false
            },
            "models": {
                "method": "GET",
                "path": "/v1/models",
                "description": "List all models across healthy backends",
                "auth": false
            },
            "status": {
                "method": "GET",
                "path": "/status",
                "description": "Cluster health, backend details, GPU metrics",
                "auth": false
            },
            "health": {
                "method": "GET",
                "path": "/health",
                "description": "Liveness check — returns OK",
                "auth": false
            },
            "generate": {
                "method": "POST",
                "path": "/api/generate",
                "description": "Ollama single-turn generation (proxied)",
                "auth": false
            },
            "chat_ollama": {
                "method": "POST",
                "path": "/api/chat",
                "description": "Ollama multi-turn chat (proxied)",
                "auth": false
            },
            "analytics": {
                "method": "GET",
                "path": "/analytics?hours=24",
                "description": "Request stats, latency percentiles, timeline",
                "auth": false
            },
            "metrics": {
                "method": "GET",
                "path": "/metrics",
                "description": "Prometheus exposition format",
                "auth": false
            }
        },
        "headers": {
            "X-Herd-Tags": "Comma-separated tags to target specific backends (e.g. 'gpu,fast')",
            "X-Request-Id": "Correlation ID — send your own or Herd generates a UUID v4",
            "X-API-Key": "Required for admin endpoints only"
        },
        "best_practices": [
            "Always specify 'model' in requests for optimal routing",
            "Use 'stream': true for long responses to avoid timeouts",
            "Query GET /v1/models to discover available models before requesting",
            "Send X-Herd-Tags to target backends suited for your workload",
            "Send X-Request-Id for traceability across distributed systems",
            "Retry on 503 — circuit breaker auto-recovers backends",
            "Never hard-code backend URLs — always route through Herd",
            "Body size limit is 10 MB",
            "Do not set 'keep_alive' in request bodies — Herd injects it centrally on every Ollama request"
        ],
        "error_codes": {
            "502": "Backend failed — Herd will retry on another backend",
            "503": "No healthy backend available — wait and retry",
            "413": "Request body too large (>10 MB)",
            "429": "Rate limit exceeded — back off and retry"
        }
    }))
}

async fn dashboard_handler() -> axum::response::Html<&'static str> {
    axum::response::Html(include_str!("../dashboard.html"))
}

async fn skills_md_handler() -> ([(axum::http::header::HeaderName, &'static str); 1], &'static str) {
    (
        [(axum::http::header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
        include_str!("../skills.md"),
    )
}

pub async fn run(config: Config, config_path: Option<PathBuf>) -> Result<()> {
    let server = Server::new(config, config_path);
    server.run().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Backend, RoutingStrategy};

    fn make_backend(name: &str, url: &str, priority: u32) -> Backend {
        Backend {
            name: name.into(),
            url: url.into(),
            priority,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn reload_config_updates_live_state() {
        let initial = Config {
            server: crate::config::ServerConfig {
                api_key: Some("old-key".into()),
                ..Default::default()
            },
            routing: crate::config::RoutingConfig {
                strategy: RoutingStrategy::Priority,
                retry_count: 1,
                ..Default::default()
            },
            backends: vec![make_backend("old", "http://old:11434", 100)],
            ..Default::default()
        };

        let updated = Config {
            server: crate::config::ServerConfig {
                api_key: Some("new-key".into()),
                ..Default::default()
            },
            routing: crate::config::RoutingConfig {
                strategy: RoutingStrategy::LeastBusy,
                retry_count: 4,
                ..Default::default()
            },
            backends: vec![make_backend("new", "http://new:11434", 10)],
            ..Default::default()
        };

        let temp_path = std::env::temp_dir().join(format!(
            "herd-reload-test-{}-{}.yaml",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        std::fs::write(&temp_path, updated.to_yaml().unwrap()).unwrap();

        let pool = Arc::new(BackendPool::new(
            initial.backends.clone(),
            initial.circuit_breaker.failure_threshold,
            parse_duration(&initial.circuit_breaker.recovery_time).unwrap(),
        ));
        let router = create_router(initial.routing.strategy.clone(), (*pool).clone());

        let state = AppState {
            pool,
            router: Arc::new(tokio::sync::RwLock::new(router)),
            client: Arc::new(reqwest::Client::new()),
            mgmt_client: Arc::new(reqwest::Client::new()),
            config: Arc::new(tokio::sync::RwLock::new(initial)),
            analytics: Arc::new(Analytics::new().unwrap()),
            metrics: Arc::new(crate::metrics::Metrics::new()),
            routing_timeout_ms: Arc::new(AtomicU64::new(1_000)),
            routing_retry_count: Arc::new(AtomicU32::new(1)),
            config_path: Some(temp_path.clone()),
        };

        let message = state.reload_config().await.unwrap();
        let snapshot = state.config_snapshot().await;

        assert!(message.contains("LeastBusy"));
        assert_eq!(snapshot.server.api_key.as_deref(), Some("new-key"));
        assert_eq!(snapshot.routing.strategy, RoutingStrategy::LeastBusy);
        assert_eq!(state.retry_count(), 4);
        assert_eq!(state.pool.all().await, vec![String::from("new")]);

        let _ = std::fs::remove_file(temp_path);
    }

    #[test]
    fn keep_alive_injected_into_api_generate() {
        let body = serde_json::json!({"model": "llama3", "prompt": "hi"});
        let bytes = serde_json::to_vec(&body).unwrap();
        let result = inject_keep_alive(&bytes, "/api/generate", "-1");
        let parsed: serde_json::Value = serde_json::from_slice(&result).unwrap();
        assert_eq!(parsed["keep_alive"], "-1");
        assert_eq!(parsed["model"], "llama3");
    }

    #[test]
    fn keep_alive_injected_into_api_chat() {
        let body = serde_json::json!({"model": "llama3", "messages": []});
        let bytes = serde_json::to_vec(&body).unwrap();
        let result = inject_keep_alive(&bytes, "/api/chat", "-1");
        let parsed: serde_json::Value = serde_json::from_slice(&result).unwrap();
        assert_eq!(parsed["keep_alive"], "-1");
    }

    #[test]
    fn keep_alive_not_injected_on_v1_path() {
        let body = serde_json::json!({"model": "llama3", "messages": []});
        let bytes = serde_json::to_vec(&body).unwrap();
        let result = inject_keep_alive(&bytes, "/v1/chat/completions", "-1");
        let parsed: serde_json::Value = serde_json::from_slice(&result).unwrap();
        assert!(!parsed.as_object().unwrap().contains_key("keep_alive"));
    }

    #[test]
    fn keep_alive_overwrites_existing_client_value() {
        let body = serde_json::json!({"model": "llama3", "prompt": "hi", "keep_alive": "5m"});
        let bytes = serde_json::to_vec(&body).unwrap();
        let result = inject_keep_alive(&bytes, "/api/generate", "-1");
        let parsed: serde_json::Value = serde_json::from_slice(&result).unwrap();
        assert_eq!(parsed["keep_alive"], "-1");
    }

    #[test]
    fn keep_alive_passthrough_on_invalid_json() {
        let bad = b"not json at all";
        let result = inject_keep_alive(bad, "/api/generate", "-1");
        assert_eq!(result.as_ref(), bad.as_ref());
    }
}
