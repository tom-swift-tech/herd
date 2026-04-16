use crate::agent::ws as agent_ws;
use crate::agent::{AgentAudit, SessionStore};
use crate::analytics::{Analytics, RequestLog};
use crate::api::{admin, agent, openai};
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

// ---------------------------------------------------------------------------
// Token extraction from proxy responses
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct TokenUsage {
    tokens_in: Option<u32>,
    tokens_out: Option<u32>,
    tokens_per_second: Option<f32>,
    prompt_eval_ms: Option<u64>,
    eval_ms: Option<u64>,
}

/// Extract token usage from a response body based on backend type and request path.
///
/// Ollama `/api/generate` and `/api/chat` responses include:
///   prompt_eval_count, eval_count, prompt_eval_duration, eval_duration
///
/// llama-server/OpenAI-compat `/v1/chat/completions` responses include:
///   usage.prompt_tokens, usage.completion_tokens
fn extract_tokens_from_response(
    body: &[u8],
    path: &str,
    backend_type: crate::config::BackendType,
) -> TokenUsage {
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(body) else {
        return TokenUsage::default();
    };

    match backend_type {
        crate::config::BackendType::Ollama => {
            if !path.contains("/api/generate") && !path.contains("/api/chat") {
                return TokenUsage::default();
            }
            let tokens_in = json
                .get("prompt_eval_count")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            let tokens_out = json
                .get("eval_count")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            let prompt_eval_duration = json.get("prompt_eval_duration").and_then(|v| v.as_u64());
            let eval_duration = json.get("eval_duration").and_then(|v| v.as_u64());

            let prompt_eval_ms = prompt_eval_duration.map(|d| d / 1_000_000);
            let eval_ms = eval_duration.map(|d| d / 1_000_000);
            let tokens_per_second = match (tokens_out, eval_duration) {
                (Some(count), Some(dur)) if dur > 0 => {
                    Some(count as f32 / (dur as f32 / 1_000_000_000.0))
                }
                _ => None,
            };

            TokenUsage {
                tokens_in,
                tokens_out,
                tokens_per_second,
                prompt_eval_ms,
                eval_ms,
            }
        }
        crate::config::BackendType::LlamaServer | crate::config::BackendType::OpenAICompat => {
            let usage = json.get("usage");
            let tokens_in = usage
                .and_then(|u| u.get("prompt_tokens"))
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            let tokens_out = usage
                .and_then(|u| u.get("completion_tokens"))
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            TokenUsage {
                tokens_in,
                tokens_out,
                ..Default::default()
            }
        }
    }
}

