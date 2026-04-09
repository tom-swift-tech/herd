use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tracing::warn;

// --- Classification struct ---

fn default_tier() -> String {
    "standard".to_string()
}

fn default_capability() -> String {
    "general".to_string()
}

fn default_language() -> String {
    "en".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Classification {
    #[serde(default = "default_tier")]
    pub tier: String,
    #[serde(default = "default_capability")]
    pub capability: String,
    #[serde(default)]
    pub needs_large_context: bool,
    #[serde(default = "default_language")]
    pub language: String,
}

// --- Helper functions ---

/// Returns true when model is None, empty, or "auto" (case-insensitive).
pub fn should_auto_classify(model: Option<&str>) -> bool {
    match model {
        None => true,
        Some(m) => m.is_empty() || m.eq_ignore_ascii_case("auto"),
    }
}

/// Builds the classification prompt, truncating user_message to first 500 chars.
pub fn build_classification_prompt(user_message: &str) -> String {
    let truncated: String = user_message.chars().take(500).collect();
    format!(
        r#"Classify this user message for LLM routing. Respond ONLY with a JSON object, no other text.

Fields:
- "tier": one of "light", "standard", "heavy", "frontier"
- "capability": one of "general", "code", "reasoning", "creative", "vision", "extraction"
- "needs_large_context": boolean, true if the task likely needs >8k context
- "language": ISO 639-1 code of the primary language

User message:
{truncated}"#
    )
}

/// Parses LLM response text into a Classification.
/// Tolerates clean JSON, markdown fences, and preamble text before JSON.
pub fn parse_classification(text: &str) -> Option<Classification> {
    // Try direct parse first
    if let Ok(c) = serde_json::from_str::<Classification>(text) {
        return Some(c);
    }

    // Try extracting from markdown fences
    if let Some(start) = text.find("```json") {
        let after_fence = &text[start + 7..];
        if let Some(end) = after_fence.find("```") {
            let json_str = after_fence[..end].trim();
            if let Ok(c) = serde_json::from_str::<Classification>(json_str) {
                return Some(c);
            }
        }
    }

    // Try extracting from generic fences
    if let Some(start) = text.find("```") {
        let after_fence = &text[start + 3..];
        if let Some(end) = after_fence.find("```") {
            let json_str = after_fence[..end].trim();
            if let Ok(c) = serde_json::from_str::<Classification>(json_str) {
                return Some(c);
            }
        }
    }

    // Try finding first '{' and last '}'
    if let Some(start) = text.find('{') {
        if let Some(end) = text.rfind('}') {
            if start < end {
                let json_str = &text[start..=end];
                if let Ok(c) = serde_json::from_str::<Classification>(json_str) {
                    return Some(c);
                }
            }
        }
    }

    None
}

/// Resolves (tier, capability) to a model name from the model_map.
/// Fallback chain: exact match -> general fallback -> ultimate fallback.
pub fn resolve_model(
    model_map: &HashMap<String, HashMap<String, String>>,
    tier: &str,
    capability: &str,
    fallback: &str,
) -> String {
    if let Some(tier_map) = model_map.get(tier) {
        if let Some(model) = tier_map.get(capability) {
            return model.clone();
        }
        if let Some(model) = tier_map.get("general") {
            return model.clone();
        }
    }
    fallback.to_string()
}

/// Hash first 500 chars of message using DefaultHasher.
pub fn cache_key(message: &str) -> u64 {
    let truncated: String = message.chars().take(500).collect();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    truncated.hash(&mut hasher);
    hasher.finish()
}

// --- ClassificationCache ---

pub struct ClassificationCache {
    entries: Mutex<HashMap<u64, (Classification, Instant)>>,
    max_size: usize,
}

impl ClassificationCache {
    pub fn new(max_size: usize) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            max_size,
        }
    }

    /// Returns None if key not found or entry expired.
    pub fn get(&self, key: &u64, ttl: Duration) -> Option<Classification> {
        let entries = self.entries.lock().ok()?;
        if let Some((classification, inserted_at)) = entries.get(key) {
            if inserted_at.elapsed() < ttl {
                return Some(classification.clone());
            }
        }
        None
    }

    /// Inserts a classification. Evicts oldest entry if at capacity.
    pub fn put(&self, key: &u64, classification: Classification) {
        let mut entries = match self.entries.lock() {
            Ok(e) => e,
            Err(_) => return,
        };

        if entries.len() >= self.max_size && !entries.contains_key(key) {
            // Evict oldest entry
            if let Some(oldest_key) = entries
                .iter()
                .min_by_key(|(_, (_, instant))| *instant)
                .map(|(k, _)| *k)
            {
                entries.remove(&oldest_key);
            }
        }

        entries.insert(*key, (classification, Instant::now()));
    }
}

// --- Async classify_request ---

