# Frontier Gateway — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a frontier gateway that routes requests to cloud LLM providers (Anthropic, OpenAI, xAI, OpenRouter, MiniMax) when local models can't handle the task, with per-provider budget enforcement, rate limiting, and cost tracking.

**Architecture:** A `src/providers/` module defines a `ProviderAdapter` trait with two implementations — `AnthropicAdapter` (translates OpenAI format to Anthropic Messages API) and `OpenAICompatAdapter` (pass-through for OpenAI/xAI/OpenRouter/MiniMax). A `FrontierGateway` orchestrates provider selection, budget checks, rate limiting, and request proxying. Cost is tracked in SQLite and exposed via Prometheus metrics. The gateway integrates into the existing proxy handler — when a model resolves to a frontier provider, the request is routed through the gateway instead of the local backend pool.

**Tech Stack:** Rust, axum 0.7, reqwest 0.11, rusqlite, serde, tokio — no new dependencies.

---

## File Structure

| File | Action | Responsibility |
|------|--------|---------------|
| `src/providers/mod.rs` | Create | `ProviderAdapter` trait, `FrontierGateway` struct, provider routing logic |
| `src/providers/anthropic.rs` | Create | OpenAI → Anthropic Messages API translation + streaming |
| `src/providers/openai_compat.rs` | Create | Pass-through adapter for OpenAI-format providers |
| `src/providers/pricing.rs` | Create | Built-in price table + configurable overrides |
| `src/providers/cost_db.rs` | Create | SQLite `frontier_costs` table, cost recording + budget queries |
| `src/config.rs` | Modify | `FrontierConfig`, `ProviderConfig`, `PricingOverride` structs |
| `src/server.rs` | Modify | Wire gateway into proxy handler, add `frontier_gateway` to AppState |
| `src/api/openai.rs` | Modify | Include frontier models in `GET /v1/models` |
| `src/analytics.rs` | Modify | Add `provider`, `frontier_cost_usd` fields to RequestLog |
| `src/metrics.rs` | Modify | Add frontier Prometheus counters |
| `src/lib.rs` | Modify | Add `pub mod providers;` |
| `herd.yaml.example` | Modify | Add `frontier` and `providers` config sections |
| `dashboard.html` | Modify | Add Costs tab or extend Analytics |

---

### Task 1: Config structs (FrontierConfig + ProviderConfig)

**Files:**
- Modify: `src/config.rs`

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn frontier_config_defaults() {
    let config: FrontierConfig = Default::default();
    assert!(!config.enabled);
    assert!(!config.allow_auto_escalation);
    assert!(config.require_header);
    assert!(config.log_all_requests);
    assert_eq!(config.warn_threshold, 0.80);
    assert_eq!(config.block_threshold, 1.00);
}

#[test]
fn provider_config_deserializes() {
    let yaml = r#"
name: "anthropic"
type: "frontier"
api_url: "https://api.anthropic.com/v1"
api_key_env: "ANTHROPIC_API_KEY"
models:
  - "claude-sonnet-4-20250514"
  - "claude-opus-4-20250514"
rate_limit: 50
monthly_budget: 100.00
priority: 50
"#;
    let config: ProviderConfig = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(config.name, "anthropic");
    assert_eq!(config.models.len(), 2);
    assert_eq!(config.rate_limit, 50);
    assert!((config.monthly_budget - 100.0).abs() < 0.01);
}

#[test]
fn provider_config_with_pricing_overrides() {
    let yaml = r#"
name: "anthropic"
api_url: "https://api.anthropic.com/v1"
api_key_env: "ANTHROPIC_API_KEY"
models: ["claude-sonnet-4-20250514"]
pricing:
  claude-sonnet-4-20250514:
    input_per_mtok: 3.00
    output_per_mtok: 15.00
"#;
    let config: ProviderConfig = serde_yaml::from_str(yaml).unwrap();
    assert!(config.pricing.contains_key("claude-sonnet-4-20250514"));
    let p = &config.pricing["claude-sonnet-4-20250514"];
    assert!((p.input_per_mtok - 3.0).abs() < 0.01);
}

