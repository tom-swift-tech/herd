use crate::config::TaskClassifierConfig;
use crate::server::AppState;
use axum::{
    body::Body,
    extract::State,
    http::{HeaderValue, Request, Response},
    middleware::Next,
};

/// Classification result attached to request extensions.
#[derive(Debug, Clone)]
pub struct ClassificationResult {
    pub tier: String,
    pub model: String,
    pub classified_by: String, // "keyword" or "default"
}

/// Axum middleware that classifies requests by tier when no explicit model or tags are specified.
pub async fn classify_task(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response<Body> {
    let config = state.config.read().await;
    let classifier_config = config.task_classifier.clone();
    drop(config);

    // Skip if X-Herd-Tags header is present
    if request.headers().contains_key("x-herd-tags") {
        return next.run(request).await;
    }

    // Buffer the body so we can inspect it
    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, 10 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => {
            let request = Request::from_parts(parts, Body::empty());
            return next.run(request).await;
        }
    };

    // Parse JSON and classify
    let mut modified_bytes = bytes.clone();
    let mut classification: Option<ClassificationResult> = None;

    if let Ok(mut json) = serde_json::from_slice::<serde_json::Value>(&bytes) {
        let has_explicit_model = json
            .get("model")
            .and_then(|m| m.as_str())
            .map(|m| !m.is_empty())
            .unwrap_or(false);

        if !has_explicit_model {
            let user_message = extract_last_user_message(&json);
            let result = classify_by_keywords(&user_message, &classifier_config);

            if let Some(ref r) = result {
                // Inject classified model into the request body
                json["model"] = serde_json::Value::String(r.model.clone());
                if let Ok(serialized) = serde_json::to_vec(&json) {
                    modified_bytes = serialized.into();
                }
            }
            classification = result;
        }
    }

    // Rebuild request with (possibly modified) body
    let mut request = Request::from_parts(parts, Body::from(modified_bytes));

    // Attach classification to request extensions so downstream handlers can read it
    if let Some(result) = classification.clone() {
        request.extensions_mut().insert(result);
    }

    // Run the rest of the middleware chain / handler
    let mut response = next.run(request).await;

    // Add X-Herd-Tier response header on classified requests
    if let Some(result) = classification {
        if let Ok(val) = HeaderValue::from_str(&result.tier) {
            response.headers_mut().insert("x-herd-tier", val);
        }
    }

    response
}

/// Extracts the content of the last user message from the messages array.
pub fn extract_last_user_message(json: &serde_json::Value) -> String {
    json.get("messages")
        .and_then(|m| m.as_array())
        .and_then(|msgs| {
            msgs.iter()
                .rev()
                .find(|msg| msg.get("role").and_then(|r| r.as_str()) == Some("user"))
        })
        .and_then(|msg| msg.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string()
}

/// Matches user message against tier keywords. Returns the first matching tier,
/// or falls back to the configured default tier.
pub fn classify_by_keywords(
    message: &str,
    config: &TaskClassifierConfig,
) -> Option<ClassificationResult> {
    let message_lower = message.to_lowercase();

    // Check each tier's keywords for a match
    for (tier_name, tier_config) in &config.tiers {
        for keyword in &tier_config.keywords {
            if message_lower.contains(&keyword.to_lowercase()) {
                return Some(ClassificationResult {
                    tier: tier_name.clone(),
                    model: tier_config.model.clone(),
                    classified_by: "keyword".to_string(),
                });
            }
        }
    }

    // No keyword match — use default tier if configured
    config
        .tiers
        .get(&config.default_tier)
        .map(|default_tier| ClassificationResult {
            tier: config.default_tier.clone(),
            model: default_tier.model.clone(),
            classified_by: "default".to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{TaskClassifierConfig, TierConfig};
    use std::collections::HashMap;

    fn test_config() -> TaskClassifierConfig {
        let mut tiers = HashMap::new();
        tiers.insert(
            "heavy".to_string(),
            TierConfig {
                keywords: vec!["analyze".to_string(), "debug complex".to_string()],
                model: "qwen2.5:32b-instruct".to_string(),
            },
        );
        tiers.insert(
            "standard".to_string(),
            TierConfig {
                keywords: vec!["summarize".to_string(), "generate".to_string()],
                model: "qwen2.5:14b-instruct".to_string(),
            },
        );
        tiers.insert(
            "lightweight".to_string(),
            TierConfig {
                keywords: vec!["ping".to_string(), "hello".to_string()],
                model: "llama3.2:3b".to_string(),
            },
        );
        TaskClassifierConfig {
            enabled: true,
            strategy: "keyword".to_string(),
            default_tier: "standard".to_string(),
            tiers,
        }
    }

    #[test]
    fn test_classify_heavy_keyword() {
        let config = test_config();
        let result = classify_by_keywords("Please analyze this code for bugs", &config);
        let result = result.unwrap();
        assert_eq!(result.tier, "heavy");
        assert_eq!(result.model, "qwen2.5:32b-instruct");
        assert_eq!(result.classified_by, "keyword");
    }

    #[test]
    fn test_classify_no_match_uses_default() {
        let config = test_config();
        let result = classify_by_keywords("some random text with no keywords", &config);
        let result = result.unwrap();
        assert_eq!(result.tier, "standard");
        assert_eq!(result.model, "qwen2.5:14b-instruct");
        assert_eq!(result.classified_by, "default");
    }

    #[test]
    fn test_classify_case_insensitive() {
        let config = test_config();
        let result = classify_by_keywords("ANALYZE this please", &config);
        let result = result.unwrap();
        assert_eq!(result.tier, "heavy");
        assert_eq!(result.classified_by, "keyword");
    }

    #[test]
    fn test_classify_multi_word_keyword() {
        let config = test_config();
        let result = classify_by_keywords("Can you debug complex issue in my app?", &config);
        let result = result.unwrap();
        assert_eq!(result.tier, "heavy");
    }

    #[test]
    fn test_classify_lightweight_keyword() {
        let config = test_config();
        let result = classify_by_keywords("hello, how are you?", &config);
        let result = result.unwrap();
        assert_eq!(result.tier, "lightweight");
        assert_eq!(result.model, "llama3.2:3b");
    }

    #[test]
    fn test_classify_no_default_tier_returns_none() {
        let mut config = test_config();
        config.default_tier = "nonexistent".to_string();
        let result = classify_by_keywords("some random text", &config);
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_last_user_message() {
        let json = serde_json::json!({
            "messages": [
                {"role": "system", "content": "You are helpful"},
                {"role": "user", "content": "First message"},
                {"role": "assistant", "content": "Response"},
                {"role": "user", "content": "analyze this code"}
            ]
        });
        let msg = extract_last_user_message(&json);
        assert_eq!(msg, "analyze this code");
    }

    #[test]
    fn test_extract_last_user_message_no_messages() {
        let json = serde_json::json!({"model": "test"});
        let msg = extract_last_user_message(&json);
        assert_eq!(msg, "");
    }

    #[test]
    fn test_extract_last_user_message_no_user_role() {
        let json = serde_json::json!({
            "messages": [
                {"role": "system", "content": "You are helpful"}
            ]
        });
        let msg = extract_last_user_message(&json);
        assert_eq!(msg, "");
    }
}
