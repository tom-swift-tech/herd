# Auto Mode Classifier — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an LLM-based request classifier that routes requests to the best model/tier based on task requirements, replacing the need for clients to specify models explicitly.

**Architecture:** A new `src/classifier_auto.rs` module makes a lightweight LLM call to a small local model to classify incoming requests by tier (light/standard/heavy/frontier) and capability (general/code/reasoning/creative/vision/extraction). Results are cached by message hash. The classifier integrates into the proxy handler — when `model` is "auto" or omitted, classification runs before routing. A configurable model map resolves (tier, capability) → model name, which then feeds into existing model_aware routing.

**Tech Stack:** Rust, axum 0.7, reqwest 0.11, serde, tokio — no new dependencies.

---

## File Structure

| File | Action | Responsibility |
|------|--------|---------------|
| `src/classifier_auto.rs` | Create | AutoClassifier struct, LLM classification call, response parsing, LRU cache, model map resolution |
| `src/config.rs` | Modify | Add `AutoRoutingConfig` struct with classifier_model, model_map, timeouts, cache TTL |
| `src/server.rs` | Modify | Hook auto classification into proxy handler before routing (lines ~1067-1076) |
| `src/analytics.rs` | Modify | Add `auto_tier`, `auto_capability`, `auto_model` fields to RequestLog |
| `src/metrics.rs` | Modify | Add `auto_classifications_total`, `auto_classification_duration_ms`, `auto_cache_hits` counters |
| `src/lib.rs` | Modify | Add `pub mod classifier_auto;` |
| `herd.yaml.example` | Modify | Add commented `routing.auto` config section |

---

### Task 1: AutoRoutingConfig in config.rs

**Files:**
- Modify: `src/config.rs`

- [ ] **Step 1: Write the failing test**

Add at the bottom of the existing `#[cfg(test)] mod tests` in `src/config.rs`:

```rust
#[test]
fn auto_routing_config_defaults() {
    let config: AutoRoutingConfig = Default::default();
    assert!(!config.enabled);
    assert_eq!(config.classifier_model, "qwen3:1.7b");
    assert_eq!(config.classifier_timeout_ms, 3000);
    assert_eq!(config.fallback_model, "");
    assert_eq!(config.cache_ttl_secs, 60);
    assert!(config.model_map.is_empty());
}

#[test]
fn auto_routing_config_deserializes_from_yaml() {
    let yaml = r#"
enabled: true
classifier_model: "qwen3:1.7b"
classifier_timeout_ms: 2000
fallback_model: "qwen2.5-coder:32b"
cache_ttl_secs: 120
model_map:
  light:
    general: "qwen3:1.7b"
    code: "qwen2.5-coder:7b"
  standard:
    general: "qwen3:8b"
    code: "qwen2.5-coder:32b"
  heavy:
    general: "qwen3:32b"
  frontier:
    _provider: "true"
    general: "claude-sonnet-4-20250514"
"#;
    let config: AutoRoutingConfig = serde_yaml::from_str(yaml).unwrap();
    assert!(config.enabled);
    assert_eq!(config.classifier_timeout_ms, 2000);
    assert_eq!(config.model_map.len(), 4);
    assert_eq!(
        config.model_map["standard"]["code"],
        "qwen2.5-coder:32b"
    );
    assert_eq!(config.model_map["frontier"]["_provider"], "true");
}

#[test]
fn config_without_auto_section_backward_compat() {
    let yaml = r#"
server:
  host: "0.0.0.0"
  port: 40114
routing:
  strategy: "model_aware"
"#;
    let config: Config = serde_yaml::from_str(yaml).unwrap();
    assert!(!config.routing.auto.enabled);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test auto_routing_config -- --nocapture`
Expected: FAIL — `AutoRoutingConfig` type does not exist.

- [ ] **Step 3: Write the implementation**

Add to `src/config.rs`, after the `RoutingConfig` struct (around line 114):

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoRoutingConfig {
    #[serde(default)]
    pub enabled: bool,

    /// Small local model used to classify requests
    #[serde(default = "default_classifier_model")]
    pub classifier_model: String,

    /// Backend name to use for classification (null = any healthy backend)
    #[serde(default)]
    pub classifier_backend: Option<String>,

    /// Max time in ms for the classification LLM call
    #[serde(default = "default_classifier_timeout")]
    pub classifier_timeout_ms: u64,

    /// Model to use when classifier fails or times out
    #[serde(default)]
    pub fallback_model: String,

    /// Cache classification results for this many seconds (by message hash)
    #[serde(default = "default_cache_ttl")]
    pub cache_ttl_secs: u64,

    /// Tier → Capability → Model name mapping
    /// e.g. { "standard": { "code": "qwen2.5-coder:32b", "general": "qwen3:8b" } }
    #[serde(default)]
    pub model_map: HashMap<String, HashMap<String, String>>,
}