#[test]
fn config_without_frontier_section_backward_compat() {
    let yaml = r#"
server:
  host: "0.0.0.0"
  port: 40114
"#;
    let config: Config = serde_yaml::from_str(yaml).unwrap();
    assert!(!config.frontier.enabled);
    assert!(config.providers.is_empty());
}
```

- [ ] **Step 2: Run tests — expect FAIL**

- [ ] **Step 3: Implement**

Add to `src/config.rs`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrontierConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub allow_auto_escalation: bool,
    #[serde(default = "default_true")]
    pub require_header: bool,
    #[serde(default = "default_true")]
    pub log_all_requests: bool,
    #[serde(default = "default_warn_threshold")]
    pub warn_threshold: f32,
    #[serde(default = "default_block_threshold")]
    pub block_threshold: f32,
}

impl Default for FrontierConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            allow_auto_escalation: false,
            require_header: true,
            log_all_requests: true,
            warn_threshold: 0.80,
            block_threshold: 1.00,
        }
    }
}

fn default_warn_threshold() -> f32 { 0.80 }
fn default_block_threshold() -> f32 { 1.00 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub name: String,
    #[serde(default = "default_frontier_type")]
    pub r#type: String,
    pub api_url: String,
    pub api_key_env: String,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub rate_limit: u64,  // requests per minute
    #[serde(default)]
    pub monthly_budget: f32,
    #[serde(default = "default_provider_priority")]
    pub priority: u32,
    #[serde(default)]
    pub pricing: HashMap<String, PricingOverride>,
}

fn default_frontier_type() -> String { "frontier".to_string() }
fn default_provider_priority() -> u32 { 50 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PricingOverride {
    pub input_per_mtok: f32,
    pub output_per_mtok: f32,
}
```

Add to Config:
```rust
#[serde(default)]
pub frontier: FrontierConfig,
#[serde(default)]
pub providers: Vec<ProviderConfig>,
```

- [ ] **Step 4: Run tests — expect PASS**
- [ ] **Step 5: Commit** `feat: add FrontierConfig and ProviderConfig structs`

---

### Task 2: Built-in pricing table

**Files:**
- Create: `src/providers/pricing.rs`

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_model_has_pricing() {
        assert!(get_pricing("claude-sonnet-4-20250514").is_some());
        assert!(get_pricing("gpt-4.1").is_some());
        assert!(get_pricing("grok-3").is_some());
    }

    #[test]
    fn unknown_model_returns_none() {
        assert!(get_pricing("totally-unknown-model").is_none());
    }

    #[test]
    fn cost_calculation() {
        let p = get_pricing("claude-sonnet-4-20250514").unwrap();
        // 1000 input tokens + 500 output tokens
        let cost = calculate_cost(&p, 1000, 500);
        // input: 1000/1M * input_per_mtok, output: 500/1M * output_per_mtok
        assert!(cost > 0.0);
    }

    #[test]
    fn override_replaces_builtin() {
        let mut overrides = HashMap::new();
        overrides.insert("claude-sonnet-4-20250514".to_string(), PricingOverride {
            input_per_mtok: 99.0, output_per_mtok: 99.0,
        });
        let p = get_pricing_with_overrides("claude-sonnet-4-20250514", &overrides).unwrap();
        assert!((p.input_per_mtok - 99.0).abs() < 0.01);
    }
}
```

- [ ] **Step 2: Run tests — expect FAIL**

- [ ] **Step 3: Implement**

```rust
use crate::config::PricingOverride;
use std::collections::HashMap;

pub struct ModelPricing {
    pub input_per_mtok: f32,
    pub output_per_mtok: f32,
}

pub fn get_pricing(model: &str) -> Option<ModelPricing> {
    // Built-in price table (updated each release)
    match model {
        // Anthropic
        "claude-opus-4-20250514" => Some(ModelPricing { input_per_mtok: 15.0, output_per_mtok: 75.0 }),
        "claude-sonnet-4-20250514" => Some(ModelPricing { input_per_mtok: 3.0, output_per_mtok: 15.0 }),
        // OpenAI
        "gpt-4.1" => Some(ModelPricing { input_per_mtok: 2.0, output_per_mtok: 8.0 }),
        "o4-mini" => Some(ModelPricing { input_per_mtok: 1.10, output_per_mtok: 4.40 }),
        // xAI
        "grok-3" => Some(ModelPricing { input_per_mtok: 3.0, output_per_mtok: 15.0 }),
        "grok-3-mini" => Some(ModelPricing { input_per_mtok: 0.30, output_per_mtok: 0.50 }),
        // MiniMax
        "MiniMax-M1" => Some(ModelPricing { input_per_mtok: 0.80, output_per_mtok: 3.20 }),
        _ => None,
    }
}

pub fn get_pricing_with_overrides(model: &str, overrides: &HashMap<String, PricingOverride>) -> Option<ModelPricing> {
    if let Some(ov) = overrides.get(model) {
        return Some(ModelPricing { input_per_mtok: ov.input_per_mtok, output_per_mtok: ov.output_per_mtok });
    }
    get_pricing(model)
}

