use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,

    #[serde(default)]
    pub routing: RoutingConfig,

    #[serde(default)]
    pub backends: Vec<Backend>,

    #[serde(default)]
    pub circuit_breaker: CircuitBreakerConfig,

    #[serde(default)]
    pub observability: ObservabilityConfig,

    #[serde(default)]
    pub model_warmer: ModelWarmerConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_host")]
    pub host: String,

    #[serde(default = "default_port")]
    pub port: u16,

    #[serde(default)]
    pub api_key: Option<String>,

    /// Global rate limit in requests per second. 0 = unlimited.
    #[serde(default)]
    pub rate_limit: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            api_key: None,
            rate_limit: 0,
        }
    }
}

fn default_host() -> String {
    "0.0.0.0".to_string()
}
fn default_port() -> u16 {
    40114
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingConfig {
    #[serde(default = "default_strategy")]
    pub strategy: RoutingStrategy,

    #[serde(default = "default_timeout")]
    pub timeout: String,

    #[serde(default = "default_retry_count")]
    pub retry_count: u32,

    // Read by proxy_handler (Task 2) — injected into every /api/generate and /api/chat request.
    #[serde(default = "default_keep_alive_value")]
    pub default_keep_alive: String,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            strategy: default_strategy(),
            timeout: default_timeout(),
            retry_count: default_retry_count(),
            default_keep_alive: default_keep_alive_value(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum RoutingStrategy {
    #[serde(rename = "priority")]
    Priority,

    #[serde(rename = "model_aware")]
    ModelAware,

    #[serde(rename = "least_busy")]
    LeastBusy,

    #[serde(rename = "weighted_round_robin")]
    WeightedRoundRobin,
}

fn default_strategy() -> RoutingStrategy {
    RoutingStrategy::ModelAware
}
fn default_timeout() -> String {
    "120s".to_string()
}
fn default_retry_count() -> u32 {
    2
}
fn default_keep_alive_value() -> String {
    "-1".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Backend {
    pub name: String,
    pub url: String,
    pub priority: u32,

    #[serde(default)]
    pub hot_models: Vec<String>,

    #[serde(default)]
    pub gpu_hot_url: Option<String>,

    #[serde(default)]
    pub model_filter: Option<String>,

    #[serde(default)]
    pub health_check_path: Option<String>,

    #[serde(default)]
    pub health_check_status: Option<u16>,

    #[serde(default)]
    pub tags: Vec<String>,
}

impl Default for Backend {
    fn default() -> Self {
        Self {
            name: String::new(),
            url: String::new(),
            priority: 50,
            hot_models: Vec::new(),
            gpu_hot_url: None,
            model_filter: None,
            health_check_path: None,
            health_check_status: None,
            tags: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelWarmerConfig {
    #[serde(default = "default_warmer_interval")]
    pub interval_secs: u64,
}

impl Default for ModelWarmerConfig {
    fn default() -> Self {
        Self { interval_secs: default_warmer_interval() }
    }
}

fn default_warmer_interval() -> u64 {
    240
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CircuitBreakerConfig {
    #[serde(default = "default_failure_threshold")]
    pub failure_threshold: u32,

    #[serde(default = "default_timeout")]
    pub timeout: String,

    #[serde(default = "default_recovery_time")]
    pub recovery_time: String,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: default_failure_threshold(),
            timeout: default_timeout(),
            recovery_time: default_recovery_time(),
        }
    }
}

fn default_failure_threshold() -> u32 {
    5
}
fn default_recovery_time() -> String {
    "30s".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservabilityConfig {
    #[serde(default = "default_true")]
    pub metrics: bool,

    #[serde(default)]
    pub admin_api: bool,

    #[serde(default)]
    pub tracing: bool,

    /// Log retention in days (default: 7)
    #[serde(default = "default_log_retention_days")]
    pub log_retention_days: u64,

    /// Max log file size in MB before rotation (default: 100, 0 = no limit)
    #[serde(default = "default_log_max_size_mb")]
    pub log_max_size_mb: u64,

    /// Max number of rotated log files to keep (default: 5)
    #[serde(default = "default_log_max_files")]
    pub log_max_files: u32,
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            metrics: true,
            admin_api: false,
            tracing: false,
            log_retention_days: default_log_retention_days(),
            log_max_size_mb: default_log_max_size_mb(),
            log_max_files: default_log_max_files(),
        }
    }
}

fn default_true() -> bool {
    true
}
fn default_log_retention_days() -> u64 {
    7
}
fn default_log_max_size_mb() -> u64 {
    100
}
fn default_log_max_files() -> u32 {
    5
}

impl Config {
    pub fn from_file(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path)?;
        let config: Config = serde_yaml::from_str(&content)?;
        Ok(config)
    }

    pub fn to_yaml(&self) -> Result<String> {
        Ok(serde_yaml::to_string(self)?)
    }
}

pub fn parse_duration(input: &str) -> Result<Duration> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        anyhow::bail!("duration is empty");
    }
    let split_at = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(trimmed.len());
    let (number, suffix) = trimmed.split_at(split_at);
    if number.is_empty() {
        anyhow::bail!("duration has no numeric component: {}", input);
    }
    let value = number.parse::<u64>()?;
    let duration = match suffix {
        "" | "s" => Duration::from_secs(value),
        "ms" => Duration::from_millis(value),
        "m" => Duration::from_secs(value.saturating_mul(60)),
        "h" => Duration::from_secs(value.saturating_mul(3600)),
        _ => anyhow::bail!("unsupported duration suffix '{}': {}", suffix, input),
    };
    Ok(duration)
}

#[cfg(test)]
mod tests {
    use super::parse_duration;
    use super::Config;
    use std::time::Duration;

    #[test]
    fn parse_duration_seconds() {
        assert_eq!(parse_duration("120s").unwrap(), Duration::from_secs(120));
    }
    #[test]
    fn parse_duration_minutes() {
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
    }
    #[test]
    fn parse_duration_millis() {
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
    }

    #[test]
    fn routing_default_keep_alive_is_negative_one() {
        let config: Config = serde_yaml::from_str("{}").unwrap();
        assert_eq!(config.routing.default_keep_alive, "-1");
    }

    #[test]
    fn routing_keep_alive_configurable() {
        let yaml = "routing:\n  default_keep_alive: \"1h\"\n";
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.routing.default_keep_alive, "1h");
    }

    #[test]
    fn model_warmer_default_interval() {
        let config: Config = serde_yaml::from_str("{}").unwrap();
        assert_eq!(config.model_warmer.interval_secs, 240);
    }

    #[test]
    fn backend_hot_models_defaults_empty() {
        let yaml = "backends:\n  - name: x\n    url: http://x\n    priority: 50\n";
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(config.backends[0].hot_models.is_empty());
    }

    #[test]
    fn old_default_model_field_silently_ignored() {
        let yaml = "backends:\n  - name: x\n    url: http://x\n    priority: 50\n    default_model: llama3:8b\n";
        let result: Result<Config, _> = serde_yaml::from_str(yaml);
        assert!(result.is_ok());
    }
}