impl Default for AutoRoutingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            classifier_model: default_classifier_model(),
            classifier_backend: None,
            classifier_timeout_ms: default_classifier_timeout(),
            fallback_model: String::new(),
            cache_ttl_secs: default_cache_ttl(),
            model_map: HashMap::new(),
        }
    }
}

fn default_classifier_model() -> String {
    "qwen3:1.7b".to_string()
}
fn default_classifier_timeout() -> u64 {
    3000
}
fn default_cache_ttl() -> u64 {
    60
}
```

Add the `auto` field to `RoutingConfig`:

```rust
pub struct RoutingConfig {
    // ... existing fields ...

    #[serde(default)]
    pub auto: AutoRoutingConfig,
}
```

Update `RoutingConfig::default()` to include `auto: AutoRoutingConfig::default()`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test auto_routing_config -- --nocapture`
Expected: 3 tests PASS.

- [ ] **Step 5: Commit**

```
git add src/config.rs
git commit -m "feat: add AutoRoutingConfig for LLM-based request classification"
```

---

### Task 2: RequestLog auto fields in analytics.rs

**Files:**
- Modify: `src/analytics.rs`

- [ ] **Step 1: Write the failing test**

Add to the existing tests in `src/analytics.rs`:

```rust
#[test]
fn request_log_auto_fields_round_trip() {
    let log = RequestLog {
        timestamp: 1000,
        model: Some("qwen3:8b".into()),
        backend: "node1".into(),
        duration_ms: 500,
        status: "success".into(),
        path: "/v1/chat/completions".into(),
        request_id: None,
        tier: None,
        classified_by: None,
        tokens_in: None,
        tokens_out: None,
        tokens_per_second: None,
        prompt_eval_ms: None,
        eval_ms: None,
        backend_type: None,
        auto_tier: Some("standard".into()),
        auto_capability: Some("code".into()),
        auto_model: Some("qwen2.5-coder:32b".into()),
    };
    let json = serde_json::to_string(&log).unwrap();
    assert!(json.contains("auto_tier"));
    assert!(json.contains("standard"));
    let deser: RequestLog = serde_json::from_str(&json).unwrap();
    assert_eq!(deser.auto_tier.as_deref(), Some("standard"));
}

#[test]
fn request_log_without_auto_fields_backward_compat() {
    let json = r#"{"timestamp":1000,"model":null,"backend":"b1","duration_ms":100,"status":"success","path":"/test"}"#;
    let log: RequestLog = serde_json::from_str(json).unwrap();
    assert!(log.auto_tier.is_none());
    assert!(log.auto_capability.is_none());
    assert!(log.auto_model.is_none());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test request_log_auto_fields -- --nocapture`
Expected: FAIL — `auto_tier` field does not exist.

- [ ] **Step 3: Write the implementation**