pub fn calculate_cost(pricing: &ModelPricing, tokens_in: u64, tokens_out: u64) -> f32 {
    (tokens_in as f32 / 1_000_000.0) * pricing.input_per_mtok
        + (tokens_out as f32 / 1_000_000.0) * pricing.output_per_mtok
}
```

- [ ] **Step 4: Run tests — expect PASS**
- [ ] **Step 5: Commit** `feat: add built-in frontier model pricing table`

---

### Task 3: SQLite cost tracking (frontier_costs table)

**Files:**
- Create: `src/providers/cost_db.rs`

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn test_cost_db() -> CostDb {
        let conn = Connection::open_in_memory().unwrap();
        let db = CostDb { conn: std::sync::Mutex::new(conn) };
        db.migrate().unwrap();
        db
    }

    #[test]
    fn record_and_query_cost() {
        let db = test_cost_db();
        db.record_cost("anthropic", "claude-sonnet-4-20250514", 1000, 500, 0.0105, Some("req-1")).unwrap();
        db.record_cost("anthropic", "claude-sonnet-4-20250514", 2000, 1000, 0.021, Some("req-2")).unwrap();

        let total = db.monthly_spend("anthropic").unwrap();
        assert!((total - 0.0315).abs() < 0.001);
    }

    #[test]
    fn monthly_spend_filters_by_provider() {
        let db = test_cost_db();
        db.record_cost("anthropic", "claude-sonnet-4-20250514", 1000, 500, 0.01, None).unwrap();
        db.record_cost("openai", "gpt-4.1", 1000, 500, 0.01, None).unwrap();

        let anthropic = db.monthly_spend("anthropic").unwrap();
        let openai = db.monthly_spend("openai").unwrap();
        assert!((anthropic - 0.01).abs() < 0.001);
        assert!((openai - 0.01).abs() < 0.001);
    }

    #[test]
    fn cost_summary_returns_all_providers() {
        let db = test_cost_db();
        db.record_cost("anthropic", "model-a", 1000, 500, 0.01, None).unwrap();
        db.record_cost("openai", "model-b", 2000, 1000, 0.02, None).unwrap();

        let summary = db.cost_summary().unwrap();
        assert_eq!(summary.len(), 2);
    }
}
```

- [ ] **Step 2: Run tests — expect FAIL**

- [ ] **Step 3: Implement**

```rust
use anyhow::Result;
use rusqlite::Connection;
use std::sync::Mutex;
use std::collections::HashMap;
use serde::Serialize;

pub struct CostDb {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderCostSummary {
    pub provider: String,
    pub total_cost_usd: f64,
    pub total_tokens_in: u64,
    pub total_tokens_out: u64,
    pub request_count: u64,
}

impl CostDb {
    pub fn new(conn: Connection) -> Self {
        let db = Self { conn: Mutex::new(conn) };
        db.migrate().unwrap_or_else(|e| tracing::warn!("Frontier cost table migration failed: {}", e));
        db
    }

    pub fn migrate(&self) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("DB lock: {}", e))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS frontier_costs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                provider TEXT NOT NULL,
                model TEXT NOT NULL,
                tokens_in INTEGER NOT NULL,
                tokens_out INTEGER NOT NULL,
                cost_usd REAL NOT NULL,
                request_id TEXT,
                timestamp TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE INDEX IF NOT EXISTS idx_frontier_costs_provider_month
                ON frontier_costs(provider, timestamp);"
        )?;
        Ok(())
    }

    pub fn record_cost(&self, provider: &str, model: &str, tokens_in: u64, tokens_out: u64, cost_usd: f32, request_id: Option<&str>) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("DB lock: {}", e))?;
        conn.execute(
            "INSERT INTO frontier_costs (provider, model, tokens_in, tokens_out, cost_usd, request_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![provider, model, tokens_in as i64, tokens_out as i64, cost_usd as f64, request_id],
        )?;
        Ok(())
    }

    /// Total spend for a provider in the current calendar month
    pub fn monthly_spend(&self, provider: &str) -> Result<f64> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("DB lock: {}", e))?;
        let now = chrono::Utc::now();
        let month_start = format!("{}-{:02}-01T00:00:00", now.format("%Y"), now.format("%m"));
        let total: f64 = conn.query_row(
            "SELECT COALESCE(SUM(cost_usd), 0.0) FROM frontier_costs WHERE provider = ?1 AND timestamp >= ?2",
            rusqlite::params![provider, month_start],
            |row| row.get(0),
        )?;
        Ok(total)
    }

    pub fn cost_summary(&self) -> Result<Vec<ProviderCostSummary>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("DB lock: {}", e))?;
        let now = chrono::Utc::now();
        let month_start = format!("{}-{:02}-01T00:00:00", now.format("%Y"), now.format("%m"));
        let mut stmt = conn.prepare(
            "SELECT provider, SUM(cost_usd), SUM(tokens_in), SUM(tokens_out), COUNT(*)
             FROM frontier_costs WHERE timestamp >= ?1 GROUP BY provider"
        )?;
        let rows = stmt.query_map(rusqlite::params![month_start], |row| {
            Ok(ProviderCostSummary {
                provider: row.get(0)?,
                total_cost_usd: row.get(1)?,
                total_tokens_in: row.get::<_, i64>(2)? as u64,
                total_tokens_out: row.get::<_, i64>(3)? as u64,
                request_count: row.get::<_, i64>(4)? as u64,
            })
        })?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}
```

