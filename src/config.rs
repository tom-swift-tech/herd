use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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

    #[serde(default)]
    pub task_classifier: TaskClassifierConfig,

    #[serde(default)]
    pub agent: AgentConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_host")]
    pub host: String,

    #[serde(default = "default_port")]
    pub port: u16,

    #[serde(default)]
    pub api_key: Option<String>,

    /// Enrollment key required for node registration. Auto-generated if not set.
    #[serde(default)]
    pub enrollment_key: Option<String>,

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
            enrollment_key: None,
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

impl std::fmt::Display for RoutingStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RoutingStrategy::Priority => write!(f, "priority"),
            RoutingStrategy::ModelAware => write!(f, "model_aware"),
            RoutingStrategy::LeastBusy => write!(f, "least_busy"),
            RoutingStrategy::WeightedRoundRobin => write!(f, "weighted_round_robin"),
        }
    }
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
    "5m".to_string()
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

    /// Per-model timeout in seconds for warming requests (default: 180).
    /// Large models (20GB+) may need 60–180s to load into VRAM.
    #[serde(default = "default_warmer_timeout")]
    pub timeout_secs: u64,
}

impl Default for ModelWarmerConfig {
    fn default() -> Self {
        Self {
            interval_secs: default_warmer_interval(),
            timeout_secs: default_warmer_timeout(),
        }
    }
}

fn default_warmer_interval() -> u64 {
    240
}
fn default_warmer_timeout() -> u64 {
    180
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

/// Deprecated config keys removed in v0.4.3 with their replacement guidance.
const DEPRECATED_KEYS: &[(&str, &str)] = &[
    ("default_model", "Config key 'default_model' is no longer supported (removed in v0.4.3). Use 'hot_models' instead. This setting has no effect."),
    ("idle_timeout_minutes", "Config key 'idle_timeout_minutes' is no longer supported (removed in v0.4.3). This setting has no effect."),
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskClassifierConfig {
    #[serde(default)]
    pub enabled: bool,

    #[serde(default = "default_classifier_strategy")]
    pub strategy: String,

    #[serde(default = "default_classifier_tier")]
    pub default_tier: String,

    #[serde(default)]
    pub tiers: HashMap<String, TierConfig>,
}

impl Default for TaskClassifierConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            strategy: default_classifier_strategy(),
            default_tier: default_classifier_tier(),
            tiers: HashMap::new(),
        }
    }
}

fn default_classifier_strategy() -> String {
    "keyword".to_string()
}

fn default_classifier_tier() -> String {
    "standard".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TierConfig {
    #[serde(default)]
    pub keywords: Vec<String>,
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    #[serde(default)]
    pub enabled: bool,

    #[serde(default = "default_max_sessions")]
    pub max_sessions: usize,

    #[serde(default = "default_max_tool_rounds")]
    pub max_tool_rounds: u32,

    #[serde(default)]
    pub default_model: Option<String>,

    #[serde(default)]
    pub permissions: PermissionsConfig,

    #[serde(default = "default_session_ttl")]
    pub session_ttl_minutes: u64,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_sessions: default_max_sessions(),
            max_tool_rounds: default_max_tool_rounds(),
            default_model: None,
            permissions: PermissionsConfig::default(),
            session_ttl_minutes: default_session_ttl(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionsConfig {
    #[serde(default)]
    pub deny_file_patterns: Vec<String>,

    #[serde(default)]
    pub deny_bash_patterns: Vec<String>,

    #[serde(default)]
    pub allow_shell_commands: bool,
}

impl Default for PermissionsConfig {
    fn default() -> Self {
        Self {
            deny_file_patterns: Vec::new(),
            deny_bash_patterns: Vec::new(),
            allow_shell_commands: false,
        }
    }
}

fn default_max_sessions() -> usize {
    100
}
fn default_max_tool_rounds() -> u32 {
    10
}
fn default_session_ttl() -> u64 {
    60
}

impl Config {
    pub fn from_file(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path)?;

        // First pass: check for deprecated keys in raw YAML
        if let Ok(raw) = serde_yaml::from_str::<serde_yaml::Value>(&content) {
            Self::warn_deprecated_keys(&raw, &[]);
        }

        let config: Config = serde_yaml::from_str(&content)?;
        Ok(config)
    }

    fn warn_deprecated_keys(value: &serde_yaml::Value, path: &[String]) {
        if let serde_yaml::Value::Mapping(map) = value {
            for (k, v) in map {
                if let serde_yaml::Value::String(key) = k {
                    for &(deprecated, message) in DEPRECATED_KEYS {
                        if key == deprecated {
                            let location = if path.is_empty() {
                                key.clone()
                            } else {
                                format!("{}.{}", path.join("."), key)
                            };
                            tracing::warn!("{} (found at '{}')", message, location);
                        }
                    }
                    let mut child_path = path.to_vec();
                    child_path.push(key.clone());
                    Self::warn_deprecated_keys(v, &child_path);
                }
            }
        }
        if let serde_yaml::Value::Sequence(seq) = value {
            for (i, item) in seq.iter().enumerate() {
                let mut child_path = path.to_vec();
                child_path.push(format!("[{}]", i));
                Self::warn_deprecated_keys(item, &child_path);
            }
        }
    }

    pub fn validate(&self) -> Result<()> {
        // Validate model_warmer interval
        if self.model_warmer.interval_secs > 0 && self.model_warmer.interval_secs < 10 {
            anyhow::bail!(
                "model_warmer.interval_secs must be >= 10 (got {})",
                self.model_warmer.interval_secs
            );
        }

        // Validate backend URLs
        for (i, backend) in self.backends.iter().enumerate() {
            if backend.url.is_empty() {
                anyhow::bail!("backends[{}] ('{}') has an empty URL", i, backend.name);
            }
            if !backend.url.starts_with("http://") && !backend.url.starts_with("https://") {
                anyhow::bail!(
                    "backends[{}] ('{}') has an invalid URL (must start with http:// or https://): '{}'",
                    i,
                    backend.name,
                    backend.url
                );
            }
        }

        // Warn if recovery_time <= timeout on circuit breaker
        if let (Ok(recovery), Ok(timeout)) = (
            parse_duration(&self.circuit_breaker.recovery_time),
            parse_duration(&self.circuit_breaker.timeout),
        ) {
            if recovery <= timeout {
                tracing::warn!(
                    "circuit_breaker.recovery_time ({}) should be greater than circuit_breaker.timeout ({}) for effective recovery",
                    self.circuit_breaker.recovery_time,
                    self.circuit_breaker.timeout
                );
            }
        }

        Ok(())
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
    fn routing_default_keep_alive_is_5m() {
        let config: Config = serde_yaml::from_str("{}").unwrap();
        assert_eq!(config.routing.default_keep_alive, "5m");
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