/// Makes a lightweight LLM call to classify a user message.
/// Returns None on empty messages, timeouts, HTTP errors, or parse failures.
pub async fn classify_request(
    client: &reqwest::Client,
    backend_url: &str,
    classifier_model: &str,
    user_message: &str,
    timeout: Duration,
) -> Option<Classification> {
    if user_message.trim().is_empty() {
        return None;
    }

    let prompt = build_classification_prompt(user_message);

    let body = serde_json::json!({
        "model": classifier_model,
        "messages": [
            {
                "role": "user",
                "content": prompt
            }
        ],
        "stream": false,
        "temperature": 0.0,
        "num_predict": 100
    });

    let url = format!("{}/v1/chat/completions", backend_url.trim_end_matches('/'));

    let response = match client.post(&url).json(&body).timeout(timeout).send().await {
        Ok(resp) => resp,
        Err(e) => {
            warn!("Auto-classifier request failed: {}", e);
            return None;
        }
    };

    if !response.status().is_success() {
        warn!("Auto-classifier returned status {}", response.status());
        return None;
    }

    let json: serde_json::Value = match response.json().await {
        Ok(v) => v,
        Err(e) => {
            warn!("Auto-classifier response parse error: {}", e);
            return None;
        }
    };

    let content = json
        .get("choices")?
        .get(0)?
        .get("message")?
        .get("content")?
        .as_str()?;

    parse_classification(content)
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_classification_response() {
        let json = r#"{"tier": "standard", "capability": "code", "needs_large_context": false, "language": "en"}"#;
        let result = parse_classification(json);
        assert!(result.is_some());
        let c = result.unwrap();
        assert_eq!(c.tier, "standard");
        assert_eq!(c.capability, "code");
        assert!(!c.needs_large_context);
    }

    #[test]
    fn parse_classification_with_markdown_fences() {
        let text = "Here is the classification:\n```json\n{\"tier\": \"heavy\", \"capability\": \"reasoning\", \"needs_large_context\": true, \"language\": \"en\"}\n```";
        let result = parse_classification(text);
        assert!(result.is_some());
        assert_eq!(result.unwrap().tier, "heavy");
    }

    #[test]
    fn parse_classification_invalid_json() {
        let result = parse_classification("not json at all");
        assert!(result.is_none());
    }

    #[test]
    fn parse_classification_missing_fields_uses_defaults() {
        let json = r#"{"tier": "light"}"#;
        let result = parse_classification(json);
        assert!(result.is_some());
        let c = result.unwrap();
        assert_eq!(c.tier, "light");
        assert_eq!(c.capability, "general");
    }

    #[test]
    fn resolve_model_exact_match() {
        let mut model_map: HashMap<String, HashMap<String, String>> = HashMap::new();
        let mut standard = HashMap::new();
        standard.insert("code".to_string(), "qwen2.5-coder:32b".to_string());
        standard.insert("general".to_string(), "qwen3:8b".to_string());
        model_map.insert("standard".to_string(), standard);

        assert_eq!(
            resolve_model(&model_map, "standard", "code", "fallback"),
            "qwen2.5-coder:32b"
        );
    }

    #[test]
    fn resolve_model_falls_back_to_general() {
        let mut model_map: HashMap<String, HashMap<String, String>> = HashMap::new();
        let mut standard = HashMap::new();
        standard.insert("general".to_string(), "qwen3:8b".to_string());
        model_map.insert("standard".to_string(), standard);

        assert_eq!(
            resolve_model(&model_map, "standard", "vision", "fallback"),
            "qwen3:8b"
        );
    }

    #[test]
    fn resolve_model_missing_tier_uses_fallback() {
        let model_map: HashMap<String, HashMap<String, String>> = HashMap::new();
        assert_eq!(
            resolve_model(&model_map, "heavy", "code", "fallback:7b"),
            "fallback:7b"
        );
    }

    #[test]
    fn cache_hit_and_miss() {
        let cache = ClassificationCache::new(100);
        let c = Classification {
            tier: "standard".to_string(),
            capability: "code".to_string(),
            needs_large_context: false,
            language: "en".to_string(),
        };
        let key = cache_key("review my code");
        cache.put(&key, c);
        assert!(cache.get(&key, Duration::from_secs(60)).is_some());
        assert!(cache.get(&999999, Duration::from_secs(60)).is_none());
    }

    #[test]
    fn cache_key_truncates_to_500_chars() {
        let long = "a".repeat(1000);
        let long_same_prefix = format!("{}bbbb", "a".repeat(500));
        assert_eq!(cache_key(&long), cache_key(&long_same_prefix));
        assert_ne!(cache_key(&long), cache_key("short"));
    }

    #[test]
    fn should_auto_classify_checks_model() {
        assert!(should_auto_classify(None));
        assert!(should_auto_classify(Some("auto")));
        assert!(should_auto_classify(Some("Auto")));
        assert!(should_auto_classify(Some("")));
        assert!(!should_auto_classify(Some("qwen3:8b")));
    }
}