- [ ] **Step 4: Run tests — expect PASS**
- [ ] **Step 5: Commit** `feat: add SQLite frontier cost tracking table`

---

### Task 4: ProviderAdapter trait + OpenAI-compat adapter

**Files:**
- Create: `src/providers/mod.rs`
- Create: `src/providers/openai_compat.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Write the failing tests**

In `src/providers/openai_compat.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_compat_passthrough_request() {
        let adapter = OpenAICompatAdapter;
        let body = serde_json::json!({
            "model": "gpt-4.1",
            "messages": [{"role": "user", "content": "hello"}],
            "stream": false
        });
        let transformed = adapter.transform_request(&body).unwrap();
        assert_eq!(transformed, body); // pass-through — unchanged
    }

    #[test]
    fn openai_compat_passthrough_response() {
        let adapter = OpenAICompatAdapter;
        let body = serde_json::json!({
            "choices": [{"message": {"content": "hi"}}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 10}
        });
        let transformed = adapter.transform_response(&body).unwrap();
        assert_eq!(transformed, body);
    }

    #[test]
    fn openai_compat_extract_usage() {
        let adapter = OpenAICompatAdapter;
        let body = serde_json::json!({
            "usage": {"prompt_tokens": 100, "completion_tokens": 50}
        });
        let usage = adapter.extract_usage(&body);
        assert!(usage.is_some());
        let (tin, tout) = usage.unwrap();
        assert_eq!(tin, 100);
        assert_eq!(tout, 50);
    }

    #[test]
    fn openai_compat_auth_header() {
        let adapter = OpenAICompatAdapter;
        assert_eq!(adapter.auth_header("sk-test"), "Bearer sk-test");
    }
}
```

- [ ] **Step 2: Run tests — expect FAIL**

- [ ] **Step 3: Implement trait + adapter**

`src/providers/mod.rs`:
```rust
pub mod anthropic;
pub mod cost_db;
pub mod openai_compat;
pub mod pricing;

use anyhow::Result;

pub trait ProviderAdapter: Send + Sync {
    fn transform_request(&self, body: &serde_json::Value) -> Result<serde_json::Value>;
    fn transform_response(&self, body: &serde_json::Value) -> Result<serde_json::Value>;
    fn transform_stream_chunk(&self, chunk: &str) -> Result<String>;
    fn extract_usage(&self, body: &serde_json::Value) -> Option<(u64, u64)>;
    fn auth_header(&self, api_key: &str) -> String;
}
```

`src/providers/openai_compat.rs`:
```rust
use super::ProviderAdapter;
use anyhow::Result;

pub struct OpenAICompatAdapter;

