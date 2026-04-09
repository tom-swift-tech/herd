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

    #[serde(default)]
    pub routing_profiles: crate::profiles::RoutingProfilesConfig,

    #[serde(default)]
    pub tls: TlsConfig,

    #[serde(default)]
    pub rate_limiting: RateLimitConfig,

    #[serde(default)]
    pub budget: BudgetConfig,

    #[serde(default)]
    pub discovery: DiscoveryConfig,
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

    #[serde(default)]
    pub auto: AutoRoutingConfig,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            strategy: default_strategy(),
            timeout: default_timeout(),
            retry_count: default_retry_count(),
            default_keep_alive: default_keep_alive_value(),
            auto: AutoRoutingConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoRoutingConfig {
    #[serde(default)]
    pub enabled: bool,

    #[serde(default = "default_classifier_model")]
    pub classifier_model: String,

    #[serde(default)]
    pub classifier_backend: Option<String>,

    #[serde(default = "default_classifier_timeout")]
    pub classifier_timeout_ms: u64,

    #[serde(default)]
    pub fallback_model: String,

    #[serde(default = "default_cache_ttl")]
    pub cache_ttl_secs: u64,

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum BackendType {
    #[default]
    #[serde(rename = "ollama")]
    Ollama,

    #[serde(rename = "llama-server")]
    LlamaServer,

    #[serde(rename = "openai-compat")]
    OpenAICompat,
}

impl std::fmt::Display for BackendType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackendType::Ollama => write!(f, "ollama"),
            BackendType::LlamaServer => write!(f, "llama-server"),
            BackendType::OpenAICompat => write!(f, "openai-compat"),
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

    #[serde(default)]
    pub backend: BackendType,

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

impl Backend {
    /// Default health check path based on backend type.
    pub fn default_health_check_path(&self) -> &str {
        match self.backend {
            BackendType::LlamaServer => "/health",
            BackendType::OpenAICompat => "/v1/models",
            BackendType::Ollama => "/",
        }
    }
}

impl Default for Backend {
    fn default() -> Self {
        Self {
            name: String::new(),
            url: String::new(),
            backend: BackendType::default(),
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PermissionsConfig {
    #[serde(default)]
    pub deny_file_patterns: Vec<String>,

    #[serde(default)]
    pub deny_bash_patterns: Vec<String>,

    #[serde(default)]
    pub allow_shell_commands: bool,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsConfig {
    #[serde(default)]
    pub enabled: bool,

    #[serde(default)]
    pub cert_path: Option<String>,

    #[serde(default)]
    pub key_path: Option<String>,

    /// Redirect HTTP to HTTPS on a separate port
    #[serde(default)]
    pub redirect_http: bool,

    /// Port for HTTP redirect listener (default: 80)
    #[serde(default = "default_redirect_port")]
    pub redirect_port: u16,
}

impl Default for TlsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            cert_path: None,
            key_path: None,
            redirect_http: false,
            redirect_port: default_redirect_port(),
        }
    }
}

fn default_redirect_port() -> u16 {
    80
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RateLimitConfig {
    /// Global rate limit (requests/sec). 0 = unlimited. Applies to unauthenticated requests.
    #[serde(default)]
    pub global: u64,

    /// Per-client rate limits by API key name.
    #[serde(default)]
    pub clients: Vec<ClientRateLimit>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientRateLimit {
    /// API key for this client
    pub api_key: String,

    /// Requests per second (0 = unlimited)
    #[serde(default)]
    pub rate_limit: u64,

    /// Optional descriptive name
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetConfig {
    #[serde(default)]
    pub enabled: bool,

    /// Global budget cap in USD. 0.0 = unlimited.
    #[serde(default)]
    pub global_limit_usd: f32,

    /// Per-client budget caps (keyed by client name from X-Herd-Client header).
    #[serde(default)]
    pub clients: HashMap<String, f32>,

    /// Per-model budget caps.
    #[serde(default)]
    pub models: HashMap<String, f32>,

    /// Reset period: "daily", "weekly", "monthly". Default: "monthly".
    #[serde(default = "default_budget_period")]
    pub reset_period: String,

    /// Action when budget exceeded: "reject" (429) or "warn" (log + allow).
    #[serde(default = "default_budget_action")]
    pub action: String,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            global_limit_usd: 0.0,
            clients: HashMap::new(),
            models: HashMap::new(),
            reset_period: default_budget_period(),
            action: default_budget_action(),
        }
    }
}

fn default_budget_period() -> String {
    "monthly".to_string()
}
fn default_budget_action() -> String {
    "reject".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryConfig {
    #[serde(default)]
    pub enabled: bool,

    /// Static fleet: list of known Herd node URLs to probe and register
    #[serde(default)]
    pub static_nodes: Vec<StaticNodeConfig>,

    /// mDNS discovery settings
    #[serde(default)]
    pub mdns: MdnsConfig,

    /// How often to re-probe static nodes (seconds). Default: 60.
    #[serde(default = "default_probe_interval")]
    pub probe_interval_secs: u64,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            static_nodes: Vec::new(),
            mdns: MdnsConfig::default(),
            probe_interval_secs: default_probe_interval(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaticNodeConfig {
    /// URL of the remote node's API (e.g., "http://192.168.1.100:8090")
    pub url: String,
    /// Backend type: ollama, llama-server, openai-compat
    #[serde(default)]
    pub backend: BackendType,
    /// Optional hostname override
    #[serde(default)]
    pub hostname: Option<String>,
    /// Tags to apply to this node
    #[serde(default)]
    pub tags: Vec<String>,
    /// Priority override
    #[serde(default = "default_discovery_priority")]
    pub priority: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MdnsConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Service name for mDNS. Default: "_herd._tcp.local."
    #[serde(default = "default_mdns_service")]
    pub service_name: String,
    /// Whether to broadcast this instance. Default: true when mDNS enabled.
    #[serde(default = "default_true")]
    pub broadcast: bool,
    /// Whether to listen for other instances. Default: true when mDNS enabled.
    #[serde(default = "default_true")]
    pub listen: bool,
}

fn default_probe_interval() -> u64 {
    60
}
fn default_discovery_priority() -> u32 {
    50
}
fn default_mdns_service() -> String {
    "_herd._tcp.local.".to_string()
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

    /// Returns the effective global rate limit.
    /// If `rate_limiting.global` is set (non-zero), it takes precedence.
    /// Otherwise falls back to the legacy `server.rate_limit` field.
    pub fn effective_global_rate_limit(&self) -> u64 {
        if self.rate_limiting.global > 0 {
            self.rate_limiting.global
        } else {
            self.server.rate_limit
        }
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
    use super::BackendType;
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

    #[test]
    fn backend_type_defaults_to_ollama() {
        let yaml = "backends:\n  - name: x\n    url: http://x\n    priority: 50\n";
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.backends[0].backend, BackendType::Ollama);
    }

    #[test]
    fn backend_type_llama_server_parses() {
        let yaml = "backends:\n  - name: x\n    url: http://x\n    priority: 50\n    backend: llama-server\n";
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.backends[0].backend, BackendType::LlamaServer);
    }

    #[test]
    fn backend_type_ollama_explicit() {
        let yaml =
            "backends:\n  - name: x\n    url: http://x\n    priority: 50\n    backend: ollama\n";
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.backends[0].backend, BackendType::Ollama);
    }

    #[test]
    fn backend_type_openai_compat_parses() {
        let yaml = "backends:\n  - name: x\n    url: http://x\n    priority: 50\n    backend: openai-compat\n";
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.backends[0].backend, BackendType::OpenAICompat);
    }

    #[test]
    fn backend_type_openai_compat_display() {
        assert_eq!(BackendType::OpenAICompat.to_string(), "openai-compat");
    }

    #[test]
    fn openai_compat_default_health_check_path() {
        let b = super::Backend {
            name: "compat1".into(),
            url: "http://localhost:8080".into(),
            backend: BackendType::OpenAICompat,
            priority: 50,
            ..Default::default()
        };
        assert_eq!(b.default_health_check_path(), "/v1/models");
    }

    #[test]
    fn backend_type_round_trip_serialize() {
        for bt in [
            BackendType::Ollama,
            BackendType::LlamaServer,
            BackendType::OpenAICompat,
        ] {
            let json = serde_json::to_string(&bt).unwrap();
            let deserialized: BackendType = serde_json::from_str(&json).unwrap();
            assert_eq!(bt, deserialized);
        }
    }

    #[test]
    fn config_without_backend_field_defaults_to_ollama() {
        // Backward compat: existing YAML configs without `backend` field
        let yaml = r#"
server:
  host: 0.0.0.0
  port: 40114
backends:
  - name: gpu1
    url: http://localhost:11434
    priority: 100
  - name: gpu2
    url: http://192.168.1.50:11434
    priority: 50
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.backends.len(), 2);
        assert_eq!(config.backends[0].backend, BackendType::Ollama);
        assert_eq!(config.backends[1].backend, BackendType::Ollama);
    }

    #[test]
    fn tls_config_defaults() {
        let tls = super::TlsConfig::default();
        assert!(!tls.enabled);
        assert!(tls.cert_path.is_none());
        assert!(tls.key_path.is_none());
        assert!(!tls.redirect_http);
        assert_eq!(tls.redirect_port, 80);
    }

    #[test]
    fn tls_config_deserializes_from_yaml() {
        let yaml = r#"
tls:
  enabled: true
  cert_path: /etc/ssl/cert.pem
  key_path: /etc/ssl/key.pem
  redirect_http: true
  redirect_port: 8080
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(config.tls.enabled);
        assert_eq!(config.tls.cert_path.as_deref(), Some("/etc/ssl/cert.pem"));
        assert_eq!(config.tls.key_path.as_deref(), Some("/etc/ssl/key.pem"));
        assert!(config.tls.redirect_http);
        assert_eq!(config.tls.redirect_port, 8080);
    }

    #[test]
    fn config_without_tls_section_backward_compat() {
        let yaml = r#"
server:
  host: 0.0.0.0
  port: 40114
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(!config.tls.enabled);
        assert!(config.tls.cert_path.is_none());
    }

    #[test]
    fn routing_profiles_defaults_disabled() {
        let config: Config = serde_yaml::from_str("{}").unwrap();
        assert!(!config.routing_profiles.enabled);
        assert!(config.routing_profiles.default_profile.is_none());
        assert!(config.routing_profiles.profiles.is_empty());
    }

    #[test]
    fn routing_profiles_deserializes_from_yaml() {
        let yaml = r#"
routing_profiles:
  enabled: true
  default_profile: balanced
  profiles:
    fast:
      strategy: priority
      description: "Fastest response"
    balanced:
      strategy: least_busy
      tags:
        - gpu
      backends:
        - local-ollama
      preferred_model: "llama3:8b"
      description: "Balanced"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(config.routing_profiles.enabled);
        assert_eq!(
            config.routing_profiles.default_profile.as_deref(),
            Some("balanced")
        );
        assert_eq!(config.routing_profiles.profiles.len(), 2);

        let fast = &config.routing_profiles.profiles["fast"];
        assert_eq!(fast.strategy, super::RoutingStrategy::Priority);
        assert!(fast.tags.is_empty());
        assert!(fast.backends.is_empty());
        assert!(fast.preferred_model.is_none());
        assert_eq!(fast.description.as_deref(), Some("Fastest response"));

        let balanced = &config.routing_profiles.profiles["balanced"];
        assert_eq!(balanced.strategy, super::RoutingStrategy::LeastBusy);
        assert_eq!(balanced.tags, vec!["gpu"]);
        assert_eq!(balanced.backends, vec!["local-ollama"]);
        assert_eq!(balanced.preferred_model.as_deref(), Some("llama3:8b"));
    }

    #[test]
    fn config_without_routing_profiles_backward_compat() {
        let yaml = r#"
server:
  host: 0.0.0.0
  port: 40114
routing:
  strategy: priority
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(!config.routing_profiles.enabled);
        assert!(config.routing_profiles.profiles.is_empty());
    }

    #[test]
    fn discovery_config_defaults_disabled() {
        let config: Config = serde_yaml::from_str("{}").unwrap();
        assert!(!config.discovery.enabled);
        assert!(config.discovery.static_nodes.is_empty());
        assert!(!config.discovery.mdns.enabled);
        assert_eq!(config.discovery.probe_interval_secs, 60);
    }

    #[test]
    fn discovery_config_deserializes_static_nodes() {
        let yaml = r#"
discovery:
  enabled: true
  probe_interval_secs: 30
  static_nodes:
    - url: http://192.168.1.100:8090
      backend: llama-server
      tags: [gpu, nvidia]
      priority: 10
    - url: http://192.168.1.101:11434
      backend: ollama
      hostname: minipc
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(config.discovery.enabled);
        assert_eq!(config.discovery.probe_interval_secs, 30);
        assert_eq!(config.discovery.static_nodes.len(), 2);

        let n0 = &config.discovery.static_nodes[0];
        assert_eq!(n0.url, "http://192.168.1.100:8090");
        assert_eq!(n0.backend, BackendType::LlamaServer);
        assert_eq!(n0.tags, vec!["gpu", "nvidia"]);
        assert_eq!(n0.priority, 10);
        assert!(n0.hostname.is_none());

        let n1 = &config.discovery.static_nodes[1];
        assert_eq!(n1.url, "http://192.168.1.101:11434");
        assert_eq!(n1.backend, BackendType::Ollama);
        assert_eq!(n1.hostname.as_deref(), Some("minipc"));
        assert_eq!(n1.priority, 50); // default
    }

    #[test]
    fn discovery_config_mdns_defaults() {
        let yaml = r#"
discovery:
  enabled: true
  mdns:
    enabled: true
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(config.discovery.mdns.enabled);
        assert_eq!(config.discovery.mdns.service_name, "_herd._tcp.local.");
        assert!(config.discovery.mdns.broadcast);
        assert!(config.discovery.mdns.listen);
    }

    #[test]
    fn config_without_discovery_section_backward_compat() {
        let yaml = r#"
server:
  host: 0.0.0.0
  port: 40114
routing:
  strategy: priority
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(!config.discovery.enabled);
        assert!(config.discovery.static_nodes.is_empty());
    }

    #[test]
    fn rate_limit_config_deserializes_from_yaml() {
        let yaml = r#"
rate_limiting:
  global: 100
  clients:
    - name: my-agent
      api_key: sk-agent-12345
      rate_limit: 50
    - name: dashboard
      api_key: sk-dash-67890
      rate_limit: 200
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.rate_limiting.global, 100);
        assert_eq!(config.rate_limiting.clients.len(), 2);
        assert_eq!(config.rate_limiting.clients[0].api_key, "sk-agent-12345");
        assert_eq!(config.rate_limiting.clients[0].rate_limit, 50);
        assert_eq!(
            config.rate_limiting.clients[0].name.as_deref(),
            Some("my-agent")
        );
        assert_eq!(config.rate_limiting.clients[1].rate_limit, 200);
    }

    #[test]
    fn rate_limit_config_defaults_to_disabled() {
        let config: Config = serde_yaml::from_str("{}").unwrap();
        assert_eq!(config.rate_limiting.global, 0);
        assert!(config.rate_limiting.clients.is_empty());
    }

    #[test]
    fn effective_global_rate_limit_prefers_rate_limiting_global() {
        let yaml = r#"
server:
  rate_limit: 50
rate_limiting:
  global: 100
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.effective_global_rate_limit(), 100);
    }

    #[test]
    fn effective_global_rate_limit_falls_back_to_server_rate_limit() {
        let yaml = r#"
server:
  rate_limit: 50
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.effective_global_rate_limit(), 50);
    }

    #[test]
    fn backward_compat_old_config_without_rate_limiting() {
        let yaml = r#"
server:
  host: 0.0.0.0
  port: 40114
  rate_limit: 10
backends:
  - name: gpu1
    url: http://localhost:11434
    priority: 100
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.server.rate_limit, 10);
        assert_eq!(config.rate_limiting.global, 0);
        assert_eq!(config.effective_global_rate_limit(), 10);
    }

    #[test]
    fn budget_config_defaults_disabled() {
        let config: Config = serde_yaml::from_str("{}").unwrap();
        assert!(!config.budget.enabled);
        assert!((config.budget.global_limit_usd).abs() < 0.001);
        assert_eq!(config.budget.reset_period, "monthly");
        assert_eq!(config.budget.action, "reject");
        assert!(config.budget.clients.is_empty());
        assert!(config.budget.models.is_empty());
    }

    #[test]
    fn budget_config_deserializes_from_yaml() {
        let yaml = r#"
budget:
  enabled: true
  global_limit_usd: 50.0
  reset_period: daily
  action: warn
  clients:
    agent-team: 20.0
  models:
    "llama3:70b": 30.0
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(config.budget.enabled);
        assert!((config.budget.global_limit_usd - 50.0).abs() < 0.001);
        assert_eq!(config.budget.reset_period, "daily");
        assert_eq!(config.budget.action, "warn");
        assert!((config.budget.clients["agent-team"] - 20.0).abs() < 0.001);
        assert!((config.budget.models["llama3:70b"] - 30.0).abs() < 0.001);
    }

    #[test]
    fn budget_config_backward_compat_no_budget_section() {
        let yaml = r#"
server:
  host: 0.0.0.0
backends:
  - name: gpu1
    url: http://localhost:11434
    priority: 100
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(!config.budget.enabled);
    }

    #[test]
    fn auto_routing_config_defaults() {
        let config: super::AutoRoutingConfig = Default::default();
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
        let config: super::AutoRoutingConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(config.enabled);
        assert_eq!(config.classifier_timeout_ms, 2000);
        assert_eq!(config.model_map.len(), 4);
        assert_eq!(config.model_map["standard"]["code"], "qwen2.5-coder:32b");
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
}
