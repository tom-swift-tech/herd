pub mod anthropic;
pub mod cost_db;
pub mod openai_compat;
pub mod pricing;

use crate::config::ProviderConfig;
use anyhow::Result;

pub trait ProviderAdapter: Send + Sync {
    fn transform_request(&self, body: &serde_json::Value) -> Result<serde_json::Value>;
    fn transform_response(&self, body: &serde_json::Value) -> Result<serde_json::Value>;
    fn transform_stream_chunk(&self, chunk: &str) -> Result<String>;
    fn extract_usage(&self, body: &serde_json::Value) -> Option<(u64, u64)>;
    fn auth_header(&self, api_key: &str) -> String;
}

// ---------------------------------------------------------------------------
// Frontier gateway helpers
// ---------------------------------------------------------------------------

/// Check if a model name belongs to any configured frontier provider
pub fn is_frontier_model(model: &str, providers: &[ProviderConfig]) -> bool {
    providers.iter().any(|p| {
        p.models.contains(&model.to_string()) || (p.models.is_empty() && !model.is_empty())
    })
}

/// Find the provider that serves a given model (highest priority wins)
pub fn resolve_provider<'a>(
    model: &str,
    providers: &'a [ProviderConfig],
) -> Option<&'a ProviderConfig> {
    providers
        .iter()
        .filter(|p| p.models.contains(&model.to_string()) || p.models.is_empty())
        .max_by_key(|p| p.priority)
}

/// Get the appropriate adapter for a provider
pub fn get_adapter(provider: &ProviderConfig) -> Box<dyn ProviderAdapter> {
    if provider.api_url.contains("anthropic.com") {
        Box::new(anthropic::AnthropicAdapter)
    } else {
        Box::new(openai_compat::OpenAICompatAdapter)
    }
}

// ---------------------------------------------------------------------------
// FrontierError
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum FrontierError {
    NoProvider(String),
    NoApiKey(String, String),
    BudgetExceeded {
        provider: String,
        spent: f32,
        budget: f32,
    },
    RequestFailed(String, String),
    HeaderRequired,
}

impl std::fmt::Display for FrontierError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoProvider(m) => write!(f, "No frontier provider for model '{}'", m),
            Self::NoApiKey(p, env) => write!(f, "Set {} for provider '{}'", env, p),
            Self::BudgetExceeded {
                provider,
                spent,
                budget,
            } => write!(
                f,
                "Budget exceeded for '{}': ${:.2}/${:.2}",
                provider, spent, budget
            ),
            Self::RequestFailed(p, e) => write!(f, "Provider '{}' failed: {}", p, e),
            Self::HeaderRequired => write!(f, "X-Herd-Frontier: true header required"),
        }
    }
}

// ---------------------------------------------------------------------------
// Frontier proxy result
// ---------------------------------------------------------------------------

pub struct FrontierProxyResult {
    pub response: reqwest::Response,
    pub provider_name: String,
}

// ---------------------------------------------------------------------------
// proxy_frontier_request
// ---------------------------------------------------------------------------