impl ProviderAdapter for OpenAICompatAdapter {
    fn transform_request(&self, body: &serde_json::Value) -> Result<serde_json::Value> {
        Ok(body.clone())
    }
    fn transform_response(&self, body: &serde_json::Value) -> Result<serde_json::Value> {
        Ok(body.clone())
    }
    fn transform_stream_chunk(&self, chunk: &str) -> Result<String> {
        Ok(chunk.to_string())
    }
    fn extract_usage(&self, body: &serde_json::Value) -> Option<(u64, u64)> {
        let usage = body.get("usage")?;
        let tin = usage.get("prompt_tokens")?.as_u64()?;
        let tout = usage.get("completion_tokens")?.as_u64()?;
        Some((tin, tout))
    }
    fn auth_header(&self, api_key: &str) -> String {
        format!("Bearer {}", api_key)
    }
}
```

Add `pub mod providers;` to `src/lib.rs`.

- [ ] **Step 4: Run tests — expect PASS**
- [ ] **Step 5: Commit** `feat: add ProviderAdapter trait and OpenAI-compat pass-through adapter`

---

### Task 5: Anthropic adapter

**Files:**
- Create: `src/providers/anthropic.rs`

This is the most complex adapter — Anthropic's Messages API has a different JSON structure than OpenAI.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transform_request_converts_format() {
        let adapter = AnthropicAdapter;
        let openai = serde_json::json!({
            "model": "claude-sonnet-4-20250514",
            "messages": [
                {"role": "system", "content": "You are helpful"},
                {"role": "user", "content": "Hello"}
            ],
            "max_tokens": 1024,
            "stream": false,
            "temperature": 0.7
        });
        let anthropic = adapter.transform_request(&openai).unwrap();
        assert_eq!(anthropic["model"], "claude-sonnet-4-20250514");
        assert_eq!(anthropic["system"], "You are helpful");
        assert_eq!(anthropic["max_tokens"], 1024);
        // Messages should not contain system role
        let msgs = anthropic["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
    }

    #[test]
    fn transform_request_adds_default_max_tokens() {
        let adapter = AnthropicAdapter;
        let openai = serde_json::json!({
            "model": "claude-sonnet-4-20250514",
            "messages": [{"role": "user", "content": "Hi"}]
        });
        let anthropic = adapter.transform_request(&openai).unwrap();
        assert!(anthropic.get("max_tokens").is_some());
    }

    #[test]
    fn transform_response_to_openai_format() {
        let adapter = AnthropicAdapter;
        let anthropic_resp = serde_json::json!({
            "id": "msg_123",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "Hello!"}],
            "model": "claude-sonnet-4-20250514",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        });
        let openai = adapter.transform_response(&anthropic_resp).unwrap();
        assert_eq!(openai["choices"][0]["message"]["content"], "Hello!");
        assert_eq!(openai["usage"]["prompt_tokens"], 10);
        assert_eq!(openai["usage"]["completion_tokens"], 5);
    }

    #[test]
    fn extract_usage_from_anthropic_response() {
        let adapter = AnthropicAdapter;
        let resp = serde_json::json!({
            "usage": {"input_tokens": 100, "output_tokens": 50}
        });
        let (tin, tout) = adapter.extract_usage(&resp).unwrap();
        assert_eq!(tin, 100);
        assert_eq!(tout, 50);
    }

    #[test]
    fn auth_header_uses_x_api_key_format() {
        let adapter = AnthropicAdapter;
        assert_eq!(adapter.auth_header("sk-test"), "sk-test");
        // Note: Anthropic uses x-api-key header, not Authorization Bearer
    }

    #[test]
    fn transform_stream_chunk_anthropic_to_openai() {
        let adapter = AnthropicAdapter;
        let chunk = r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hello"}}"#;
        let transformed = adapter.transform_stream_chunk(chunk).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&transformed).unwrap();
        assert_eq!(parsed["choices"][0]["delta"]["content"], "Hello");
    }
}
```

- [ ] **Step 2: Run tests — expect FAIL**

- [ ] **Step 3: Implement**

Key translation rules:
- OpenAI `messages` with `role: "system"` → Anthropic top-level `system` field
- OpenAI `max_tokens` → Anthropic `max_tokens` (required, default to 4096 if missing)
- Anthropic response `content[0].text` → OpenAI `choices[0].message.content`
- Anthropic `usage.input_tokens` → OpenAI `usage.prompt_tokens`
- Anthropic `usage.output_tokens` → OpenAI `usage.completion_tokens`
- Anthropic streaming: `content_block_delta` events → OpenAI `chat.completion.chunk` format
- Auth: Anthropic uses `x-api-key` header (not `Authorization: Bearer`)

```rust
use super::ProviderAdapter;
use anyhow::Result;

pub struct AnthropicAdapter;

impl ProviderAdapter for AnthropicAdapter {
    fn transform_request(&self, body: &serde_json::Value) -> Result<serde_json::Value> {
        let mut result = serde_json::Map::new();
        
        // Copy model
        if let Some(model) = body.get("model") {
            result.insert("model".to_string(), model.clone());
        }
        
        // Extract system message, pass remaining messages
        let mut system_parts = Vec::new();
        let mut messages = Vec::new();
        if let Some(msgs) = body.get("messages").and_then(|m| m.as_array()) {
            for msg in msgs {
                if msg.get("role").and_then(|r| r.as_str()) == Some("system") {
                    if let Some(content) = msg.get("content").and_then(|c| c.as_str()) {
                        system_parts.push(content.to_string());
                    }
                } else {
                    messages.push(msg.clone());
                }
            }
        }
        if !system_parts.is_empty() {
            result.insert("system".to_string(), serde_json::Value::String(system_parts.join("\n")));
        }
        result.insert("messages".to_string(), serde_json::Value::Array(messages));
        
        // max_tokens (required by Anthropic, default 4096)
        let max_tokens = body.get("max_tokens").cloned()
            .unwrap_or(serde_json::json!(4096));
        result.insert("max_tokens".to_string(), max_tokens);
        
        // Pass through optional fields
        for field in &["temperature", "top_p", "stream"] {
            if let Some(val) = body.get(*field) {
                result.insert(field.to_string(), val.clone());
            }
        }
        
        Ok(serde_json::Value::Object(result))
    }
    
    fn transform_response(&self, body: &serde_json::Value) -> Result<serde_json::Value> {
        // Extract text from content blocks
        let content = body.get("content")
            .and_then(|c| c.as_array())
            .map(|blocks| {
                blocks.iter()
                    .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default();
        
        // Map usage fields
        let usage = body.get("usage");
        let prompt_tokens = usage.and_then(|u| u.get("input_tokens")).cloned().unwrap_or(serde_json::json!(0));
        let completion_tokens = usage.and_then(|u| u.get("output_tokens")).cloned().unwrap_or(serde_json::json!(0));
        
        Ok(serde_json::json!({
            "id": body.get("id").cloned().unwrap_or(serde_json::json!("")),
            "object": "chat.completion",
            "model": body.get("model").cloned().unwrap_or(serde_json::json!("")),
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": content },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
            }
        }))
    }
    
    fn transform_stream_chunk(&self, chunk: &str) -> Result<String> {
        let parsed: serde_json::Value = serde_json::from_str(chunk)?;
        let event_type = parsed.get("type").and_then(|t| t.as_str()).unwrap_or("");
        
        match event_type {
            "content_block_delta" => {
                let text = parsed.get("delta")
                    .and_then(|d| d.get("text"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                Ok(serde_json::to_string(&serde_json::json!({
                    "choices": [{"index": 0, "delta": {"content": text}}]
                }))?)
            }
            "message_stop" => {
                Ok(serde_json::to_string(&serde_json::json!({
                    "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
                }))?)
            }
            _ => Ok(String::new()) // Skip other event types
        }
    }
    
    fn extract_usage(&self, body: &serde_json::Value) -> Option<(u64, u64)> {
        let usage = body.get("usage")?;
        let tin = usage.get("input_tokens")?.as_u64()?;
        let tout = usage.get("output_tokens")?.as_u64()?;
        Some((tin, tout))
    }
    
    fn auth_header(&self, api_key: &str) -> String {
        // Anthropic uses x-api-key header, not Authorization Bearer
        // The caller must use this value with the x-api-key header name
        api_key.to_string()
    }
}
```