Add three new fields to `RequestLog` in `src/analytics.rs`:

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub auto_tier: Option<String>,
#[serde(default, skip_serializing_if = "Option::is_none")]
pub auto_capability: Option<String>,
#[serde(default, skip_serializing_if = "Option::is_none")]
pub auto_model: Option<String>,
```

Update ALL existing `RequestLog` construction sites (in `server.rs` and `api/openai.rs`) to include `auto_tier: None, auto_capability: None, auto_model: None`. Search for `RequestLog {` to find them all.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test request_log_auto -- --nocapture`
Expected: PASS. Also run `cargo test` to verify no other tests break.

- [ ] **Step 5: Commit**

```
git add src/analytics.rs src/server.rs src/api/openai.rs
git commit -m "feat: add auto_tier, auto_capability, auto_model fields to RequestLog"
```

---

### Task 3: Auto classification metrics in metrics.rs

**Files:**
- Modify: `src/metrics.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn auto_classification_metrics() {
    let metrics = Metrics::new();
    metrics.record_auto_classification("standard", "code", 150, false).await;
    metrics.record_auto_classification("heavy", "reasoning", 200, true).await;
    let output = metrics.render().await;
    assert!(output.contains("herd_auto_classifications_total"));
    assert!(output.contains("herd_auto_classification_duration_ms"));
    assert!(output.contains("herd_auto_cache_hits_total"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test auto_classification_metrics -- --nocapture`
Expected: FAIL — method does not exist.

- [ ] **Step 3: Write the implementation**

Add to `Metrics` struct:
```rust
pub auto_classifications: Arc<RwLock<HashMap<String, AtomicU64>>>,  // tier|capability → count
pub auto_classification_duration_sum: Arc<AtomicU64>,
pub auto_classification_duration_count: Arc<AtomicU64>,
pub auto_cache_hits: Arc<AtomicU64>,
```

Initialize in `Metrics::new()`.

Add method:
```rust
pub async fn record_auto_classification(&self, tier: &str, capability: &str, duration_ms: u64, cache_hit: bool) {
    let key = format!("{}|{}", tier, capability);
    let map = self.auto_classifications.read().await;
    if let Some(counter) = map.get(&key) {
        counter.fetch_add(1, Ordering::Relaxed);
    } else {
        drop(map);
        let mut map = self.auto_classifications.write().await;
        map.entry(key).or_insert_with(|| AtomicU64::new(0)).fetch_add(1, Ordering::Relaxed);
    }
    self.auto_classification_duration_sum.fetch_add(duration_ms, Ordering::Relaxed);
    self.auto_classification_duration_count.fetch_add(1, Ordering::Relaxed);
    if cache_hit {
        self.auto_cache_hits.fetch_add(1, Ordering::Relaxed);
    }
}
```

Add rendering in the `render()` method for these three metric families.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test auto_classification_metrics -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```
git add src/metrics.rs
git commit -m "feat: add Prometheus metrics for auto classification"
```

---

### Task 4: AutoClassifier core module

**Files:**
- Create: `src/classifier_auto.rs`
- Modify: `src/lib.rs`

This is the core task — the LLM-based classifier with cache.

- [ ] **Step 1: Write the failing tests**

Create `src/classifier_auto.rs` with the test module first:

```rust
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
    fn parse_classification_with_extra_text() {
        // LLM might output markdown fences or preamble
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
        assert_eq!(c.capability, "general"); // default
    }

    #[test]
    fn resolve_model_from_map() {
        let mut model_map: HashMap<String, HashMap<String, String>> = HashMap::new();
        let mut standard = HashMap::new();
        standard.insert("code".to_string(), "qwen2.5-coder:32b".to_string());
        standard.insert("general".to_string(), "qwen3:8b".to_string());
        model_map.insert("standard".to_string(), standard);

        let result = resolve_model(&model_map, "standard", "code", "fallback:7b");
        assert_eq!(result, "qwen2.5-coder:32b");
    }

    #[test]
    fn resolve_model_missing_capability_falls_back_to_general() {
        let mut model_map: HashMap<String, HashMap<String, String>> = HashMap::new();
        let mut standard = HashMap::new();
        standard.insert("general".to_string(), "qwen3:8b".to_string());
        model_map.insert("standard".to_string(), standard);

        let result = resolve_model(&model_map, "standard", "vision", "fallback:7b");
        assert_eq!(result, "qwen3:8b"); // falls back to general within tier
    }

    #[test]
    fn resolve_model_missing_tier_uses_fallback() {
        let model_map: HashMap<String, HashMap<String, String>> = HashMap::new();
        let result = resolve_model(&model_map, "heavy", "code", "fallback:7b");
        assert_eq!(result, "fallback:7b");
    }

    #[test]
    fn cache_hit_and_expiry() {
        let cache = ClassificationCache::new(100);
        let classification = Classification {
            tier: "standard".to_string(),
            capability: "code".to_string(),
            needs_large_context: false,
            language: "en".to_string(),
        };
        let key = cache_key("Hello world, please review my code");
        cache.put(&key, classification.clone());
        assert!(cache.get(&key, Duration::from_secs(60)).is_some());

        // Different message = different key
        let key2 = cache_key("Completely different message");
        assert!(cache.get(&key2, Duration::from_secs(60)).is_none());
    }

    #[test]
    fn cache_key_uses_first_500_chars() {
        let short = "short message";
        let long = "a".repeat(1000);
        let long_same_prefix = format!("{}bbbb", "a".repeat(500));

        // Messages with same first 500 chars get same cache key
        let k1 = cache_key(&long);
        let k2 = cache_key(&long_same_prefix);
        assert_eq!(k1, k2);

        let k3 = cache_key(short);
        assert_ne!(k1, k3);
    }

    #[test]
    fn should_auto_classify_checks_model_field() {
        assert!(should_auto_classify(None));
        assert!(should_auto_classify(Some("auto")));
        assert!(should_auto_classify(Some("Auto")));
        assert!(should_auto_classify(Some("")));
        assert!(!should_auto_classify(Some("qwen3:8b")));
        assert!(!should_auto_classify(Some("llama3:70b")));
    }

    #[test]
    fn build_classification_prompt_includes_message() {
        let prompt = build_classification_prompt("Please review my Rust code for bugs");
        assert!(prompt.contains("review my Rust code"));
        assert!(prompt.contains("tier"));
        assert!(prompt.contains("capability"));
    }
}
```

- [ ] **Step 2: Add module to lib.rs**

Add `pub mod classifier_auto;` to `src/lib.rs`.

- [ ] **Step 3: Write the implementation above the tests**

```rust
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Classification result from the LLM classifier
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

fn default_tier() -> String { "standard".to_string() }
fn default_capability() -> String { "general".to_string() }
fn default_language() -> String { "en".to_string() }

/// Check if the request should trigger auto classification
pub fn should_auto_classify(model: Option<&str>) -> bool {
    match model {
        None => true,
        Some(m) => m.is_empty() || m.eq_ignore_ascii_case("auto"),
    }
}

/// Build the classification prompt for the LLM
pub fn build_classification_prompt(user_message: &str) -> String {
    format!(
        r#"You are a request classifier for an LLM router. Given the following user request, determine the best model tier and capability required.

Respond ONLY with a JSON object:
{{"tier": "light" | "standard" | "heavy" | "frontier", "capability": "general" | "code" | "reasoning" | "creative" | "vision" | "extraction", "needs_large_context": true | false, "language": "en"}}

User request:
{}"#,
        &user_message[..user_message.len().min(500)]
    )
}

/// Parse the LLM's classification response, tolerating markdown fences and preamble
pub fn parse_classification(text: &str) -> Option<Classification> {
    // Try direct JSON parse first
    if let Ok(c) = serde_json::from_str::<Classification>(text.trim()) {
        return Some(c);
    }
    // Try to extract JSON from markdown fences or surrounding text
    let text = text.trim();
    for line in text.lines() {
        let line = line.trim().trim_start_matches("```json").trim_start_matches("```").trim();
        if line.starts_with('{') {
            if let Ok(c) = serde_json::from_str::<Classification>(line) {
                return Some(c);
            }
        }
    }
    None
}

/// Resolve (tier, capability) → model name from the model map
pub fn resolve_model(
    model_map: &HashMap<String, HashMap<String, String>>,
    tier: &str,
    capability: &str,
    fallback: &str,
) -> String {
    if let Some(tier_map) = model_map.get(tier) {
        // Try exact capability match
        if let Some(model) = tier_map.get(capability) {
            return model.clone();
        }
        // Fall back to "general" within the same tier
        if let Some(model) = tier_map.get("general") {
            return model.clone();
        }
    }
    fallback.to_string()
}

/// Compute a cache key from the first 500 chars of a message
pub fn cache_key(message: &str) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let truncated = &message[..message.len().min(500)];
    let mut hasher = DefaultHasher::new();
    truncated.hash(&mut hasher);
    hasher.finish()
}

/// Simple LRU-ish cache for classification results
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

    pub fn get(&self, key: &u64, ttl: Duration) -> Option<Classification> {
        let entries = self.entries.lock().ok()?;
        let (classification, created_at) = entries.get(key)?;
        if created_at.elapsed() < ttl {
            Some(classification.clone())
        } else {
            None
        }
    }

    pub fn put(&self, key: &u64, classification: Classification) {
        let mut entries = match self.entries.lock() {
            Ok(e) => e,
            Err(_) => return,
        };
        // Evict expired or excess entries
        if entries.len() >= self.max_size {
            let oldest_key = entries
                .iter()
                .min_by_key(|(_, (_, ts))| *ts)
                .map(|(k, _)| *k);
            if let Some(k) = oldest_key {
                entries.remove(&k);
            }
        }
        entries.insert(*key, (classification, Instant::now()));
    }
}

/// Make the actual LLM classification call via Herd's backend pool.
/// Returns None on timeout/error (caller should use fallback).
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
        "messages": [{"role": "user", "content": prompt}],
        "stream": false,
        "options": {
            "temperature": 0.0,
            "num_predict": 100
        }
    });

    let url = format!("{}/v1/chat/completions", backend_url.trim_end_matches('/'));
    let resp = client
        .post(&url)
        .json(&body)
        .timeout(timeout)
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        tracing::debug!("Auto classifier got HTTP {}", resp.status());
        return None;
    }

    let resp_json: serde_json::Value = resp.json().await.ok()?;
    let content = resp_json
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())?;

    parse_classification(content)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test classifier_auto -- --nocapture`
Expected: All 10 tests PASS.

- [ ] **Step 5: Commit**

```
git add src/classifier_auto.rs src/lib.rs
git commit -m "feat: add LLM-based auto classifier module with cache and model map resolution"
```

---

### Task 5: Wire auto classification into proxy handler

**Files:**
- Modify: `src/server.rs` (lines ~1067-1076, where model is extracted from body)

- [ ] **Step 1: Write the failing test**

Add to `src/server.rs` tests:

```rust
#[test]
fn should_auto_classify_integration() {
    // Re-export test — verify the function is accessible from server context
    assert!(crate::classifier_auto::should_auto_classify(None));
    assert!(crate::classifier_auto::should_auto_classify(Some("auto")));
    assert!(!crate::classifier_auto::should_auto_classify(Some("qwen3:8b")));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test should_auto_classify_integration -- --nocapture`
Expected: PASS immediately (since classifier_auto module exists from Task 4). This test validates the integration path.

- [ ] **Step 3: Add AutoClassifier to AppState and wire into proxy handler**

In `src/server.rs`, add to `AppState`:
```rust
pub auto_cache: Arc<crate::classifier_auto::ClassificationCache>,
```

Initialize in the `Server::start()` method where AppState is constructed:
```rust
auto_cache: Arc::new(crate::classifier_auto::ClassificationCache::new(1000)),
```

In the proxy handler, after model extraction (~line 1076) and before the routing/retry loop, add:

```rust
// Auto classification: if model is "auto" or absent, classify and resolve
let mut auto_result: Option<crate::classifier_auto::Classification> = None;
if crate::classifier_auto::should_auto_classify(model_name.as_deref()) {
    let auto_config = state.config.read().await.routing.auto.clone();
    if auto_config.enabled {
        let user_message = crate::classifier::extract_last_user_message(
            &serde_json::from_slice::<serde_json::Value>(&body_bytes).unwrap_or_default()
        );

        if !user_message.is_empty() {
            let cache_key = crate::classifier_auto::cache_key(&user_message);
            let ttl = std::time::Duration::from_secs(auto_config.cache_ttl_secs);

            // Check cache first
            if let Some(cached) = state.auto_cache.get(&cache_key, ttl) {
                auto_result = Some(cached.clone());
                state.metrics.record_auto_classification(
                    &cached.tier, &cached.capability, 0, true
                ).await;
            } else {
                // Find a backend with the classifier model loaded
                let classify_start = std::time::Instant::now();
                let classifier_backend = state.pool.find_model_backend(&auto_config.classifier_model).await
                    .or_else(|| {
                        // Fall back to any healthy backend
                        // Use first healthy backend URL
                        None
                    });

                if let Some(backend_url) = classifier_backend {
                    let timeout = std::time::Duration::from_millis(auto_config.classifier_timeout_ms);
                    let result = crate::classifier_auto::classify_request(
                        &state.client,
                        &backend_url,
                        &auto_config.classifier_model,
                        &user_message,
                        timeout,
                    ).await;

                    let duration_ms = classify_start.elapsed().as_millis() as u64;
                    if let Some(ref c) = result {
                        state.auto_cache.put(&cache_key, c.clone());
                        state.metrics.record_auto_classification(
                            &c.tier, &c.capability, duration_ms, false
                        ).await;
                    }
                    auto_result = result;
                } else {
                    tracing::warn!("Auto classifier: no backend with model '{}' — using fallback", auto_config.classifier_model);
                }
            }

            // Resolve classification to a model name
            let resolved_model = if let Some(ref classification) = auto_result {
                crate::classifier_auto::resolve_model(
                    &auto_config.model_map,
                    &classification.tier,
                    &classification.capability,
                    &auto_config.fallback_model,
                )
            } else {
                auto_config.fallback_model.clone()
            };

            if !resolved_model.is_empty() {
                model_name = Some(resolved_model);
            }
        }
    }
}
```

Also add a helper method to `BackendPool` (`src/backend/pool.rs`):
```rust
/// Find the URL of a backend that has a specific model loaded
pub async fn find_model_backend(&self, model: &str) -> Option<String> {
    let backends = self.backends.read().await;
    backends.iter()
        .find(|b| b.healthy && b.models.iter().any(|m| m == model))
        .map(|b| b.config.url.clone())
}
```

Add response headers and populate RequestLog auto fields in `ProxyRequestContext::log_and_record`.

- [ ] **Step 4: Run full test suite**

Run: `cargo test`
Expected: All tests pass. Run `cargo clippy -- -D warnings` — clean.

- [ ] **Step 5: Commit**

```
git add src/server.rs src/backend/pool.rs
git commit -m "feat: wire auto classification into proxy handler with cache and model resolution"
```

---

### Task 6: Response headers and RequestLog population

**Files:**
- Modify: `src/server.rs`

- [ ] **Step 1: Add auto response headers**

In the proxy handler, after routing completes and before building the response, add headers when auto classification was used:

```rust
if let Some(ref classification) = auto_result {
    if let Ok(val) = axum::http::HeaderValue::from_str(&classification.tier) {
        builder = builder.header("x-herd-auto-tier", val);
    }
    if let Ok(val) = axum::http::HeaderValue::from_str(&classification.capability) {
        builder = builder.header("x-herd-auto-capability", val);
    }
    if let Some(ref m) = model_name {
        if let Ok(val) = axum::http::HeaderValue::from_str(m) {
            builder = builder.header("x-herd-auto-model", val);
        }
    }
}
```

- [ ] **Step 2: Populate auto fields in RequestLog**

Update `ProxyRequestContext` to carry `auto_tier`, `auto_capability`, `auto_model` and include them in the `log_and_record` method's `RequestLog` construction.

- [ ] **Step 3: Run full test suite**

Run: `cargo test && cargo clippy -- -D warnings`
Expected: All pass, clean.

- [ ] **Step 4: Commit**

```
git add src/server.rs
git commit -m "feat: add X-Herd-Auto-* response headers and populate RequestLog auto fields"
```

---

### Task 7: herd.yaml.example and documentation

**Files:**
- Modify: `herd.yaml.example`

- [ ] **Step 1: Add commented auto routing config**

```yaml
# routing:
#   strategy: "auto"          # or keep "model_aware" — auto only activates for model: "auto" / omitted
#   auto:
#     enabled: false           # off by default — zero overhead when disabled
#     classifier_model: "qwen3:1.7b"   # small local model for classification
#     classifier_timeout_ms: 3000       # max time for classification LLM call
#     fallback_model: "qwen2.5-coder:32b"  # used when classifier fails/times out
#     cache_ttl_secs: 60       # cache classification results by message hash
#     model_map:
#       light:
#         general: "qwen3:1.7b"
#         code: "qwen2.5-coder:7b"
#       standard:
#         general: "qwen3:8b"
#         code: "qwen2.5-coder:32b"
#         reasoning: "qwen3:32b"
#       heavy:
#         general: "qwen3:32b"
#         code: "qwen2.5-coder:32b"
#       frontier:
#         _provider: "true"    # signals: route to frontier gateway
#         general: "claude-sonnet-4-20250514"
#         reasoning: "claude-opus-4-20250514"
```

- [ ] **Step 2: Commit**

```
git add herd.yaml.example
git commit -m "docs: add auto routing config example to herd.yaml.example"
```

---

## Summary

| Task | What | Tests |
|------|------|-------|
| 1 | AutoRoutingConfig in config.rs | 3 |
| 2 | RequestLog auto fields | 2 |
| 3 | Prometheus auto metrics | 1 |
| 4 | AutoClassifier core (parse, resolve, cache, classify) | 10 |
| 5 | Wire into proxy handler | 1 |
| 6 | Response headers + RequestLog wiring | 0 (integration) |
| 7 | herd.yaml.example docs | 0 |

**Total new tests: ~17**
**New files: 1** (`src/classifier_auto.rs`)
**Modified files: 6** (config.rs, analytics.rs, metrics.rs, server.rs, backend/pool.rs, lib.rs)