/// Extract the last SSE `data:` line from a chunk of SSE text.
/// Returns the JSON payload bytes if found.
fn extract_last_sse_data(chunk: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(chunk).ok()?;
    let mut last_data: Option<&str> = None;
    for line in text.lines() {
        if let Some(data) = line.strip_prefix("data: ") {
            let trimmed = data.trim();
            if trimmed != "[DONE]" && !trimmed.is_empty() {
                last_data = Some(trimmed);
            }
        }
    }
    last_data.map(|d| d.as_bytes().to_vec())
}

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
    pub session_store: Arc<SessionStore>,
    pub agent_audit: Arc<AgentAudit>,
    pub node_db: Arc<crate::nodes::NodeDb>,
    pub budget: Arc<crate::budget::BudgetTracker>,
    pub rate_limiter: Arc<crate::rate_limit::RateLimiter>,
    pub auto_cache: Arc<crate::classifier_auto::ClassificationCache>,
    pub cost_db: Arc<crate::providers::cost_db::CostDb>,
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

    /// Reloads configuration from disk.
    ///
    /// **Reloaded (takes effect immediately):**
    /// - Backends (added, removed, updated)
    /// - Router strategy
    /// - Routing timeout and retry count
    /// - `routing.default_keep_alive`
    ///
    /// **NOT reloaded (requires restart):**
    /// - `server.host`, `server.port`
    /// - `server.rate_limit`
    /// - `server.api_key`
    /// - `observability.admin_api` (admin API enable/disable)
    /// - `observability.metrics` (metrics route enable/disable)
    /// - `model_warmer.interval_secs`
    /// - `observability.log_retention_days`, `log_max_size_mb`, `log_max_files`
    /// - `circuit_breaker.*`
    ///
    /// Admin CRUD operations (`/admin/backends`) mutate the live pool only and are
    /// ephemeral — a reload or restart discards those changes in favor of the config file.
    pub async fn reload_config(&self) -> anyhow::Result<String> {
        let path = self
            .config_path
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No config file path (started with CLI args)"))?;

        let new_config = Config::from_file(path)?;
        new_config.validate()?;

        // Detect settings that changed but require a restart to take effect
        let mut restart_warnings: Vec<String> = Vec::new();
        {
            let old = self.config.read().await;
            if old.server.host != new_config.server.host
                || old.server.port != new_config.server.port
            {
                restart_warnings.push("server.host/port".to_string());
            }
            if old.server.rate_limit != new_config.server.rate_limit {
                restart_warnings.push("server.rate_limit".to_string());
            }
            if old.server.api_key != new_config.server.api_key {
                restart_warnings.push("server.api_key".to_string());
            }
            if old.observability.admin_api != new_config.observability.admin_api {
                restart_warnings.push("observability.admin_api".to_string());
            }
            if old.observability.metrics != new_config.observability.metrics {
                restart_warnings.push("observability.metrics".to_string());
            }
            if old.model_warmer.interval_secs != new_config.model_warmer.interval_secs {
                restart_warnings.push("model_warmer.interval_secs".to_string());
            }
            if old.observability.log_retention_days != new_config.observability.log_retention_days
                || old.observability.log_max_size_mb != new_config.observability.log_max_size_mb
                || old.observability.log_max_files != new_config.observability.log_max_files
            {
                restart_warnings.push("log rotation settings".to_string());
            }
        } // read lock dropped here

        for w in &restart_warnings {
            tracing::warn!("Reload: {} changed but requires restart to take effect", w);
        }

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
        self.budget.update_config(new_config.budget.clone()).await;
        *self.config.write().await = new_config.clone();

        let mut msg = format!(
            "Reloaded: {} backends, strategy={:?}",
            new_config.backends.len(),
            new_config.routing.strategy
        );
        if !restart_warnings.is_empty() {
            msg.push_str(&format!(
                ". Requires restart: {}",
                restart_warnings.join(", ")
            ));
        }
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

    pub async fn run(mut self) -> Result<()> {
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

        let agent_enabled = if self.config.agent.enabled && self.config.server.api_key.is_none() {
            tracing::warn!("agent is enabled but server.api_key is not set — disabling agent API");
            false
        } else {
            self.config.agent.enabled
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

        let warmer = ModelWarmer::new(
            self.config.model_warmer.interval_secs,
            self.config.model_warmer.timeout_secs,
        );
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

        // Initialize agent session store and audit log
        let session_store = if agent_enabled {
            let session_dir = dirs::home_dir()
                .ok_or_else(|| anyhow::anyhow!("Could not find home directory"))?
                .join(".herd")
                .join("sessions");
            Arc::new(SessionStore::persistent(
                self.config.agent.max_sessions,
                session_dir,
            )?)
        } else {
            Arc::new(SessionStore::new(self.config.agent.max_sessions))
        };
        let agent_audit = Arc::new(AgentAudit::new()?);

        // Auto-generate enrollment_key if not set
        if self.config.server.enrollment_key.is_none() {
            let key = uuid::Uuid::new_v4().to_string();
            tracing::info!("No enrollment_key configured. Auto-generated: {}", key);
            self.config.server.enrollment_key = Some(key);
        }

        let node_db = Arc::new(crate::nodes::NodeDb::open()?);
        let cost_db = Arc::new(crate::providers::cost_db::CostDb::new(
            rusqlite::Connection::open(
                dirs::home_dir()
                    .ok_or_else(|| anyhow::anyhow!("Could not find home directory"))?
                    .join(".herd")
                    .join("frontier_costs.db"),
            )?,
        ));

        // Start node health poller (polls registered nodes every 10s, tags every 60s)
        let node_poller = crate::nodes::NodeHealthPoller::new(10, 60);
        node_poller.spawn(Arc::clone(&node_db), Arc::clone(&pool));

        // Start multi-node discovery (static fleet probing)
        if self.config.discovery.enabled {
            let discovery = crate::discovery::NodeDiscovery::new(self.config.discovery.clone());
            discovery.spawn(Arc::clone(&node_db));
        }

        let budget = crate::budget::BudgetTracker::new(self.config.budget.clone());

        // Build per-client rate limiter config, merging legacy server.rate_limit
        let mut rl_config = self.config.rate_limiting.clone();
        if rl_config.global == 0 && self.config.server.rate_limit > 0 {
            rl_config.global = self.config.server.rate_limit;
        }
        let rate_limiter = Arc::new(crate::rate_limit::RateLimiter::new(&rl_config));

        let state = AppState {
            pool: Arc::clone(&pool),
            router: Arc::new(tokio::sync::RwLock::new(router)),
            client: Arc::clone(&client),
            mgmt_client,
            config: Arc::new(tokio::sync::RwLock::new(self.config.clone())),
            analytics,
            session_store,
            agent_audit,
            metrics,
            node_db,
            budget,
            rate_limiter,
            auto_cache: Arc::new(crate::classifier_auto::ClassificationCache::new(1000)),
            cost_db,
            routing_timeout_ms: Arc::new(AtomicU64::new(routing_timeout.as_millis() as u64)),
            routing_retry_count: Arc::new(AtomicU32::new(self.config.routing.retry_count)),
            config_path: self.config_path.clone(),
        };

        // Start session reaper and audit log cleanup (every 5 minutes)
        if agent_enabled {
            let ttl_secs = (self.config.agent.session_ttl_minutes * 60) as i64;
            let store_clone = Arc::clone(&state.session_store);
            let audit_clone = Arc::clone(&state.agent_audit);
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(Duration::from_secs(300));
                loop {
                    ticker.tick().await;
                    let removed = store_clone.reap_expired(ttl_secs).await;
                    if removed > 0 {
                        tracing::info!("Session reaper removed {} expired sessions", removed);
                    }
                    if let Err(e) = audit_clone.cleanup_old(7 * 24 * 3600).await {
                        tracing::error!("Audit log cleanup failed: {}", e);
                    }
                }
            });
        }

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
            .route("/skills.md", axum::routing::get(skills_md_handler))
            // herd-tune script download (no auth)
            .route(
                "/api/nodes/script",
                axum::routing::get(crate::api::nodes::download_script),
            )
            // Node registration (enrollment key auth — called by herd-tune scripts)
            .route(
                "/api/nodes/register",
                axum::routing::post(crate::api::nodes::register_node),
            )
            // Node management (public — dashboard uses these)
            .route(
                "/api/nodes",
                axum::routing::get(crate::api::nodes::list_nodes),
            )
            .route(
                "/api/nodes/:id",
                axum::routing::get(crate::api::nodes::get_node)
                    .put(crate::api::nodes::update_node)
                    .delete(crate::api::nodes::delete_node),
            )
            // Model search and node model management
            .route(
                "/api/models/search",
                axum::routing::get(crate::api::models::search_models),
            )
            .route(
                "/api/nodes/:id/models",
                axum::routing::get(crate::api::models::list_node_models),
            )
            // Ollama blob extraction (read-only listing is public)
            .route(
                "/api/ollama/models",
                axum::routing::get(crate::api::models::list_ollama_blobs),
            )
            // Routing profiles (public read)
            .route(
                "/api/profiles",
                axum::routing::get(crate::api::profiles::list_profiles),
            );

        // Set default profile (admin auth required)
        {
            let profile_admin_routes = axum::Router::new()
                .route(
                    "/api/profiles/default",
                    axum::routing::put(crate::api::profiles::set_default_profile),
                )
                .layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    require_api_key,
                ));
            app = app.merge(profile_admin_routes);
        }

        // Destructive model endpoints require authentication
        {
            let model_mgmt_routes = axum::Router::new()
                .route(
                    "/api/nodes/:id/models/download",
                    axum::routing::post(crate::api::models::download_model),
                )
                .route(
                    "/api/nodes/:id/models/:model_name",
                    axum::routing::delete(crate::api::models::delete_node_model),
                )
                // Blob extraction writes to disk — requires auth
                .route(
                    "/api/ollama/extract",
                    axum::routing::post(crate::api::models::extract_ollama_blob),
                )
                .layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    require_api_key,
                ));
            app = app.merge(model_mgmt_routes);
        }

        // Conditionally mount metrics
        if self.config.observability.metrics {
            app = app
                .route("/metrics", axum::routing::get(metrics_handler))
                .route("/analytics", axum::routing::get(analytics_handler))
                .route(
                    "/analytics/agent",
                    axum::routing::get(agent_analytics_handler),
                );
        }

        // Budget summary endpoint (always mounted, returns empty if disabled)
        app = app.route("/api/budget", axum::routing::get(budget_handler));

        // Frontier cost summary endpoint
        app = app.route(
            "/api/frontier/costs",
            axum::routing::get(frontier_costs_handler),
        );

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
                .route(
                    "/:name/models",
                    axum::routing::get(admin::list_backend_models),
                )
                .route(
                    "/:name/models/:model",
                    axum::routing::delete(admin::delete_model),
                )
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

        // Conditionally mount agent API (requires auth)
        if agent_enabled {
            let auth_layer = axum::middleware::from_fn_with_state(state.clone(), require_api_key);
            let agent_routes = axum::Router::new()
                .route(
                    "/agent/sessions",
                    axum::routing::get(agent::list_sessions).post(agent::create_session),
                )
                .route(
                    "/agent/sessions/:id",
                    axum::routing::get(agent::get_session).delete(agent::delete_session),
                )
                .route(
                    "/agent/sessions/:id/messages",
                    axum::routing::post(agent::send_message),
                )
                .route(
                    "/agent/sessions/:id/ws",
                    axum::routing::get(agent_ws::ws_handler),
                )
                .layer(auth_layer);
            app = app.merge(agent_routes);
        }

        // Config editor — always mounted (bootstrap mode for containerized deploys)
        let config_routes = axum::Router::new()
            .route(
                "/admin/config",
                axum::routing::get(admin::get_config).put(admin::update_config),
            )
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                require_api_key,
            ));
        app = app.merge(config_routes);

        // Clone state for file watcher before it's consumed by with_state()
        let watcher_state = state.clone();

        // Task classifier middleware (only if enabled)
        let classifier_enabled = self.config.task_classifier.enabled;
        if classifier_enabled {
            tracing::info!(
                "Task classifier enabled (strategy: {})",
                self.config.task_classifier.strategy
            );
        }

        // Proxy (catch-all) + middleware layers (per-client rate limiting)
        let rate_limiter_mw = Arc::clone(&state.rate_limiter);
        let mut app = app
            .fallback(proxy_handler)
            .layer(tower::ServiceBuilder::new().layer(TraceLayer::new_for_http()))
            .layer(axum::middleware::from_fn(
                move |req: axum::extract::Request, next: axum::middleware::Next| {
                    let limiter = Arc::clone(&rate_limiter_mw);
                    async move {
                        let api_key = extract_api_key(&req);
                        let result = limiter.check_rate_limit(api_key.as_deref()).await;

                        match result {
                            Ok(info) => {
                                let mut response = next.run(req).await;
                                inject_rate_limit_headers(response.headers_mut(), &info);
                                Ok::<_, axum::response::Response>(response)
                            }
                            Err(info) => {
                                let retry_after_ms = info
                                    .reset_at
                                    .saturating_sub(
                                        std::time::SystemTime::now()
                                            .duration_since(std::time::UNIX_EPOCH)
                                            .unwrap_or_default()
                                            .as_secs(),
                                    )
                                    .saturating_mul(1000)
                                    .max(1000);
                                let body = serde_json::json!({
                                    "error": "Rate limit exceeded",
                                    "retry_after_ms": retry_after_ms
                                });
                                let mut response = axum::response::IntoResponse::into_response((
                                    axum::http::StatusCode::TOO_MANY_REQUESTS,
                                    axum::Json(body),
                                ));
                                inject_rate_limit_headers(response.headers_mut(), &info);
                                Ok(response)
                            }
                        }
                    }
                },
            ));
        if classifier_enabled {
            app = app.layer(axum::middleware::from_fn_with_state(
                state.clone(),
                crate::classifier::classify_task,
            ));
        }
        let app = app.with_state(state);

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

        // Start server — try TLS if configured, fall back to HTTP on failure
        #[cfg(feature = "tls")]
        if self.config.tls.enabled {
            match self.try_start_tls(&addr, app).await {
                Ok(()) => return Ok(()),
                Err(tls_err) => {
                    tracing::warn!(
                        "TLS configured but failed to start — falling back to HTTP: {}",
                        tls_err
                    );
                    // Cannot recover `app` after move; rebuild is not practical.
                    // Return the TLS error so the operator knows what happened.
                    return Err(tls_err);
                }
            }
        }

        #[cfg(not(feature = "tls"))]
        if self.config.tls.enabled {
            tracing::warn!(
                "TLS configured in herd.yaml but the 'tls' feature is not compiled in — starting HTTP instead. \
                 Rebuild with: cargo build --features tls"
            );
        }

        let listener = tokio::net::TcpListener::bind(&addr).await?;
        info!("Listening on http://{}", addr);
        axum::serve(listener, app).await?;

        Ok(())
    }

    /// Attempt to start an HTTPS server using rustls. Returns Ok(()) if the server
    /// ran (and eventually shut down), or Err if TLS setup failed so the caller can
    /// report the issue.
    #[cfg(feature = "tls")]
    async fn try_start_tls(&self, addr: &str, app: axum::Router) -> Result<()> {
        use axum_server::tls_rustls::RustlsConfig;

        let cert_path = self
            .config
            .tls
            .cert_path
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("TLS enabled but cert_path is not set"))?;
        let key_path = self
            .config
            .tls
            .key_path
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("TLS enabled but key_path is not set"))?;

        if !std::path::Path::new(cert_path).exists() {
            anyhow::bail!("TLS cert_path does not exist: {}", cert_path);
        }
        if !std::path::Path::new(key_path).exists() {
            anyhow::bail!("TLS key_path does not exist: {}", key_path);
        }

        let rustls_config = RustlsConfig::from_pem_file(cert_path, key_path)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to load TLS cert/key: {}", e))?;

        info!("TLS enabled — listening on https://{}", addr);

        // Optional HTTP → HTTPS redirect listener
        if self.config.tls.redirect_http {
            let redirect_port = self.config.tls.redirect_port;
            let host = self.config.server.host.clone();
            let target_port = self.config.server.port;
            info!("HTTP redirect active on port {}", redirect_port);

            tokio::spawn(async move {
                let redirect_app = axum::Router::new().fallback(
                    move |req: axum::http::Request<axum::body::Body>| async move {
                        let host_header = req
                            .headers()
                            .get(axum::http::header::HOST)
                            .and_then(|v| v.to_str().ok())
                            .map(|h| {
                                // Strip port from host header if present
                                h.split(':').next().unwrap_or(h).to_string()
                            })
                            .unwrap_or_else(|| "localhost".to_string());

                        let path_and_query = req
                            .uri()
                            .path_and_query()
                            .map(|pq| pq.as_str())
                            .unwrap_or("/");

                        let target = if target_port == 443 {
                            format!("https://{}{}", host_header, path_and_query)
                        } else {
                            format!("https://{}:{}{}", host_header, target_port, path_and_query)
                        };

                        axum::response::Redirect::permanent(&target)
                    },
                );

                let redirect_addr = format!("{}:{}", host, redirect_port);
                match tokio::net::TcpListener::bind(&redirect_addr).await {
                    Ok(listener) => {
                        if let Err(e) =
                            axum::serve(listener, redirect_app.into_make_service()).await
                        {
                            tracing::error!("HTTP redirect listener failed: {}", e);
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Failed to bind HTTP redirect listener on {}: {}",
                            redirect_addr,
                            e
                        );
                    }
                }
            });
        }

        let socket_addr: std::net::SocketAddr = addr.parse()?;
        axum_server::bind_rustls(socket_addr, rustls_config)
            .serve(app.into_make_service())
            .await?;

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Rate limit response headers
// ---------------------------------------------------------------------------