- [ ] **Step 4: Run tests — expect PASS**
- [ ] **Step 5: Commit** `feat: add Anthropic Messages API translation adapter`

---

### Task 6: FrontierGateway orchestrator

**Files:**
- Modify: `src/providers/mod.rs`

This is the central orchestrator that resolves providers, enforces budgets/rate limits, proxies requests, and records costs.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn test_providers() -> Vec<ProviderConfig> {
        vec![
            ProviderConfig {
                name: "anthropic".into(),
                r#type: "frontier".into(),
                api_url: "https://api.anthropic.com/v1".into(),
                api_key_env: "ANTHROPIC_API_KEY".into(),
                models: vec!["claude-sonnet-4-20250514".into()],
                rate_limit: 50,
                monthly_budget: 100.0,
                priority: 50,
                pricing: HashMap::new(),
            },
            ProviderConfig {
                name: "openai".into(),
                r#type: "frontier".into(),
                api_url: "https://api.openai.com/v1".into(),
                api_key_env: "OPENAI_API_KEY".into(),
                models: vec!["gpt-4.1".into()],
                rate_limit: 60,
                monthly_budget: 50.0,
                priority: 40,
                pricing: HashMap::new(),
            },
        ]
    }

    #[test]
    fn resolve_provider_by_model() {
        let providers = test_providers();
        let result = resolve_provider("claude-sonnet-4-20250514", &providers);
        assert!(result.is_some());
        assert_eq!(result.unwrap().name, "anthropic");
    }

    #[test]
    fn resolve_provider_unknown_model() {
        let providers = test_providers();
        assert!(resolve_provider("unknown-model", &providers).is_none());
    }

    #[test]
    fn resolve_provider_by_priority() {
        let mut providers = test_providers();
        // Both serve the same model — should pick higher priority
        providers[1].models.push("shared-model".into());
        providers[0].models.push("shared-model".into());
        let result = resolve_provider("shared-model", &providers);
        assert_eq!(result.unwrap().name, "anthropic"); // priority 50 > 40
    }

    #[test]
    fn is_frontier_model_checks_all_providers() {
        let providers = test_providers();
        assert!(is_frontier_model("claude-sonnet-4-20250514", &providers));
        assert!(is_frontier_model("gpt-4.1", &providers));
        assert!(!is_frontier_model("qwen3:8b", &providers));
    }

    #[test]
    fn adapter_for_provider_returns_correct_type() {
        let anthropic = ProviderConfig { name: "anthropic".into(), api_url: "https://api.anthropic.com/v1".into(), ..Default::default() };
        let openai = ProviderConfig { name: "openai".into(), api_url: "https://api.openai.com/v1".into(), ..Default::default() };
        
        // Anthropic URL → AnthropicAdapter, others → OpenAICompatAdapter
        let a = get_adapter(&anthropic);
        let o = get_adapter(&openai);
        // We can verify by checking auth_header behavior
        assert_eq!(a.auth_header("key"), "key"); // Anthropic raw key
        assert_eq!(o.auth_header("key"), "Bearer key"); // OpenAI Bearer
    }
}
```

- [ ] **Step 2: Run tests — expect FAIL**

- [ ] **Step 3: Implement**

Add to `src/providers/mod.rs`:

```rust
use crate::config::ProviderConfig;

