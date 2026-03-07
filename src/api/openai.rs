use crate::router::Router;
use crate::server::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde_json::{json, Value};

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
    let headers = parts.headers.clone();

    // Read body with size cap (10 MB)
    let body_bytes = axum::body::to_bytes(body, 10 * 1024 * 1024)
        .await
        .map_err(|_| openai_error(StatusCode::PAYLOAD_TOO_LARGE, "Request body too large"))?;

    // Extract model from body (required for routing)
    let model_name = serde_json::from_slice::<Value>(&body_bytes)
        .ok()
        .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(String::from));

    // Route to backend based on model
    let backend = state
        .router
        .route(model_name.as_deref())
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

    // Preserve query string if present
    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| "/v1/chat/completions".to_string());

    let url = format!(
        "{}{}",
        backend.url.trim_end_matches('/'),
        path_and_query
    );

    // Build proxied request with header forwarding
    let method = reqwest::Method::from_bytes(parts.method.as_str().as_bytes())
        .unwrap_or(reqwest::Method::POST);

    let mut req_builder = state
        .client
        .request(method, &url)
        .timeout(state.routing_timeout)
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

    let response = match req_builder.send().await {
        Ok(r) => {
            state.pool.mark_healthy(&backend.name).await;
            r
        }
        Err(e) => {
            tracing::error!("Upstream request to {} failed: {}", url, e);
            state.pool.mark_unhealthy(&backend.name).await;

            let duration = start.elapsed();
            let log = crate::analytics::RequestLog {
                timestamp: chrono::Utc::now().timestamp(),
                model: model_name,
                backend: backend.name.clone(),
                duration_ms: duration.as_millis() as u64,
                status: "error".to_string(),
                path: "/v1/chat/completions".to_string(),
            };
            let _ = state.analytics.log_request(log).await;

            return Err(openai_error(
                StatusCode::BAD_GATEWAY,
                &format!("Backend '{}' unreachable: {}", backend.name, e),
            ));
        }
    };

    let duration = start.elapsed();
    let status = if response.status().is_success() { "success" } else { "error" };

    let log = crate::analytics::RequestLog {
        timestamp: chrono::Utc::now().timestamp(),
        model: model_name,
        backend: backend.name.clone(),
        duration_ms: duration.as_millis() as u64,
        status: status.to_string(),
        path: "/v1/chat/completions".to_string(),
    };
    if let Err(e) = state.analytics.log_request(log).await {
        tracing::error!("Failed to log request: {}", e);
    }

    // Bridge response back (reqwest → axum)
    let status_code = axum::http::StatusCode::from_u16(response.status().as_u16())
        .unwrap_or(axum::http::StatusCode::OK);

    let mut builder = axum::response::Response::builder().status(status_code);
    for (name, value) in response.headers() {
        if let (Ok(an), Ok(av)) = (
            axum::http::HeaderName::from_bytes(name.as_ref()),
            axum::http::HeaderValue::from_bytes(value.as_ref()),
        ) {
            builder = builder.header(an, av);
        }
    }

    // Stream body for SSE streaming support
    let body = axum::body::Body::from_stream(response.bytes_stream());
    builder
        .body(body)
        .map_err(|_| openai_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to build response"))
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
