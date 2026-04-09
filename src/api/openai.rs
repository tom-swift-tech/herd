use crate::router::Router;
use crate::server::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde_json::{json, Value};
use std::collections::HashSet;

/// GET /v1/models — OpenAI-compatible model listing.
/// Aggregates unique model names from all healthy backends.
pub async fn list_models(State(state): State<AppState>) -> Json<Value> {
    let mut seen = std::collections::HashSet::new();
    let mut models = Vec::new();

    for name in state.pool.all_healthy().await {
        if let Some(backend) = state.pool.get(&name).await {
            for model in &backend.models {
                if seen.insert(model.clone()) {
                    models.push(json!({
                        "id": model,
                        "object": "model",
                        "created": 0,
                        "owned_by": "ollama",
                    }));
                }
            }
        }
    }

    Json(json!({ "object": "list", "data": models }))
}

/// POST /v1/chat/completions — OpenAI-compatible chat completions.
/// Extracts the model, routes to the correct backend, and proxies the request.
/// Returns OpenAI-format errors on failure.
pub async fn chat_completions(
    State(state): State<AppState>,
    request: axum::extract::Request,
) -> Result<axum::response::Response, (StatusCode, Json<Value>)> {
    let start = std::time::Instant::now();

    let (parts, body) = request.into_parts();
    let mut headers = parts.headers.clone();

    // Get or generate correlation ID
    let request_id = headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            let id = uuid::Uuid::new_v4().to_string();
            if let Ok(val) = axum::http::HeaderValue::from_str(&id) {
                headers.insert("x-request-id", val);
            }
            id
        });

    // Read body with size cap (10 MB)
    let body_bytes = axum::body::to_bytes(body, 10 * 1024 * 1024)
        .await
        .map_err(|_| openai_error(StatusCode::PAYLOAD_TOO_LARGE, "Request body too large"))?;

    // Extract model and streaming flag from body
    let request_json = serde_json::from_slice::<Value>(&body_bytes).ok();
    let model_name = request_json
        .as_ref()
        .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(String::from));
    let is_streaming = request_json
        .as_ref()
        .and_then(|v| v.get("stream").and_then(|s| s.as_bool()))
        .unwrap_or(false);

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

    // Preserve query string if present
    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| "/v1/chat/completions".to_string());

    let method = reqwest::Method::from_bytes(parts.method.as_str().as_bytes())
        .unwrap_or(reqwest::Method::POST);
    let mut response = None;
    let mut selected_backend = None;
    let mut excluded = HashSet::new();

    for _ in 0..=state.retry_count() {
        let backend = state
            .router
            .read()
            .await
            .route_excluding(model_name.as_deref(), tags.as_deref(), &excluded)
            .await
            .map_err(|_| {
                openai_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    &format!(
                        "No healthy backend available{}",
                        model_name
                            .as_deref()
                            .map(|m| format!(" for model '{}'", m))
                            .unwrap_or_default()
                    ),
                )
            })?;

        state.pool.touch_request(&backend.name).await;
        selected_backend = Some(backend.name.clone());

        let url = format!("{}{}", backend.url.trim_end_matches('/'), path_and_query);
        let mut req_builder = state
            .client
            .request(method.clone(), &url)
            .timeout(state.routing_timeout())
            .body(body_bytes.clone());

        for (name, value) in &headers {
            if name == axum::http::header::HOST || name == axum::http::header::CONTENT_LENGTH {
                continue;
            }
            if let (Ok(rn), Ok(rv)) = (
                reqwest::header::HeaderName::from_bytes(name.as_ref()),
                reqwest::header::HeaderValue::from_bytes(value.as_ref()),
            ) {
                req_builder = req_builder.header(rn, rv);
            }
        }

        match req_builder.send().await {
            Ok(r) => {
                if r.status().as_u16() == 404 {
                    tracing::warn!(
                        "Backend {} returned 404 for /v1/chat/completions, retrying",
                        backend.name
                    );
                    excluded.insert(backend.name.clone());
                    continue;
                }

                if matches!(r.status().as_u16(), 500 | 502 | 503) {
                    tracing::warn!(
                        "Backend {} returned {} for /v1/chat/completions — retrying on another backend",
                        backend.name, r.status()
                    );
                    state.pool.mark_unhealthy(&backend.name).await;
                    excluded.insert(backend.name.clone());
                    continue;
                }

                state.pool.mark_healthy(&backend.name).await;
                response = Some(r);
                break;
            }
            Err(e) => {
                tracing::error!("Upstream request to {} failed: {}", url, e);
                state.pool.mark_unhealthy(&backend.name).await;
                excluded.insert(backend.name.clone());
            }
        }
    }

    let response = match response {
        Some(response) => response,
        None => {
            let duration = start.elapsed();
            let log = crate::analytics::RequestLog {
                timestamp: chrono::Utc::now().timestamp(),
                model: model_name,
                backend: selected_backend.unwrap_or_else(|| "none".to_string()),
                duration_ms: duration.as_millis() as u64,
                status: "error".to_string(),
                path: "/v1/chat/completions".to_string(),
                request_id: Some(request_id.clone()),
                tier: None,
                classified_by: None,
                tokens_in: None,
                tokens_out: None,
                tokens_per_second: None,
                prompt_eval_ms: None,
                eval_ms: None,
                backend_type: None,
                auto_tier: None,
                auto_capability: None,
                auto_model: None,
            };
            state
                .metrics
                .record_request(&log.backend, &log.status, log.duration_ms)
                .await;
            let _ = state.analytics.log_request(log).await;

            return Err(openai_error(
                StatusCode::BAD_GATEWAY,
                "All candidate backends failed for /v1/chat/completions",
            ));
        }
    };

    let duration = start.elapsed();
    let status = if response.status().is_success() {
        "success"
    } else {
        "error"
    };
    let backend_name = selected_backend.unwrap_or_else(|| "none".to_string());

    // Bridge response back (reqwest → axum)
    let status_code = axum::http::StatusCode::from_u16(response.status().as_u16())
        .unwrap_or(axum::http::StatusCode::OK);

    let mut builder = axum::response::Response::builder()
        .status(status_code)
        .header("x-request-id", &request_id);
    for (name, value) in response.headers() {
        if let (Ok(an), Ok(av)) = (
            axum::http::HeaderName::from_bytes(name.as_ref()),
            axum::http::HeaderValue::from_bytes(value.as_ref()),
        ) {
            builder = builder.header(an, av);
        }
    }

    if is_streaming {
        // Streaming: pass through as-is, token extraction from SSE not yet supported
        let log = crate::analytics::RequestLog {
            timestamp: chrono::Utc::now().timestamp(),
            model: model_name,
            backend: backend_name.clone(),
            duration_ms: duration.as_millis() as u64,
            status: status.to_string(),
            path: "/v1/chat/completions".to_string(),
            request_id: Some(request_id.clone()),
            tier: None,
            classified_by: None,
            tokens_in: None,
            tokens_out: None,
            tokens_per_second: None,
            prompt_eval_ms: None,
            eval_ms: None,
            backend_type: None,
            auto_tier: None,
            auto_capability: None,
            auto_model: None,
        };
        state
            .metrics
            .record_request(&log.backend, &log.status, log.duration_ms)
            .await;
        if let Err(e) = state.analytics.log_request(log).await {
            tracing::error!("Failed to log request: {}", e);
        }

        let body = axum::body::Body::from_stream(response.bytes_stream());
        builder.body(body).map_err(|_| {
            openai_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to build response",
            )
        })
    } else {
        // Non-streaming: buffer body to extract token usage
        let body_bytes = response.bytes().await.unwrap_or_default();

        let mut tokens_in: Option<u32> = None;
        let mut tokens_out: Option<u32> = None;
        if let Ok(body_json) = serde_json::from_slice::<Value>(&body_bytes) {
            if let Some(usage) = body_json.get("usage") {
                tokens_in = usage
                    .get("prompt_tokens")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32);
                tokens_out = usage
                    .get("completion_tokens")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32);
            }
        }

        let duration_ms = duration.as_millis() as u64;
        let log = crate::analytics::RequestLog {
            timestamp: chrono::Utc::now().timestamp(),
            model: model_name.clone(),
            backend: backend_name.clone(),
            duration_ms,
            status: status.to_string(),
            path: "/v1/chat/completions".to_string(),
            request_id: Some(request_id.clone()),
            tier: None,
            classified_by: None,
            tokens_in,
            tokens_out,
            tokens_per_second: None,
            prompt_eval_ms: None,
            eval_ms: None,
            backend_type: None,
            auto_tier: None,
            auto_capability: None,
            auto_model: None,
        };
        state
            .metrics
            .record_request(&log.backend, &log.status, duration_ms)
            .await;
        if let (Some(tin), Some(tout)) = (tokens_in, tokens_out) {
            state
                .metrics
                .record_tokens(model_name.as_deref().unwrap_or("unknown"), tin, tout)
                .await;
            state
                .metrics
                .record_request_labeled(
                    &backend_name,
                    model_name.as_deref().unwrap_or("unknown"),
                    status,
                    duration_ms,
                )
                .await;
        }
        if let Err(e) = state.analytics.log_request(log).await {
            tracing::error!("Failed to log request: {}", e);
        }

        let body = axum::body::Body::from(body_bytes);
        builder.body(body).map_err(|_| {
            openai_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to build response",
            )
        })
    }
}

/// Returns an OpenAI-format error response.
fn openai_error(status: StatusCode, message: &str) -> (StatusCode, Json<Value>) {
    (
        status,
        Json(json!({
            "error": {
                "message": message,
                "type": "server_error",
                "code": status.as_u16(),
            }
        })),
    )
}