/// Check if a model name belongs to any configured frontier provider
pub fn is_frontier_model(model: &str, providers: &[ProviderConfig]) -> bool {
    providers.iter().any(|p| p.models.contains(&model.to_string()) || p.models.is_empty())
}

/// Find the provider that serves a given model (highest priority wins)
pub fn resolve_provider<'a>(model: &str, providers: &'a [ProviderConfig]) -> Option<&'a ProviderConfig> {
    providers.iter()
        .filter(|p| p.models.contains(&model.to_string()) || p.models.is_empty())
        .max_by_key(|p| p.priority)
}

/// Get the appropriate adapter for a provider (Anthropic vs OpenAI-compat)
pub fn get_adapter(provider: &ProviderConfig) -> Box<dyn ProviderAdapter> {
    if provider.api_url.contains("anthropic.com") {
        Box::new(anthropic::AnthropicAdapter)
    } else {
        Box::new(openai_compat::OpenAICompatAdapter)
    }
}
```

Also add `impl Default for ProviderConfig` in config.rs for test convenience.

- [ ] **Step 4: Run tests — expect PASS**
- [ ] **Step 5: Commit** `feat: add FrontierGateway provider resolution and adapter dispatch`

---

### Task 7: Frontier proxy handler + budget enforcement

**Files:**
- Modify: `src/providers/mod.rs`
- Modify: `src/server.rs`
- Modify: `src/analytics.rs`
- Modify: `src/metrics.rs`

This wires everything together — the actual request proxying through frontier providers.

- [ ] **Step 1: Implement frontier proxy function**

Add to `src/providers/mod.rs`:

```rust
use std::sync::Arc;
use crate::config::FrontierConfig;

/// Result of a frontier proxy request
pub struct FrontierResult {
    pub provider_name: String,
    pub response: reqwest::Response,
    pub cost_usd: Option<f32>,
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
}

/// Proxy a request through a frontier provider.
/// Handles: provider resolution, API key lookup, budget check, request translation, proxying.
pub async fn proxy_frontier_request(
    client: &reqwest::Client,
    frontier_config: &FrontierConfig,
    providers: &[ProviderConfig],
    cost_db: &cost_db::CostDb,
    model: &str,
    body: &serde_json::Value,
    request_id: Option<&str>,
) -> Result<FrontierResult, FrontierError> {
    // 1. Resolve provider
    let provider = resolve_provider(model, providers)
        .ok_or(FrontierError::NoProvider(model.to_string()))?;
    
    // 2. Get API key from env
    let api_key = std::env::var(&provider.api_key_env)
        .map_err(|_| FrontierError::NoApiKey(provider.name.clone(), provider.api_key_env.clone()))?;
    
    // 3. Budget check
    if provider.monthly_budget > 0.0 {
        let spent = cost_db.monthly_spend(&provider.name).unwrap_or(0.0);
        let ratio = spent as f32 / provider.monthly_budget;
        if ratio >= frontier_config.block_threshold {
            return Err(FrontierError::BudgetExceeded {
                provider: provider.name.clone(),
                spent: spent as f32,
                budget: provider.monthly_budget,
            });
        }
        if ratio >= frontier_config.warn_threshold {
            tracing::warn!(
                "Frontier provider '{}' at {:.0}% of monthly budget (${:.2}/${:.2})",
                provider.name, ratio * 100.0, spent, provider.monthly_budget
            );
        }
    }
    
    // 4. Get adapter and translate request
    let adapter = get_adapter(provider);
    let translated = adapter.transform_request(body)?;
    
    // 5. Build and send request
    let url = format!("{}/messages", provider.api_url.trim_end_matches('/'));
    // Anthropic uses /messages, OpenAI uses /chat/completions
    let url = if provider.api_url.contains("anthropic.com") {
        format!("{}/messages", provider.api_url.trim_end_matches('/'))
    } else {
        format!("{}/chat/completions", provider.api_url.trim_end_matches('/'))
    };
    
    let mut req = client.post(&url).json(&translated);
    
    // Set auth header
    if provider.api_url.contains("anthropic.com") {
        req = req.header("x-api-key", &api_key)
            .header("anthropic-version", "2023-06-01");
    } else {
        req = req.header("Authorization", format!("Bearer {}", api_key));
    }
    
    let response = req.send().await
        .map_err(|e| FrontierError::RequestFailed(provider.name.clone(), e.to_string()))?;
    
    Ok(FrontierResult {
        provider_name: provider.name.clone(),
        response,
        cost_usd: None, // Populated after response body is read
        tokens_in: None,
        tokens_out: None,
    })
}