pub async fn proxy_frontier_request(
    client: &reqwest::Client,
    _frontier_config: &crate::config::FrontierConfig,
    providers: &[ProviderConfig],
    cost_db: &cost_db::CostDb,
    model: &str,
    body: &serde_json::Value,
    request_id: Option<&str>,
) -> std::result::Result<FrontierProxyResult, FrontierError> {
    // 1. Resolve provider
    let provider = resolve_provider(model, providers)
        .ok_or_else(|| FrontierError::NoProvider(model.to_string()))?;

    // 2. Get API key from env
    let api_key = std::env::var(&provider.api_key_env).map_err(|_| {
        FrontierError::NoApiKey(provider.name.clone(), provider.api_key_env.clone())
    })?;

    // 3. Check monthly budget
    if provider.monthly_budget > 0.0 {
        let spent = cost_db.monthly_spend(&provider.name).unwrap_or(0.0) as f32;
        if spent >= provider.monthly_budget {
            return Err(FrontierError::BudgetExceeded {
                provider: provider.name.clone(),
                spent,
                budget: provider.monthly_budget,
            });
        }
    }

    // 4. Get adapter, transform request body
    let adapter = get_adapter(provider);
    let transformed = adapter
        .transform_request(body)
        .map_err(|e| FrontierError::RequestFailed(provider.name.clone(), e.to_string()))?;

    // 5. Build URL path (Anthropic: /messages, others: /chat/completions)
    let is_anthropic = provider.api_url.contains("anthropic.com");
    let url = if is_anthropic {
        format!("{}/v1/messages", provider.api_url.trim_end_matches('/'))
    } else {
        format!(
            "{}/v1/chat/completions",
            provider.api_url.trim_end_matches('/')
        )
    };

    // 6. Build request with correct auth
    let mut req = client
        .post(&url)
        .header("content-type", "application/json")
        .json(&transformed);

    if is_anthropic {
        req = req
            .header("x-api-key", &api_key)
            .header("anthropic-version", "2023-06-01");
    } else {
        req = req.header("authorization", format!("Bearer {}", api_key));
    }

    if let Some(rid) = request_id {
        req = req.header("x-request-id", rid);
    }

    // 7. Send and return raw response
    let response = req
        .send()
        .await
        .map_err(|e| FrontierError::RequestFailed(provider.name.clone(), e.to_string()))?;

    Ok(FrontierProxyResult {
        response,
        provider_name: provider.name.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_providers() -> Vec<ProviderConfig> {
        vec![
            ProviderConfig {
                name: "anthropic".to_string(),
                api_url: "https://api.anthropic.com".to_string(),
                api_key_env: "ANTHROPIC_API_KEY".to_string(),
                models: vec![
                    "claude-opus-4-20250514".to_string(),
                    "claude-sonnet-4-20250514".to_string(),
                ],
                priority: 100,
                monthly_budget: 50.0,
                ..Default::default()
            },
            ProviderConfig {
                name: "openai".to_string(),
                api_url: "https://api.openai.com".to_string(),
                api_key_env: "OPENAI_API_KEY".to_string(),
                models: vec!["gpt-4.1".to_string(), "o4-mini".to_string()],
                priority: 80,
                monthly_budget: 100.0,
                ..Default::default()
            },
            ProviderConfig {
                name: "fallback".to_string(),
                api_url: "https://api.example.com".to_string(),
                api_key_env: "FALLBACK_API_KEY".to_string(),
                models: vec![],
                priority: 10,
                monthly_budget: 0.0,
                ..Default::default()
            },
        ]
    }

    #[test]
    fn resolve_provider_by_model() {
        let providers = test_providers();
        let p = resolve_provider("claude-opus-4-20250514", &providers).unwrap();
        assert_eq!(p.name, "anthropic");

        let p = resolve_provider("gpt-4.1", &providers).unwrap();
        assert_eq!(p.name, "openai");
    }

    #[test]
    fn resolve_provider_unknown_falls_to_wildcard() {
        let providers = test_providers();
        // "unknown-model" doesn't match anthropic or openai models specifically,
        // but the fallback provider has empty models (wildcard).
        // Wildcard matches all, so we get the highest-priority wildcard.
        let p = resolve_provider("unknown-model", &providers).unwrap();
        assert_eq!(p.name, "fallback");
    }

    #[test]
    fn is_frontier_model_known_and_unknown() {
        let providers = test_providers();
        assert!(is_frontier_model("claude-opus-4-20250514", &providers));
        assert!(is_frontier_model("gpt-4.1", &providers));
        // fallback has empty models, so any non-empty model matches
        assert!(is_frontier_model("random-model", &providers));
        // empty model name never matches
        assert!(!is_frontier_model("", &providers));
    }

    #[test]
    fn adapter_selection_anthropic() {
        let provider = ProviderConfig {
            api_url: "https://api.anthropic.com".to_string(),
            ..Default::default()
        };
        let adapter = get_adapter(&provider);
        // Anthropic adapter should transform the request (not passthrough)
        let body = serde_json::json!({
            "model": "claude-sonnet-4-20250514",
            "messages": [{"role": "user", "content": "hi"}],
        });
        let result = adapter.transform_request(&body).unwrap();
        // Anthropic adapter adds max_tokens
        assert!(result.get("max_tokens").is_some());
    }

    #[test]
    fn adapter_selection_openai() {
        let provider = ProviderConfig {
            api_url: "https://api.openai.com".to_string(),
            ..Default::default()
        };
        let adapter = get_adapter(&provider);
        // OpenAI adapter is passthrough
        let body = serde_json::json!({"model": "gpt-4.1", "messages": [{"role": "user", "content": "hi"}]});
        let result = adapter.transform_request(&body).unwrap();
        assert_eq!(result, body);
    }
}