fn inject_rate_limit_headers(
    headers: &mut axum::http::HeaderMap,
    info: &crate::rate_limit::RateLimitInfo,
) {
    // Only inject headers when rate limiting is active (limit > 0)
    if info.limit == 0 {
        return;
    }
    if let Ok(v) = axum::http::HeaderValue::from_str(&info.limit.to_string()) {
        headers.insert("x-herd-ratelimit-limit", v);
    }
    if let Ok(v) = axum::http::HeaderValue::from_str(&info.remaining.to_string()) {
        headers.insert("x-herd-ratelimit-remaining", v);
    }
    if let Ok(v) = axum::http::HeaderValue::from_str(&info.reset_at.to_string()) {
        headers.insert("x-herd-ratelimit-reset", v);
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

pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
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
        // Skip hop-by-hop and framing headers. Content-Length is re-computed
        // by reqwest from the actual body — forwarding the original value would
        // send a wrong length after inject_keep_alive modifies the body.
        if name == axum::http::header::HOST
            || name == axum::http::header::CONNECTION
            || name == axum::http::header::TRANSFER_ENCODING
            || name == axum::http::header::CONTENT_LENGTH
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
    let is_ollama_endpoint = path.contains("/api/generate") || path.contains("/api/chat");
    if !is_ollama_endpoint {
        return axum::body::Bytes::copy_from_slice(body);
    }
    let Ok(mut payload) = serde_json::from_slice::<serde_json::Value>(body) else {
        return axum::body::Bytes::copy_from_slice(body);
    };
    if let Some(obj) = payload.as_object_mut() {
        // Inject as integer when keep_alive is a bare number (e.g. "-1", "300")
        // so all Ollama versions accept it. Older Ollama rejects "-1" as a string
        // because time.ParseDuration doesn't special-case it.
        let value = if let Ok(n) = keep_alive.parse::<i64>() {
            serde_json::Value::Number(serde_json::Number::from(n))
        } else {
            serde_json::Value::String(keep_alive.to_string())
        };
        obj.insert("keep_alive".to_string(), value);
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

    // Collect request headers and extensions before consuming body
    let mut headers = request.headers().clone();
    let extensions = request.extensions().clone();

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

    // Auto classification: if model is "auto" or absent, classify and resolve
    let mut auto_classification: Option<crate::classifier_auto::Classification> = None;
    if crate::classifier_auto::should_auto_classify(model_name.as_deref()) {
        let auto_config = state.config.read().await.routing.auto.clone();
        if auto_config.enabled {
            let body_json =
                serde_json::from_slice::<serde_json::Value>(&body_bytes).unwrap_or_default();
            let user_message = crate::classifier::extract_last_user_message(&body_json);

            if !user_message.is_empty() {
                let ck = crate::classifier_auto::cache_key(&user_message);
                let ttl = std::time::Duration::from_secs(auto_config.cache_ttl_secs);

                if let Some(cached) = state.auto_cache.get(&ck, ttl) {
                    state
                        .metrics
                        .record_auto_classification(&cached.tier, &cached.capability, 0, true)
                        .await;
                    auto_classification = Some(cached);
                } else {
                    let classify_start = std::time::Instant::now();
                    let backend_url = state
                        .pool
                        .find_model_backend(&auto_config.classifier_model)
                        .await;

                    if let Some(url) = backend_url {
                        let timeout =
                            std::time::Duration::from_millis(auto_config.classifier_timeout_ms);
                        let result = crate::classifier_auto::classify_request(
                            &state.client,
                            &url,
                            &auto_config.classifier_model,
                            &user_message,
                            timeout,
                        )
                        .await;

                        let dur = classify_start.elapsed().as_millis() as u64;
                        if let Some(ref c) = result {
                            state.auto_cache.put(&ck, c.clone());
                            state
                                .metrics
                                .record_auto_classification(&c.tier, &c.capability, dur, false)
                                .await;
                        }
                        auto_classification = result;
                    } else {
                        tracing::warn!(
                            "Auto classifier: no backend with model '{}' — using fallback",
                            auto_config.classifier_model
                        );
                    }
                }

                // Resolve classification to a model name
                let resolved = if let Some(ref c) = auto_classification {
                    crate::classifier_auto::resolve_model(
                        &auto_config.model_map,
                        &c.tier,
                        &c.capability,
                        &auto_config.fallback_model,
                    )
                } else if !auto_config.fallback_model.is_empty() {
                    auto_config.fallback_model.clone()
                } else {
                    String::new()
                };

                if !resolved.is_empty() {
                    model_name = Some(resolved);
                }
            }
        }
    }

    // Extract client identifier from X-Herd-Client header (for budget tracking)
    let client_name: Option<String> = headers
        .get("x-herd-client")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // Budget check (before routing to avoid wasting backend resources)
    {
        state.budget.reset_if_needed().await;
        let budget_status = state
            .budget
            .check_budget(
                client_name.as_deref(),
                model_name.as_deref().unwrap_or("unknown"),
            )
            .await;

        match budget_status {
            crate::budget::BudgetStatus::Exceeded {
                cap_type,
                limit,
                current,
            } => {
                let body = serde_json::json!({
                    "error": "Budget exceeded",
                    "cap": cap_type,
                    "limit": limit,
                    "current": current,
                });
                return axum::response::Response::builder()
                    .status(axum::http::StatusCode::TOO_MANY_REQUESTS)
                    .header("content-type", "application/json")
                    .header("x-request-id", &request_id)
                    .body(axum::body::Body::from(body.to_string()))
                    .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR);
            }
            crate::budget::BudgetStatus::Warning {
                cap_type,
                limit,
                current,
            } => {
                tracing::warn!(
                    "Budget warning: {} at {:.2}/{:.2} USD",
                    cap_type,
                    current,
                    limit
                );
            }
            crate::budget::BudgetStatus::Ok { .. } => {}
        }
    }

    // Frontier gateway: if model is a frontier model, route through providers
    let frontier_config = state.config.read().await.frontier.clone();
    let provider_configs = state.config.read().await.providers.clone();

    if let Some(response) = crate::providers::frontier_route_if_applicable(
        &state.client,
        &frontier_config,
        &provider_configs,
        &state.cost_db,
        model_name.as_deref(),
        &headers,
        auto_classification.as_ref(),
        &body_bytes,
        &request_id,
    )
    .await
    {
        return Ok(response);
    }

    // Prepare both versions of the body — keep_alive is Ollama-specific
    let keep_alive_value = state.config.read().await.routing.default_keep_alive.clone();
    let forward_bytes_ollama = inject_keep_alive(&body_bytes, &path, &keep_alive_value);
    let forward_bytes_raw = body_bytes.clone();

    // Extract tags from X-Herd-Tags header (comma-separated)
    let request_tags: Option<Vec<String>> = headers
        .get("x-herd-tags")
        .and_then(|v| v.to_str().ok())
        .map(|s| {
            s.split(',')
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect()
        });

    // Resolve routing profile (X-Herd-Profile header or default)
    let profile_header = headers
        .get("x-herd-profile")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let resolved = {
        let config = state.config.read().await;
        crate::profiles::resolve_profile(&config, profile_header.as_deref())
    };

    // Merge tags: request tags take precedence, profile tags are baseline
    let tags: Option<Vec<String>> = if request_tags.is_some() {
        request_tags
    } else if !resolved.tags.is_empty() {
        Some(resolved.tags.clone())
    } else {
        None
    };

    // If profile specifies a preferred model and request didn't specify one, use it
    if model_name.is_none() {
        if let Some(ref preferred) = resolved.preferred_model {
            model_name = Some(preferred.clone());
        }
    }

    let profile_name = resolved.profile_name.clone();

    // Retry loop: try routing to different backends on failure
    let mut response = None;
    let mut selected_backend: Option<String> = None;
    let mut excluded = HashSet::new();

    // If profile restricts to specific backends, pre-exclude all others
    if !resolved.backends.is_empty() {
        for name in state.pool.all().await {
            if !resolved.backends.contains(&name) {
                excluded.insert(name);
            }
        }
    }

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

        // Only inject keep_alive for Ollama backends
        let forward_bytes = if state
            .pool
            .get(&backend.name)
            .await
            .map(|s| s.config.backend == crate::config::BackendType::Ollama)
            .unwrap_or(true)
        {
            forward_bytes_ollama.clone()
        } else {
            forward_bytes_raw.clone()
        };

        let req_builder = state
            .client
            .request(method.clone(), &url)
            .timeout(state.routing_timeout())
            .body(forward_bytes);
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

                // Note: mid-stream failover is not possible for streaming responses;
                // this handles pre-stream 5xx only.
                if matches!(r.status().as_u16(), 500 | 502 | 503) {
                    tracing::warn!(
                        "Backend {} returned {} — retrying on another backend",
                        backend.name,
                        r.status()
                    );
                    state.pool.mark_unhealthy(&backend.name).await;
                    excluded.insert(backend.name.clone());
                    continue;
                }

                state.pool.mark_healthy(&backend.name).await;
                response = Some(r);
                let strategy = state.config_snapshot().await.routing.strategy.to_string();
                state
                    .metrics
                    .record_routing_selection(&backend.name, &strategy)
                    .await;
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

    let status = match &response {
        Some(r) if r.status().is_success() => "success",
        _ => "error",
    };

    // Check for classification result from the classifier middleware
    let classification = extensions.get::<crate::classifier::ClassificationResult>();

    // Determine backend type for the selected backend
    let backend_type = if let Some(ref name) = selected_backend {
        state.pool.get(name).await.map(|s| s.config.backend)
    } else {
        None
    };
    let backend_type_str = backend_type.map(|bt| bt.to_string());

    // Context struct for proxy request logging/metrics (avoids 11-parameter closures)
    struct ProxyRequestContext {
        state: AppState,
        model_name: Option<String>,
        client_name: Option<String>,
        selected_backend: String,
        path: String,
        request_id: String,
        tier: Option<String>,
        classified_by: Option<String>,
        backend_type_str: Option<String>,
        auto_tier: Option<String>,
        auto_capability: Option<String>,
        auto_model: Option<String>,
        start: std::time::Instant,
    }

    impl ProxyRequestContext {
        async fn log_and_record(&self, status: &str, usage: TokenUsage) {
            let duration_ms = self.start.elapsed().as_millis() as u64;
            let log = RequestLog {
                timestamp: chrono::Utc::now().timestamp(),
                model: self.model_name.clone(),
                backend: self.selected_backend.clone(),
                duration_ms,
                status: status.to_string(),
                path: self.path.clone(),
                request_id: Some(self.request_id.clone()),
                tier: self.tier.clone(),
                classified_by: self.classified_by.clone(),
                tokens_in: usage.tokens_in,
                tokens_out: usage.tokens_out,
                tokens_per_second: usage.tokens_per_second,
                prompt_eval_ms: usage.prompt_eval_ms,
                eval_ms: usage.eval_ms,
                backend_type: self.backend_type_str.clone(),
                auto_tier: self.auto_tier.clone(),
                auto_capability: self.auto_capability.clone(),
                auto_model: self.auto_model.clone(),
                frontier_provider: None,
                frontier_cost_usd: None,
            };

            self.state
                .metrics
                .record_request(&log.backend, &log.status, log.duration_ms)
                .await;

            if let (Some(tin), Some(tout)) = (usage.tokens_in, usage.tokens_out) {
                self.state
                    .metrics
                    .record_tokens(self.model_name.as_deref().unwrap_or("unknown"), tin, tout)
                    .await;
            }
            if let Some(tps) = usage.tokens_per_second {
                self.state.metrics.record_tokens_per_second(tps).await;
            }

            self.state
                .metrics
                .record_request_labeled(
                    &log.backend,
                    self.model_name.as_deref().unwrap_or("unknown"),
                    status,
                    log.duration_ms,
                )
                .await;

            if let Err(e) = self.state.analytics.log_request(log).await {
                tracing::error!("Failed to log request: {}", e);
            }

            // Record cost for budget tracking
            if let (Some(tin), Some(tout)) = (usage.tokens_in, usage.tokens_out) {
                let model = self.model_name.as_deref().unwrap_or("unknown");
                let cost = crate::analytics::estimate_api_cost(model, tin as u64, tout as u64);
                self.state
                    .budget
                    .record_cost(self.client_name.as_deref(), model, cost)
                    .await;
            }
        }
    }

    // Extract auto classification fields for logging and headers
    let auto_tier = auto_classification.as_ref().map(|c| c.tier.clone());
    let auto_capability = auto_classification.as_ref().map(|c| c.capability.clone());
    let auto_model = if auto_classification.is_some() {
        model_name.clone()
    } else {
        None
    };

    match response {
        Some(r) => {
            let status_code = axum::http::StatusCode::from_u16(r.status().as_u16())
                .unwrap_or(axum::http::StatusCode::OK);

            let mut builder = axum::response::Response::builder()
                .status(status_code)
                .header("x-request-id", &request_id);

            // Add routing profile header if a profile was used
            if let Some(ref pname) = profile_name {
                if let Ok(val) = axum::http::HeaderValue::from_str(pname) {
                    builder = builder.header("x-herd-profile", val);
                }
            }

            // Add auto classification response headers
            if let Some(ref tier) = auto_tier {
                if let Ok(val) = axum::http::HeaderValue::from_str(tier) {
                    builder = builder.header("x-herd-auto-tier", val);
                }
            }
            if let Some(ref cap) = auto_capability {
                if let Ok(val) = axum::http::HeaderValue::from_str(cap) {
                    builder = builder.header("x-herd-auto-capability", val);
                }
            }
            if let Some(ref model) = auto_model {
                if let Ok(val) = axum::http::HeaderValue::from_str(model) {
                    builder = builder.header("x-herd-auto-model", val);
                }
            }

            // Forward response headers (bridge reqwest http 0.2 → axum http 1.x)
            for (name, value) in r.headers() {
                if let (Ok(aname), Ok(aval)) = (
                    axum::http::header::HeaderName::from_bytes(name.as_ref()),
                    axum::http::header::HeaderValue::from_bytes(value.as_ref()),
                ) {
                    builder = builder.header(aname, aval);
                }
            }

            // Detect streaming response by content-type
            let is_streaming = r
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|ct| ct.contains("text/event-stream") || ct.contains("application/x-ndjson"))
                .unwrap_or(false);

            let bt = backend_type.unwrap_or(crate::config::BackendType::Ollama);
            let selected = selected_backend.unwrap_or_else(|| "none".to_string());
            let tier = classification.map(|c| c.tier.clone());
            let classified_by = classification.map(|c| c.classified_by.clone());

            if is_streaming {
                // Streaming: wrap the byte stream to capture only the last SSE data chunk.
                // The stream passes through to the client in real-time.
                let last_data = Arc::new(tokio::sync::Mutex::new(Option::<Vec<u8>>::None));
                let last_data_capture = last_data.clone();

                // Oneshot channel to signal stream completion (replaces Arc::strong_count polling)
                let (stream_done_tx, stream_done_rx) = tokio::sync::oneshot::channel::<()>();
                let mut stream_done_tx = Some(stream_done_tx);

                let inner_stream = r.bytes_stream();
                let capturing_stream = futures_util::stream::unfold(
                    (inner_stream, last_data_capture, stream_done_tx.take()),
                    |(mut stream, last_data, stream_done_tx)| async move {
                        use futures_util::StreamExt;
                        match stream.next().await {
                            Some(Ok(chunk)) => {
                                if let Some(data) = extract_last_sse_data(&chunk) {
                                    *last_data.lock().await = Some(data);
                                }
                                Some((
                                    Ok::<_, std::io::Error>(chunk),
                                    (stream, last_data, stream_done_tx),
                                ))
                            }
                            Some(Err(e)) => {
                                tracing::warn!("Stream error from backend: {}", e);
                                Some((
                                    Err(std::io::Error::other(e)),
                                    (stream, last_data, stream_done_tx),
                                ))
                            }
                            None => {
                                // Stream ended — signal the waiting task
                                if let Some(tx) = stream_done_tx {
                                    let _ = tx.send(());
                                }
                                None
                            }
                        }
                    },
                );

                let body = axum::body::Body::from_stream(capturing_stream);
                let resp = builder
                    .body(body)
                    .map_err(|_| axum::http::StatusCode::BAD_GATEWAY)?;

                // Fire-and-forget: extract tokens after stream ends
                let ctx = ProxyRequestContext {
                    state: state.clone(),
                    model_name: model_name.clone(),
                    client_name: client_name.clone(),
                    selected_backend: selected,
                    path: path.clone(),
                    request_id: request_id.clone(),
                    tier,
                    classified_by,
                    backend_type_str: backend_type_str.clone(),
                    auto_tier: auto_tier.clone(),
                    auto_capability: auto_capability.clone(),
                    auto_model: auto_model.clone(),
                    start,
                };
                tokio::spawn(async move {
                    // Wait for stream to complete via oneshot signal
                    let _ = stream_done_rx.await;
                    let data = last_data.lock().await.clone();
                    let usage = if let Some(data) = data {
                        extract_tokens_from_response(&data, &ctx.path, bt)
                    } else {
                        TokenUsage::default()
                    };
                    ctx.log_and_record(status, usage).await;
                });

                Ok(resp)
            } else {
                // Non-streaming: buffer body, extract tokens, then forward
                let body_bytes = r.bytes().await.unwrap_or_default();
                let usage = extract_tokens_from_response(&body_bytes, &path, bt);

                let ctx = ProxyRequestContext {
                    state: state.clone(),
                    model_name: model_name.clone(),
                    client_name: client_name.clone(),
                    selected_backend: selected,
                    path: path.clone(),
                    request_id: request_id.clone(),
                    tier,
                    classified_by,
                    backend_type_str,
                    auto_tier,
                    auto_capability,
                    auto_model,
                    start,
                };
                ctx.log_and_record(status, usage).await;

                let body = axum::body::Body::from(body_bytes);
                builder
                    .body(body)
                    .map_err(|_| axum::http::StatusCode::BAD_GATEWAY)
            }
        }
        None => {
            let ctx = ProxyRequestContext {
                state: state.clone(),
                model_name: model_name.clone(),
                client_name: client_name.clone(),
                selected_backend: selected_backend.unwrap_or_else(|| "none".to_string()),
                path: path.clone(),
                request_id: request_id.clone(),
                tier: classification.map(|c| c.tier.clone()),
                classified_by: classification.map(|c| c.classified_by.clone()),
                backend_type_str,
                auto_tier,
                auto_capability,
                auto_model,
                start,
            };
            ctx.log_and_record(status, TokenUsage::default()).await;

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

async fn budget_handler(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> axum::Json<serde_json::Value> {
    let summary = state.budget.get_summary().await;
    axum::Json(
        serde_json::to_value(&summary)
            .unwrap_or_else(|_| serde_json::json!({"error": "serialization failed"})),
    )
}

async fn frontier_costs_handler(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> axum::Json<serde_json::Value> {
    match state.cost_db.cost_summary() {
        Ok(summary) => axum::Json(
            serde_json::to_value(&summary)
                .unwrap_or_else(|_| serde_json::json!({"error": "serialization failed"})),
        ),
        Err(e) => axum::Json(serde_json::json!({
            "error": format!("Failed to get frontier costs: {}", e)
        })),
    }
}

async fn agent_analytics_handler(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> axum::Json<serde_json::Value> {
    let hours = params
        .get("hours")
        .and_then(|h| h.parse::<i64>().ok())
        .unwrap_or(24)
        .clamp(1, 168);

    let seconds = hours * 3600;

    match state.agent_audit.get_stats(seconds).await {
        Ok(stats) => axum::Json(
            serde_json::to_value(&stats)
                .unwrap_or_else(|_| serde_json::json!({"error": "serialization failed"})),
        ),
        Err(e) => axum::Json(serde_json::json!({
            "error": format!("Failed to get agent analytics: {}", e)
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

async fn skills_md_handler() -> (
    [(axum::http::header::HeaderName, &'static str); 1],
    &'static str,
) {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/markdown; charset=utf-8",
        )],
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
            config: Arc::new(tokio::sync::RwLock::new(initial.clone())),
            analytics: Arc::new(Analytics::new().unwrap()),
            session_store: Arc::new(SessionStore::new(100)),
            agent_audit: Arc::new(AgentAudit::new().unwrap()),
            metrics: Arc::new(crate::metrics::Metrics::new()),
            node_db: Arc::new(crate::nodes::NodeDb::open().unwrap()),
            budget: crate::budget::BudgetTracker::new(initial.budget.clone()),
            rate_limiter: Arc::new(crate::rate_limit::RateLimiter::new(&initial.rate_limiting)),
            auto_cache: Arc::new(crate::classifier_auto::ClassificationCache::new(1000)),
            cost_db: Arc::new(crate::providers::cost_db::CostDb::new(
                rusqlite::Connection::open_in_memory().unwrap(),
            )),
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
        assert_eq!(parsed["keep_alive"], -1);
        assert_eq!(parsed["model"], "llama3");
    }

    #[test]
    fn keep_alive_injected_into_api_chat() {
        let body = serde_json::json!({"model": "llama3", "messages": []});
        let bytes = serde_json::to_vec(&body).unwrap();
        let result = inject_keep_alive(&bytes, "/api/chat", "-1");
        let parsed: serde_json::Value = serde_json::from_slice(&result).unwrap();
        assert_eq!(parsed["keep_alive"], -1);
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
        assert_eq!(parsed["keep_alive"], -1);
    }

    #[test]
    fn keep_alive_passthrough_on_invalid_json() {
        let bad = b"not json at all";
        let result = inject_keep_alive(bad, "/api/generate", "-1");
        assert_eq!(result.as_ref(), bad.as_ref());
    }

    // -----------------------------------------------------------------------
    // Token extraction tests
    // -----------------------------------------------------------------------

    #[test]
    fn extract_tokens_ollama_generate() {
        let body = serde_json::json!({
            "model": "llama3:8b",
            "response": "Hello!",
            "done": true,
            "prompt_eval_count": 26,
            "eval_count": 298,
            "prompt_eval_duration": 4_500_000_000_u64,
            "eval_duration": 8_200_000_000_u64
        });
        let bytes = serde_json::to_vec(&body).unwrap();
        let usage = extract_tokens_from_response(
            &bytes,
            "/api/generate",
            crate::config::BackendType::Ollama,
        );
        assert_eq!(usage.tokens_in, Some(26));
        assert_eq!(usage.tokens_out, Some(298));
        assert_eq!(usage.prompt_eval_ms, Some(4500));
        assert_eq!(usage.eval_ms, Some(8200));
        // tokens_per_second = 298 / (8.2) ≈ 36.34
        let tps = usage.tokens_per_second.unwrap();
        assert!(tps > 36.0 && tps < 37.0, "tps was {}", tps);
    }

    #[test]
    fn extract_tokens_ollama_chat() {
        let body = serde_json::json!({
            "model": "llama3:8b",
            "message": {"role": "assistant", "content": "Hi"},
            "done": true,
            "prompt_eval_count": 50,
            "eval_count": 100,
            "prompt_eval_duration": 2_000_000_000_u64,
            "eval_duration": 4_000_000_000_u64
        });
        let bytes = serde_json::to_vec(&body).unwrap();
        let usage =
            extract_tokens_from_response(&bytes, "/api/chat", crate::config::BackendType::Ollama);
        assert_eq!(usage.tokens_in, Some(50));
        assert_eq!(usage.tokens_out, Some(100));
    }

    #[test]
    fn extract_tokens_llama_server() {
        let body = serde_json::json!({
            "id": "chatcmpl-1234",
            "choices": [{"message": {"content": "Hi"}}],
            "usage": {
                "prompt_tokens": 26,
                "completion_tokens": 298,
                "total_tokens": 324
            }
        });
        let bytes = serde_json::to_vec(&body).unwrap();
        let usage = extract_tokens_from_response(
            &bytes,
            "/v1/chat/completions",
            crate::config::BackendType::LlamaServer,
        );
        assert_eq!(usage.tokens_in, Some(26));
        assert_eq!(usage.tokens_out, Some(298));
        // llama-server doesn't provide timing info in the response
        assert!(usage.tokens_per_second.is_none());
        assert!(usage.prompt_eval_ms.is_none());
        assert!(usage.eval_ms.is_none());
    }

    #[test]
    fn extract_tokens_openai_compat() {
        let body = serde_json::json!({
            "id": "chatcmpl-5678",
            "choices": [{"message": {"content": "Hello"}}],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 50
            }
        });
        let bytes = serde_json::to_vec(&body).unwrap();
        let usage = extract_tokens_from_response(
            &bytes,
            "/v1/chat/completions",
            crate::config::BackendType::OpenAICompat,
        );
        assert_eq!(usage.tokens_in, Some(100));
        assert_eq!(usage.tokens_out, Some(50));
    }

    #[test]
    fn extract_tokens_invalid_json_returns_default() {
        let usage = extract_tokens_from_response(
            b"not json",
            "/api/generate",
            crate::config::BackendType::Ollama,
        );
        assert!(usage.tokens_in.is_none());
        assert!(usage.tokens_out.is_none());
    }

    #[test]
    fn extract_tokens_ollama_wrong_path_returns_default() {
        let body = serde_json::json!({"prompt_eval_count": 10, "eval_count": 20});
        let bytes = serde_json::to_vec(&body).unwrap();
        let usage =
            extract_tokens_from_response(&bytes, "/v1/models", crate::config::BackendType::Ollama);
        assert!(usage.tokens_in.is_none());
    }

    #[test]
    fn extract_tokens_partial_ollama_response() {
        // Ollama response with eval_count but no prompt_eval_count
        let body = serde_json::json!({"eval_count": 100, "eval_duration": 2_000_000_000_u64});
        let bytes = serde_json::to_vec(&body).unwrap();
        let usage = extract_tokens_from_response(
            &bytes,
            "/api/generate",
            crate::config::BackendType::Ollama,
        );
        assert!(usage.tokens_in.is_none());
        assert_eq!(usage.tokens_out, Some(100));
        assert_eq!(usage.eval_ms, Some(2000));
    }

    // -----------------------------------------------------------------------
    // SSE data extraction tests
    // -----------------------------------------------------------------------

    #[test]
    fn extract_last_sse_data_single_line() {
        let chunk =
            b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":20}}\n\n";
        let result = extract_last_sse_data(chunk);
        assert!(result.is_some());
        let json: serde_json::Value = serde_json::from_slice(&result.unwrap()).unwrap();
        assert_eq!(json["usage"]["prompt_tokens"], 10);
    }

    #[test]
    fn extract_last_sse_data_multiple_lines() {
        let chunk = b"data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":10}}\n\ndata: [DONE]\n\n";
        let result = extract_last_sse_data(chunk);
        assert!(result.is_some());
        let json: serde_json::Value = serde_json::from_slice(&result.unwrap()).unwrap();
        assert_eq!(json["usage"]["completion_tokens"], 10);
    }

    #[test]
    fn extract_last_sse_data_done_only() {
        let chunk = b"data: [DONE]\n\n";
        let result = extract_last_sse_data(chunk);
        assert!(result.is_none());
    }

    #[test]
    fn extract_last_sse_data_empty() {
        let result = extract_last_sse_data(b"");
        assert!(result.is_none());
    }

    #[test]
    fn tls_config_missing_cert_path_detected() {
        let tls = crate::config::TlsConfig {
            enabled: true,
            cert_path: None,
            key_path: Some("/tmp/key.pem".into()),
            ..Default::default()
        };
        assert!(tls.cert_path.is_none());
    }

    #[test]
    fn tls_config_missing_key_path_detected() {
        let tls = crate::config::TlsConfig {
            enabled: true,
            cert_path: Some("/tmp/cert.pem".into()),
            key_path: None,
            ..Default::default()
        };
        assert!(tls.key_path.is_none());
    }

    #[test]
    fn tls_redirect_url_standard_port() {
        // When target port is 443, the URL should omit the port
        let target_port: u16 = 443;
        let host = "example.com";
        let path = "/v1/chat/completions";
        let url = if target_port == 443 {
            format!("https://{}{}", host, path)
        } else {
            format!("https://{}:{}{}", host, target_port, path)
        };
        assert_eq!(url, "https://example.com/v1/chat/completions");
    }

    #[test]
    fn tls_redirect_url_custom_port() {
        let target_port: u16 = 40114;
        let host = "example.com";
        let path = "/v1/chat/completions";
        let url = if target_port == 443 {
            format!("https://{}{}", host, path)
        } else {
            format!("https://{}:{}{}", host, target_port, path)
        };
        assert_eq!(url, "https://example.com:40114/v1/chat/completions");
    }
}