#[derive(Debug)]
pub enum FrontierError {
    NoProvider(String),
    NoApiKey(String, String),
    BudgetExceeded { provider: String, spent: f32, budget: f32 },
    RequestFailed(String, String),
    Other(String),
}

impl std::fmt::Display for FrontierError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoProvider(model) => write!(f, "No frontier provider serves model '{}'", model),
            Self::NoApiKey(provider, env) => write!(f, "API key not found: set {} for provider '{}'", env, provider),
            Self::BudgetExceeded { provider, spent, budget } => write!(f, "Budget exceeded for '{}': ${:.2}/${:.2}", provider, spent, budget),
            Self::RequestFailed(provider, err) => write!(f, "Request to '{}' failed: {}", provider, err),
            Self::Other(msg) => write!(f, "{}", msg),
        }
    }
}
```

- [ ] **Step 2: Add frontier fields to RequestLog**

In `src/analytics.rs`, add:
```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub frontier_provider: Option<String>,
#[serde(default, skip_serializing_if = "Option::is_none")]
pub frontier_cost_usd: Option<f32>,
```

Update all RequestLog construction sites.

- [ ] **Step 3: Add frontier Prometheus metrics**

In `src/metrics.rs`, add counters:
- `herd_frontier_requests_total{provider, model}`
- `herd_frontier_cost_usd_total{provider}`
- `record_frontier_request()` method

- [ ] **Step 4: Wire into proxy handler**

In `src/server.rs`, in the proxy handler after model resolution (and auto classification), add:

```rust
// Check if model is a frontier model
let frontier_config = state.config.read().await.frontier.clone();
let providers = state.config.read().await.providers.clone();
if frontier_config.enabled && crate::providers::is_frontier_model(model_name.as_deref().unwrap_or(""), &providers) {
    // Check require_header
    if frontier_config.require_header {
        let has_header = headers.get("x-herd-frontier")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let is_auto_escalation = auto_classification.as_ref().map(|c| c.tier == "frontier").unwrap_or(false)
            && frontier_config.allow_auto_escalation;
        if !has_header && !is_auto_escalation {
            // Frontier model requested but header not present
            return Err(axum::http::StatusCode::FORBIDDEN);
        }
    }
    
    // Route through frontier gateway instead of local backend pool
    // ... (proxy_frontier_request call)
}
```

- [ ] **Step 5: Add cost_db to AppState**

Initialize alongside node_db — reuse the same SQLite connection or open a separate one.

- [ ] **Step 6: Add GET /api/frontier/costs endpoint**

Wire a handler that calls `cost_db.cost_summary()`.

- [ ] **Step 7: Run full test suite + clippy**
- [ ] **Step 8: Commit** `feat: wire frontier gateway into proxy handler with budget enforcement`

---

### Task 8: Extend /v1/models with frontier models

**Files:**
- Modify: `src/api/openai.rs`

- [ ] **Step 1: Write test**

```rust
#[test]
fn frontier_models_included_when_enabled() {
    // Verify the list_models response structure includes frontier models
    // with herd_provider and herd_type fields
}
```

- [ ] **Step 2: Implement**

In `list_models()`, after collecting local models, append frontier models:

```rust
let config = state.config.read().await;
if config.frontier.enabled {
    for provider in &config.providers {
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
```

- [ ] **Step 3: Run tests + commit** `feat: include frontier models in /v1/models listing`

---

### Task 9: herd.yaml.example + docs

**Files:**
- Modify: `herd.yaml.example`
- Modify: `dashboard.html` (Costs tab — stretch goal)

- [ ] **Step 1: Add frontier + providers config sections to herd.yaml.example**
- [ ] **Step 2: Commit** `docs: add frontier gateway config examples`

---

## Summary

| Task | What | Tests | Complexity |
|------|------|-------|-----------|
| 1 | FrontierConfig + ProviderConfig | 4 | Low |
| 2 | Built-in pricing table | 4 | Low |
| 3 | SQLite cost tracking | 3 | Low |
| 4 | ProviderAdapter trait + OpenAI-compat | 4 | Low |
| 5 | Anthropic adapter | 6 | Medium |
| 6 | Gateway orchestrator (resolve, budget) | 5 | Medium |
| 7 | Proxy handler integration + metrics | 3+ | High |
| 8 | /v1/models frontier listing | 1 | Low |
| 9 | Config examples + docs | 0 | Low |

**Total new tests: ~30**
**New files: 5** (providers/mod.rs, anthropic.rs, openai_compat.rs, pricing.rs, cost_db.rs)
**Modified files: 6** (config.rs, server.rs, api/openai.rs, analytics.rs, metrics.rs, lib.rs)

**Parallelization:** Tasks 1-5 are independent modules. Tasks 6-7 depend on 1-5. Task 8 depends on 1. Task 9 is standalone.
