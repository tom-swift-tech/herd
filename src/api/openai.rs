use crate::router::Router;
use crate::server::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde_json::{json, Value};
use std::collections::HashSet;

fn frontier_provider_is_available(provider: &crate::config::ProviderConfig) -> bool {
    !provider.api_key_env.is_empty() && std::env::var(&provider.api_key_env).is_ok()
}

fn extract_client_name(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get("x-herd-client")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

fn extract_tags(headers: &axum::http::HeaderMap) -> Option<Vec<String>> {
    headers
        .get("x-herd-tags")
        .and_then(|v| v.to_str().ok())
        .map(|s| {
            s.split(',')
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect()
        })
}

/// Rewrite the JSON body's `model` field to match the resolved model name.
///
/// Auto-mode classification, frontier-fallback, and profile `preferred_model`
/// all mutate `model_name` in memory for routing purposes, but the outgoing
/// body still carries the client's original `"model"` (often `"auto"` or
/// missing). Backends receive that literal and 404 it, which the retry loop
/// mis-reads as backend failure and exhausts into a 503. This helper closes
/// that gap. Falls through on non-JSON bodies per Herd's "degrade gracefully"
/// policy.
pub(crate) fn rewrite_request_model(body: &[u8], model: Option<&str>) -> Vec<u8> {
    let Some(model) = model else {
        return body.to_vec();
    };
    let Ok(mut parsed) = serde_json::from_slice::<Value>(body) else {
        return body.to_vec();
    };
    let Some(obj) = parsed.as_object_mut() else {
        return body.to_vec();
    };
    obj.insert("model".to_string(), Value::String(model.to_string()));
    serde_json::to_vec(&parsed).unwrap_or_else(|_| body.to_vec())
}

/// Cheap pre-routing estimate of prompt size in tokens, for the scored router's
/// `prompt_size_vs_capacity` dimension. Heuristic: ~4 characters per token over
/// all chat `messages[].content` (string or multimodal `text` parts) or a plain
/// `prompt` string. Returns `None` when the body isn't recognized JSON or has no
/// text — the dimension then stays absent (neutral), never a penalty.
pub(crate) fn estimate_prompt_tokens(body: &[u8]) -> Option<u32> {
    const CHARS_PER_TOKEN: usize = 4;
    let json: Value = serde_json::from_slice(body).ok()?;

    let mut chars: usize = 0;
    if let Some(messages) = json.get("messages").and_then(Value::as_array) {
        for msg in messages {
            match msg.get("content") {
                Some(Value::String(s)) => chars += s.chars().count(),
                Some(Value::Array(parts)) => {
                    for part in parts {
                        if let Some(t) = part.get("text").and_then(Value::as_str) {
                            chars += t.chars().count();
                        }
                    }
                }
                _ => {}
            }
        }
    } else if let Some(prompt) = json.get("prompt").and_then(Value::as_str) {
        chars += prompt.chars().count();
    } else {
        return None;
    }

    if chars == 0 {
        return None;
    }
    Some((chars / CHARS_PER_TOKEN).max(1) as u32)
}

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

    // Include frontier provider models when frontier is enabled
    let config = state.config.read().await;
    if config.frontier.enabled {
        for provider in &config.providers {
            if !frontier_provider_is_available(provider) {
                continue;
            }
            for model in &provider.models {
                if seen.insert(model.clone()) {
                    models.push(json!({
                        "id": model,
                        "object": "model",
                        "created": 0,
                        "owned_by": &provider.name,
                        "herd_provider": &provider.name,
                        "herd_type": "frontier",
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
    let mut model_name = request_json
        .as_ref()
        .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(String::from));
    let is_streaming = request_json
        .as_ref()
        .and_then(|v| v.get("stream").and_then(|s| s.as_bool()))
        .unwrap_or(false);

    // Auto classification: if model is "auto" or absent, classify and resolve
    let mut auto_tier: Option<String> = None;
    let mut auto_capability: Option<String> = None;
    let mut auto_model: Option<String> = None;
    let mut auto_classification: Option<crate::classifier_auto::Classification> = None;
    if crate::classifier_auto::should_auto_classify(model_name.as_deref()) {
        let auto_config = state.config.read().await.routing.auto.clone();
        if auto_config.enabled {
            let user_message = request_json
                .as_ref()
                .map(crate::classifier::extract_last_user_message)
                .unwrap_or_default();

            if !user_message.is_empty() {
                let ck = crate::classifier_auto::cache_key(&user_message);
                let ttl = std::time::Duration::from_secs(auto_config.cache_ttl_secs);

                auto_classification = if let Some(cached) = state.auto_cache.get(&ck, ttl) {
                    state
                        .metrics
                        .record_auto_classification(&cached.tier, &cached.capability, 0, true)
                        .await;
                    Some(cached)
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
                        result
                    } else {
                        tracing::warn!(
                            "Auto classifier: no backend with model '{}' — using fallback",
                            auto_config.classifier_model
                        );
                        None
                    }
                };

                let resolved = if let Some(ref c) = auto_classification {
                    auto_tier = Some(c.tier.clone());
                    auto_capability = Some(c.capability.clone());
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
                    auto_model = Some(resolved.clone());
                    model_name = Some(resolved);
                }
            }
        }
    }

    let client_name = extract_client_name(&headers);

    // Frontier gateway: escalate to cloud provider if model resolved to a
    // frontier model (either explicitly by the client or via auto mode).
    let frontier_config = state.config.read().await.frontier.clone();
    let provider_configs = state.config.read().await.providers.clone();

    let frontier_limiter = state.frontier_rate_limiter.read().await;
    if let Some(response) = crate::providers::frontier_route_if_applicable(
        &state.client,
        &frontier_config,
        &provider_configs,
        &state.cost_db,
        &frontier_limiter,
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
    drop(frontier_limiter);

    // If auto classified to frontier tier but the gateway declined to handle
    // it (disabled, escalation blocked, or no matching provider), fall back
    // to the configured fallback_model so we don't try to route a frontier
    // model name to a local backend.
    if auto_tier.as_deref() == Some("frontier") {
        let fallback = state
            .config
            .read()
            .await
            .routing
            .auto
            .fallback_model
            .clone();
        if !fallback.is_empty() {
            auto_model = Some(fallback.clone());
            model_name = Some(fallback);
        }
    }

    state.budget.reset_if_needed().await;
    match state
        .budget
        .check_budget(
            client_name.as_deref(),
            model_name.as_deref().unwrap_or("unknown"),
        )
        .await
    {
        crate::budget::BudgetStatus::Exceeded {
            cap_type,
            limit,
            current,
        } => {
            return Err(openai_error_with_code(
                StatusCode::TOO_MANY_REQUESTS,
                "budget_exceeded",
                &format!(
                    "Budget exceeded (cap={}, limit={:.2}, current={:.2})",
                    cap_type, limit, current
                ),
            ));
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

    let request_tags = extract_tags(&headers);
    let profile_header = headers
        .get("x-herd-profile")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let resolved = {
        let config = state.config.read().await;
        crate::profiles::resolve_profile(&config, profile_header.as_deref())
    };

    let tags: Option<Vec<String>> = if request_tags.is_some() {
        request_tags
    } else if !resolved.tags.is_empty() {
        Some(resolved.tags.clone())
    } else {
        None
    };

    if model_name.is_none() {
        if let Some(ref preferred) = resolved.preferred_model {
            model_name = Some(preferred.clone());
        }
    }

    let profile_name = resolved.profile_name.clone();

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

    if !resolved.backends.is_empty() {
        for name in state.pool.all().await {
            if !resolved.backends.contains(&name) {
                excluded.insert(name);
            }
        }
    }

    let forward_body = rewrite_request_model(&body_bytes, model_name.as_deref());

    // Session id (X-Herd-Session) for the scored router's dim 18 stickiness.
    let session_id: Option<String> = headers
        .get("x-herd-session")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty());

    // Estimate prompt size once (pre-routing) for the scored router's dim 3.
    // Ignored by the other routing strategies (their route_scored is ctx-blind).
    let route_ctx = crate::router::RouteContext {
        prompt_tokens: estimate_prompt_tokens(&body_bytes),
        requested_ctx_len: None,
        session_id: session_id.clone(),
    };

    for _ in 0..=state.retry_count() {
        let backend = state
            .router
            .read()
            .await
            .route_scored(
                model_name.as_deref(),
                tags.as_deref(),
                &excluded,
                &route_ctx,
            )
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
            .body(forward_body.clone());

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
                auto_tier: auto_tier.clone(),
                auto_capability: auto_capability.clone(),
                auto_model: auto_model.clone(),
                frontier_provider: None,
                frontier_cost_usd: None,
            };
            state
                .metrics
                .record_request(&log.backend, &log.status, log.duration_ms)
                .await;
            // Phase-3: feed the error ring even when all retries fail.
            state
                .routing_stats
                .update(
                    &log.backend,
                    log.model.as_deref().unwrap_or(""),
                    log.duration_ms,
                    true, // all-retries-failed → error
                    None,
                )
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
    if let Some(ref pname) = profile_name {
        if let Ok(val) = axum::http::HeaderValue::from_str(pname) {
            builder = builder.header("x-herd-profile", val);
        }
    }
    // Auto classification response headers
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
    if let Some(ref m) = auto_model {
        if let Ok(val) = axum::http::HeaderValue::from_str(m) {
            builder = builder.header("x-herd-auto-model", val);
        }
    }
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
            frontier_provider: None,
            frontier_cost_usd: None,
        };
        state
            .metrics
            .record_request(&log.backend, &log.status, log.duration_ms)
            .await;
        // Phase-3: streaming path — tokens_out not available; feeds latency + error ring.
        state
            .routing_stats
            .update(
                &log.backend,
                log.model.as_deref().unwrap_or(""),
                log.duration_ms,
                log.status == "error",
                None, // tokens not extracted from streaming SSE
            )
            .await;
        // Phase-4 dim 23: stamp warm-recency on a successful served request.
        if log.status != "error" {
            if let Some(m) = log.model.as_deref() {
                state.pool.record_served(&log.backend, m).await;
            }
            // Phase-4 dim 18: record this session's backend for next-turn stickiness.
            if let Some(sid) = session_id.as_deref() {
                state.session_affinity.record(sid, &log.backend).await;
            }
        }
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
            frontier_provider: None,
            frontier_cost_usd: None,
        };
        state
            .metrics
            .record_request(&log.backend, &log.status, duration_ms)
            .await;
        // Phase-3: non-streaming path — tokens_out available when the backend
        // returned a usage block; feeds latency EWMA, tps EWMA, and error ring.
        state
            .routing_stats
            .update(
                &log.backend,
                log.model.as_deref().unwrap_or(""),
                duration_ms,
                log.status == "error",
                tokens_out,
            )
            .await;
        // Phase-4 dim 23: stamp warm-recency on a successful served request.
        if log.status != "error" {
            if let Some(m) = log.model.as_deref() {
                state.pool.record_served(&log.backend, m).await;
            }
            // Phase-4 dim 18: record this session's backend for next-turn stickiness.
            if let Some(sid) = session_id.as_deref() {
                state.session_affinity.record(sid, &log.backend).await;
            }
        }
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

        if let (Some(tin), Some(tout)) = (tokens_in, tokens_out) {
            let model = model_name.as_deref().unwrap_or("unknown");
            let cost = crate::analytics::estimate_api_cost(model, tin as u64, tout as u64);
            state
                .budget
                .record_cost(client_name.as_deref(), model, cost)
                .await;
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
    openai_error_with_code(status, "server_error", message)
}

fn openai_error_with_code(
    status: StatusCode,
    error_type: &str,
    message: &str,
) -> (StatusCode, Json<Value>) {
    (
        status,
        Json(json!({
            "error": {
                "message": message,
                "type": error_type,
                "code": status.as_u16(),
            }
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(bytes: &[u8]) -> Value {
        serde_json::from_slice(bytes).unwrap()
    }

    #[test]
    fn estimate_tokens_sums_chat_message_content() {
        // 40 chars across two messages → 40/4 = 10 tokens.
        let body = br#"{"messages":[{"role":"system","content":"aaaaaaaaaaaaaaaaaaaa"},{"role":"user","content":"bbbbbbbbbbbbbbbbbbbb"}]}"#;
        assert_eq!(estimate_prompt_tokens(body), Some(10));
    }

    #[test]
    fn estimate_tokens_handles_multimodal_text_parts() {
        // Only the text parts count (8 chars → 2 tokens); image parts ignored.
        let body = br#"{"messages":[{"role":"user","content":[{"type":"text","text":"abcdefgh"},{"type":"image_url","image_url":{"url":"x"}}]}]}"#;
        assert_eq!(estimate_prompt_tokens(body), Some(2));
    }

    #[test]
    fn estimate_tokens_handles_plain_prompt() {
        // Completions/Ollama-generate style `prompt` string (12 chars → 3).
        assert_eq!(
            estimate_prompt_tokens(br#"{"prompt":"abcdefghijkl"}"#),
            Some(3)
        );
    }

    #[test]
    fn estimate_tokens_none_when_unrecognized_or_empty() {
        assert_eq!(estimate_prompt_tokens(b"not json"), None);
        assert_eq!(estimate_prompt_tokens(br#"{"model":"x"}"#), None);
        assert_eq!(estimate_prompt_tokens(br#"{"messages":[]}"#), None);
        // A sub-token-length prompt still rounds up to at least 1.
        assert_eq!(estimate_prompt_tokens(br#"{"prompt":"hi"}"#), Some(1));
    }

    #[test]
    fn rewrite_replaces_existing_model() {
        let body = br#"{"model":"auto","messages":[{"role":"user","content":"hi"}]}"#;
        let out = rewrite_request_model(body, Some("qwen3:1.7b"));
        assert_eq!(parse(&out)["model"], "qwen3:1.7b");
        assert_eq!(parse(&out)["messages"][0]["content"], "hi");
    }

    #[test]
    fn rewrite_adds_missing_model() {
        let body = br#"{"messages":[{"role":"user","content":"hi"}]}"#;
        let out = rewrite_request_model(body, Some("gemma4:e4b"));
        assert_eq!(parse(&out)["model"], "gemma4:e4b");
    }

    #[test]
    fn rewrite_noop_when_model_is_none() {
        let body = br#"{"model":"llama3","messages":[]}"#;
        let out = rewrite_request_model(body, None);
        assert_eq!(out, body);
    }

    #[test]
    fn rewrite_falls_through_on_non_json() {
        let body = b"not json at all";
        let out = rewrite_request_model(body, Some("qwen3:1.7b"));
        assert_eq!(out, body);
    }

    #[test]
    fn rewrite_falls_through_on_non_object_json() {
        let body = br#"["array","not","object"]"#;
        let out = rewrite_request_model(body, Some("qwen3:1.7b"));
        assert_eq!(out, body);
    }
}
