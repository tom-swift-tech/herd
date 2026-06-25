use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
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

    #[serde(default)]
    pub fleet: FleetConfig,

    #[serde(default)]
    pub frontier: FrontierConfig,

    #[serde(default)]
    pub providers: Vec<ProviderConfig>,

    /// Root directory for all gateway data stores (node DB, analytics, audit,
    /// sessions, costs, published binaries). Env `HERD_DATA_DIR` wins over this
    /// field. Defaults to `~/.herd` when neither is set — byte-identical to the
    /// pre-v1.2 behaviour, so existing deployments are unaffected.
    #[serde(default)]
    pub data_dir: Option<String>,
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

    /// Config for the Scored routing strategy. Omitting the block applies all defaults.
    #[serde(default)]
    pub scored: ScoredConfig,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            strategy: default_strategy(),
            timeout: default_timeout(),
            retry_count: default_retry_count(),
            default_keep_alive: default_keep_alive_value(),
            auto: AutoRoutingConfig::default(),
            scored: ScoredConfig::default(),
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

    #[serde(rename = "scored")]
    Scored,
}

impl std::fmt::Display for RoutingStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RoutingStrategy::Priority => write!(f, "priority"),
            RoutingStrategy::ModelAware => write!(f, "model_aware"),
            RoutingStrategy::LeastBusy => write!(f, "least_busy"),
            RoutingStrategy::WeightedRoundRobin => write!(f, "weighted_round_robin"),
            RoutingStrategy::Scored => write!(f, "scored"),
        }
    }
}

// ── Scored router config ─────────────────────────────────────────────────────

/// Gate mode for model-residency check in the Scored router.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ModelGate {
    /// (Default) Relax the model predicate once if no resident candidate exists —
    /// parity with `model_aware`'s "never 503 just because the model isn't loaded."
    #[default]
    #[serde(rename = "relaxed")]
    Relaxed,
    /// Hard gate: if no backend has the model resident, return Err (→ 503).
    #[serde(rename = "strict")]
    Strict,
}

/// Per-dimension weights for the Scored router.
/// Any omitted key in YAML falls back to its default fn value.
/// Field names are byte-for-byte identical to the YAML keys and catalog names.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ScoredWeights {
    // Group A — model & placement (Phase 1, active)
    #[serde(default = "w_model_resident")]
    pub model_resident: f64,
    #[serde(default = "w_model_fits_vram")]
    pub model_fits_vram: f64,
    #[serde(default = "w_prompt_size_vs_capacity")]
    pub prompt_size_vs_capacity: f64,
    // Group B — GPU pressure (Phase 1, active)
    #[serde(default = "w_gpu_utilization")]
    pub gpu_utilization: f64,
    #[serde(default = "w_vram_headroom")]
    pub vram_headroom: f64,
    #[serde(default = "w_gpu_temperature")]
    pub gpu_temperature: f64,
    // Group C — operator intent & affinity (Phase 1, active)
    #[serde(default = "w_operator_priority")]
    pub operator_priority: f64,
    #[serde(default = "w_tag_affinity")]
    pub tag_affinity: f64,
    #[serde(default = "w_zero")]
    pub backend_type_affinity: f64,
    // Group D — live load (Phase 2). All active (latency-aware balanced defaults).
    // concurrency_saturation (dim 12) carries a lower weight than queue_depth
    // (dim 10): the two coexist (absolute vs capacity-relative queue pressure).
    #[serde(default = "w_queue_depth")]
    pub queue_depth: f64,
    #[serde(default = "w_ttft_p50")]
    pub ttft_p50: f64,
    #[serde(default = "w_concurrency_saturation")]
    pub concurrency_saturation: f64,
    #[serde(default = "w_precise_vram_free")]
    pub precise_vram_free: f64,
    // Group E — history & stability (Phase 3). Active defaults: latency and
    // reliability signals carry high weight; throughput moderate; stability lower.
    #[serde(default = "w_ewma_latency")]
    pub ewma_latency: f64,
    #[serde(default = "w_recent_error_rate")]
    pub recent_error_rate: f64,
    #[serde(default = "w_recent_success_throughput")]
    pub recent_success_throughput: f64,
    #[serde(default = "w_flap_stability")]
    pub flap_stability: f64,
    #[serde(default = "w_zero")]
    pub session_stickiness: f64,
    #[serde(default = "w_zero")]
    pub network_locality: f64,
    #[serde(default = "w_zero")]
    pub power_cost: f64,
    #[serde(default = "w_zero")]
    pub rpc_shard_capability: f64,
    #[serde(default = "w_zero")]
    pub gpu_class_affinity: f64,
    #[serde(default = "w_zero")]
    pub warm_model_recency: f64,
}

impl Default for ScoredWeights {
    fn default() -> Self {
        Self {
            model_resident: w_model_resident(),
            model_fits_vram: w_model_fits_vram(),
            prompt_size_vs_capacity: w_prompt_size_vs_capacity(),
            gpu_utilization: w_gpu_utilization(),
            vram_headroom: w_vram_headroom(),
            gpu_temperature: w_gpu_temperature(),
            operator_priority: w_operator_priority(),
            tag_affinity: w_tag_affinity(),
            backend_type_affinity: w_zero(),
            queue_depth: w_queue_depth(),
            ttft_p50: w_ttft_p50(),
            concurrency_saturation: w_concurrency_saturation(),
            precise_vram_free: w_precise_vram_free(),
            ewma_latency: w_ewma_latency(),
            recent_error_rate: w_recent_error_rate(),
            recent_success_throughput: w_recent_success_throughput(),
            flap_stability: w_flap_stability(),
            session_stickiness: w_zero(),
            network_locality: w_zero(),
            power_cost: w_zero(),
            rpc_shard_capability: w_zero(),
            gpu_class_affinity: w_zero(),
            warm_model_recency: w_zero(),
        }
    }
}

// Weight default fns — each matches the defaults table in the spec.
fn w_model_resident() -> f64 {
    5.0
}
fn w_model_fits_vram() -> f64 {
    2.0
}
fn w_prompt_size_vs_capacity() -> f64 {
    1.0
}
fn w_gpu_utilization() -> f64 {
    3.0
}
fn w_vram_headroom() -> f64 {
    2.0
}
fn w_gpu_temperature() -> f64 {
    1.0
}
fn w_operator_priority() -> f64 {
    2.0
}
fn w_tag_affinity() -> f64 {
    1.0
}
// Group D — live load (Phase 2 Slice 1). Latency-aware balanced posture: the
// measured live-load signals sit at parity with the sampled gpu_utilization (3.0)
// and vram_headroom (2.0) rather than dominating operator intent / model placement.
fn w_queue_depth() -> f64 {
    2.0
}
fn w_ttft_p50() -> f64 {
    3.0
}
fn w_concurrency_saturation() -> f64 {
    1.0
}
fn w_precise_vram_free() -> f64 {
    2.0
}
// Group E — history & stability (Phase 3).
// Latency + error rate: high weight (3.0 each) — the primary reliability signal.
// Throughput: moderate (2.0) — useful when latency is similar.
// Flap stability: lower (1.0) — flapping matters but is rarer.
fn w_ewma_latency() -> f64 {
    3.0
}
fn w_recent_error_rate() -> f64 {
    3.0
}
fn w_recent_success_throughput() -> f64 {
    2.0
}
fn w_flap_stability() -> f64 {
    1.0
}
fn w_zero() -> f64 {
    0.0
}

/// Known weight field names (catalog order, byte-identical to YAML keys).
/// Used by `unknown_weight_keys` to warn on unrecognized config entries.
pub const SCORED_WEIGHT_FIELD_NAMES: &[&str] = &[
    "model_resident",
    "model_fits_vram",
    "prompt_size_vs_capacity",
    "gpu_utilization",
    "vram_headroom",
    "gpu_temperature",
    "operator_priority",
    "tag_affinity",
    "backend_type_affinity",
    "queue_depth",
    "ttft_p50",
    "concurrency_saturation",
    "precise_vram_free",
    "ewma_latency",
    "recent_error_rate",
    "recent_success_throughput",
    "flap_stability",
    "session_stickiness",
    "network_locality",
    "power_cost",
    "rpc_shard_capability",
    "gpu_class_affinity",
    "warm_model_recency",
];

/// Return the keys in `mapping` that are not recognised weight field names.
/// The caller is responsible for issuing `warn!` messages.
pub fn unknown_weight_keys(mapping: &serde_yaml::Mapping) -> Vec<String> {
    mapping
        .keys()
        .filter_map(|k| k.as_str())
        .filter(|k| !SCORED_WEIGHT_FIELD_NAMES.contains(k))
        .map(|k| k.to_owned())
        .collect()
}

/// Config block for the Scored routing strategy.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScoredConfig {
    /// Gate mode for model-residency filter. Default `relaxed`.
    #[serde(default)]
    pub model_gate: ModelGate,
    /// Optional soft preference for a backend type (feeds dim 9).
    /// `None` → dim 9 not present (neutral) for every backend. Never a gate.
    #[serde(default)]
    pub prefer_backend_type: Option<BackendType>,
    /// Per-dimension weights. Omitted keys fall back to their default values.
    #[serde(default)]
    pub weights: ScoredWeights,
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

/// Network locality tier for a backend, relative to the gateway. Feeds the
/// scored router's `network_locality` dimension (dim 19): closer backends are
/// preferred so the proxy hop adds the least latency. `None` on a `Backend` →
/// that dimension is absent for it (neutral, weight-dropped).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LocalityTier {
    /// Same host as the gateway (loopback / unix socket).
    #[serde(rename = "local")]
    Local,
    /// Same LAN segment.
    #[serde(rename = "lan")]
    Lan,
    /// Reachable over a Tailscale / WireGuard tailnet.
    #[serde(rename = "tailnet")]
    Tailnet,
    /// Public internet / WAN.
    #[serde(rename = "wan")]
    Wan,
}

impl std::fmt::Display for LocalityTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LocalityTier::Local => write!(f, "local"),
            LocalityTier::Lan => write!(f, "lan"),
            LocalityTier::Tailnet => write!(f, "tailnet"),
            LocalityTier::Wan => write!(f, "wan"),
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

    /// Context-window size (tokens) this backend serves. Feeds the scored
    /// router's `prompt_size_vs_capacity` dimension; `None` (the default) →
    /// that dimension is absent for this backend (neutral, weight-dropped).
    #[serde(default)]
    pub max_context_len: Option<u32>,

    /// Network locality tier relative to the gateway. Feeds the scored router's
    /// `network_locality` dimension (dim 19); `None` (the default) → that
    /// dimension is absent for this backend (neutral, weight-dropped).
    #[serde(default)]
    pub locality: Option<LocalityTier>,

    /// Relative cost/power weight for this backend (e.g. watts, or $/1k tokens);
    /// lower is cheaper. Feeds the scored router's `power_cost` dimension
    /// (dim 20); `None` (the default) → that dimension is absent for this
    /// backend (neutral, weight-dropped).
    #[serde(default)]
    pub power_cost: Option<f64>,
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
            max_context_len: None,
            locality: None,
            power_cost: None,
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

/// Fleet version authority (v1.2 PR #6): the gateway declares the version
/// agents should run and serves published agent binaries. Advertising a target
/// alone never triggers an update — a binary must also be published under
/// `publish_dir` for the target version and the agent's platform, so promoting
/// a build is always a deliberate act, not a side effect of restarting `serve`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FleetConfig {
    /// Version agents should self-update to. Defaults to the gateway's own
    /// version; set this (or HERD_TARGET_AGENT_VERSION, which wins over
    /// config) so a mid-debug gateway build never becomes the fleet target.
    #[serde(default)]
    pub target_agent_version: Option<String>,

    /// Directory of published agent binaries, laid out as
    /// `{publish_dir}/{version}/{os}-{arch}/herd[.exe]`.
    /// Env override: HERD_AGENT_PUBLISH_DIR. Default: `~/.herd/binaries`.
    #[serde(default)]
    pub publish_dir: Option<String>,

    /// Base URL agents download binaries from. Unset (default) means the
    /// gateway serves them itself and sends no download_url at all — agents
    /// construct the URL from their own --gateway address (presence of
    /// download_url in a heartbeat reply ⇔ external override). Point this at
    /// an external host (e.g. GitHub release assets) to hand off downloads
    /// with no agent-side change — agents treat the URL as opaque.
    #[serde(default)]
    pub download_url_base: Option<String>,

    /// How agents restart themselves after applying a self-update. Agents
    /// read this via the HERD_RESPAWN_MODE env var or `herd agent
    /// --respawn-mode` (they don't load herd.yaml); this field documents the
    /// fleet-wide intent and is reserved for heartbeat relay in a later PR.
    #[serde(default)]
    pub respawn_mode: RespawnMode,
}

/// How `herd agent` brings the new binary up after a self-update swap.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RespawnMode {
    /// Spawn the (now-replaced) executable with the same argv and env, then
    /// exit. For `herd agent` run bare in a terminal.
    #[default]
    #[serde(rename = "self")]
    SelfSpawn,
    /// Exit(0) only — the supervisor (NSSM, systemd Restart=always, ...)
    /// brings up the new binary. Self-spawning here would double-run the
    /// agent under the service manager.
    Supervised,
}

impl std::str::FromStr for RespawnMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim() {
            "self" => Ok(Self::SelfSpawn),
            "supervised" => Ok(Self::Supervised),
            other => Err(format!(
                "unknown respawn mode '{other}' (expected 'self' or 'supervised')"
            )),
        }
    }
}

impl FleetConfig {
    /// Effective target agent version: env (HERD_TARGET_AGENT_VERSION) wins
    /// over config, which wins over the gateway's own build version.
    pub fn resolved_target_version(&self) -> String {
        Self::target_version_from(
            std::env::var("HERD_TARGET_AGENT_VERSION").ok().as_deref(),
            self.target_agent_version.as_deref(),
        )
    }

    /// Env-injectable core of [`resolved_target_version`] so tests don't race
    /// on process-global env vars.
    pub fn target_version_from(env: Option<&str>, config: Option<&str>) -> String {
        env.map(str::trim)
            .filter(|s| !s.is_empty())
            .or_else(|| config.map(str::trim).filter(|s| !s.is_empty()))
            .unwrap_or(env!("CARGO_PKG_VERSION"))
            .to_string()
    }

    /// Effective publish directory: env (HERD_AGENT_PUBLISH_DIR) wins over
    /// config, which wins over `{data_root}/binaries`.
    pub fn resolved_publish_dir(&self, data_root: &std::path::Path) -> std::path::PathBuf {
        Self::publish_dir_from(
            std::env::var("HERD_AGENT_PUBLISH_DIR").ok().as_deref(),
            self.publish_dir.as_deref(),
            data_root,
        )
    }

    /// Env-injectable core of [`resolved_publish_dir`]. The default branch
    /// roots under `data_root` rather than hard-coding `~/.herd` so a
    /// containerised gateway that sets `HERD_DATA_DIR=/var/lib/herd` gets
    /// `{data_root}/binaries` as its publish dir automatically. env >
    /// config > default order is unchanged.
    pub fn publish_dir_from(
        env: Option<&str>,
        config: Option<&str>,
        data_root: &std::path::Path,
    ) -> std::path::PathBuf {
        if let Some(dir) = env
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or_else(|| config.map(str::trim).filter(|s| !s.is_empty()))
        {
            return std::path::PathBuf::from(dir);
        }
        data_root.join("binaries")
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

/// Frontier Gateway global settings.
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
            warn_threshold: default_warn_threshold(),
            block_threshold: default_block_threshold(),
        }
    }
}

fn default_warn_threshold() -> f32 {
    0.80
}
fn default_block_threshold() -> f32 {
    1.00
}

/// Per-model pricing override (USD per million tokens).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PricingOverride {
    pub input_per_mtok: f32,
    pub output_per_mtok: f32,
}

/// A single frontier/external provider (e.g. OpenAI, Anthropic).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub name: String,

    #[serde(default = "default_frontier_type")]
    pub r#type: String,

    pub api_url: String,

    #[serde(default)]
    pub api_key_env: String,

    #[serde(default)]
    pub models: Vec<String>,

    #[serde(default)]
    pub rate_limit: u64,

    #[serde(default)]
    pub monthly_budget: f32,

    #[serde(default = "default_provider_priority")]
    pub priority: u32,

    #[serde(default)]
    pub pricing: HashMap<String, PricingOverride>,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            r#type: default_frontier_type(),
            api_url: String::new(),
            api_key_env: String::new(),
            models: Vec::new(),
            rate_limit: 0,
            monthly_budget: 0.0,
            priority: default_provider_priority(),
            pricing: HashMap::new(),
        }
    }
}

fn default_frontier_type() -> String {
    "frontier".to_string()
}
fn default_provider_priority() -> u32 {
    50
}

impl Config {
    /// Env-injectable core for the data-root resolver. Mirrors the idiom used
    /// by `FleetConfig::publish_dir_from`: trim, filter non-empty, env over
    /// config over fallback. Fallback is `~/.herd` (byte-identical to the
    /// pre-v1.2 default so existing deployments see no change when neither
    /// env nor config field is set).
    pub fn data_dir_from(env: Option<&str>, config: Option<&str>) -> std::path::PathBuf {
        if let Some(dir) = env
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or_else(|| config.map(str::trim).filter(|s| !s.is_empty()))
        {
            return std::path::PathBuf::from(dir);
        }
        dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(".herd")
    }

    /// Effective data root: `HERD_DATA_DIR` env wins over the `data_dir`
    /// config field, which wins over `~/.herd`.
    pub fn resolved_data_dir(&self) -> std::path::PathBuf {
        Self::data_dir_from(
            std::env::var("HERD_DATA_DIR").ok().as_deref(),
            self.data_dir.as_deref(),
        )
    }

    pub fn from_file(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path)?;

        // First pass: check for deprecated keys in raw YAML
        if let Ok(raw) = serde_yaml::from_str::<serde_yaml::Value>(&content) {
            Self::warn_deprecated_keys(&raw, &[]);

            // Warn on unrecognised keys under routing.scored.weights (spec acceptance #9).
            // Navigate: raw["routing"]["scored"]["weights"] → Mapping → diff against known names.
            // Every step is guarded: missing key or wrong type → skip silently (never bail).
            if let Some(routing_val) = raw.get("routing") {
                if let Some(scored_val) = routing_val.get("scored") {
                    if let Some(serde_yaml::Value::Mapping(weights_map)) = scored_val.get("weights")
                    {
                        for k in unknown_weight_keys(weights_map) {
                            tracing::warn!("unknown scored weight key '{}' — ignored", k);
                        }
                    }
                }
            }
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

    pub fn validate(&mut self) -> Result<()> {
        // Validate fleet.target_agent_version: it is used as a path component
        // under publish_dir, so restrict to a version-shaped charset. Bad
        // values warn and fall back to the gateway's own version (never bail).
        if let Some(v) = &self.fleet.target_agent_version {
            let v = v.trim();
            let version_shaped = !v.is_empty()
                && v.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+'));
            if !version_shaped {
                tracing::warn!(
                    "fleet.target_agent_version '{}' is not a valid version string - ignoring (gateway's own version will be advertised)",
                    v
                );
                self.fleet.target_agent_version = None;
            }
        }

        // Validate model_warmer interval. A value of 0 disables the warmer.
        if self.model_warmer.interval_secs > 0 && self.model_warmer.interval_secs < 10 {
            tracing::warn!(
                "model_warmer.interval_secs must be >= 10 or 0 to disable (got {}) - disabling model warmer",
                self.model_warmer.interval_secs
            );
            self.model_warmer.interval_secs = 0;
        }

        // Validate backend URLs, reserved-prefix names, and name uniqueness.
        // Order per spec (impl-delta #8): URL-validity → reserved-prefix → duplicate.
        // Bad entries are skipped (warn + continue) — never bail!
        let mut valid_backends = Vec::with_capacity(self.backends.len());
        let mut seen_names: BTreeSet<String> = BTreeSet::new();
        for (i, backend) in self.backends.drain(..).enumerate() {
            let url = backend.url.trim();
            let parsed = reqwest::Url::parse(url);
            let valid = parsed
                .as_ref()
                .ok()
                .filter(|u| matches!(u.scheme(), "http" | "https") && u.host_str().is_some())
                .is_some();

            if url.is_empty() {
                tracing::warn!("Skipping backends[{}] ('{}'): empty URL", i, backend.name);
                continue;
            }
            if !valid {
                tracing::warn!(
                    "Skipping backends[{}] ('{}'): invalid URL '{}'. Backend URLs must be absolute http(s) URLs with a host",
                    i,
                    backend.name,
                    backend.url
                );
                continue;
            }

            // Reserved-prefix check (impl-delta #8 — B-1 determinism precondition).
            // The `agent:` and `node:` namespaces are owned by the fleet reconcilers;
            // a static entry using those prefixes would collide with reconciler-owned
            // keys, breaking the SELECT tie-break's name-uniqueness invariant.
            if backend.name.starts_with("agent:") || backend.name.starts_with("node:") {
                tracing::warn!(
                    "backend name '{}' uses a reserved prefix ('agent:' / 'node:') — \
                     these namespaces are owned by the fleet reconcilers; skipping",
                    backend.name
                );
                continue;
            }

            // Duplicate-name check (warn + keep first).
            if seen_names.contains(&backend.name) {
                tracing::warn!(
                    "duplicate backend name '{}' — keeping first, dropping duplicate",
                    backend.name
                );
                continue;
            }
            seen_names.insert(backend.name.clone());

            valid_backends.push(backend);
        }
        self.backends = valid_backends;

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

        // Sanitize scored router weights: negative/non-finite → per-key default;
        // all Phase-1-active dims zero → warn + restore full defaults.
        // Never bail — house rule: degrade gracefully.
        crate::router::scored::sanitize_weights(&mut self.routing.scored.weights);

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
    fn fleet_section_defaults_to_unset() {
        let config: Config = serde_yaml::from_str("{}").unwrap();
        assert!(config.fleet.target_agent_version.is_none());
        assert!(config.fleet.publish_dir.is_none());
        assert!(config.fleet.download_url_base.is_none());
    }

    #[test]
    fn fleet_target_version_resolution_order() {
        use super::FleetConfig;
        // env wins over config wins over own version
        assert_eq!(
            FleetConfig::target_version_from(Some("1.3.0"), Some("1.2.5")),
            "1.3.0"
        );
        assert_eq!(
            FleetConfig::target_version_from(None, Some("1.2.5")),
            "1.2.5"
        );
        assert_eq!(
            FleetConfig::target_version_from(None, None),
            env!("CARGO_PKG_VERSION")
        );
        // empty/whitespace values are treated as unset
        assert_eq!(
            FleetConfig::target_version_from(Some("  "), Some("")),
            env!("CARGO_PKG_VERSION")
        );
    }

    #[test]
    fn fleet_publish_dir_resolution_order() {
        use super::FleetConfig;
        let data_dir = super::Config::data_dir_from(None, None);
        assert_eq!(
            FleetConfig::publish_dir_from(Some("/srv/bin"), Some("/cfg/bin"), &data_dir),
            std::path::PathBuf::from("/srv/bin")
        );
        assert_eq!(
            FleetConfig::publish_dir_from(None, Some("/cfg/bin"), &data_dir),
            std::path::PathBuf::from("/cfg/bin")
        );
        let default = FleetConfig::publish_dir_from(None, None, &data_dir);
        assert!(default.ends_with(std::path::Path::new(".herd/binaries")));
    }

    #[test]
    fn data_dir_from_defaults_to_herd() {
        let d = super::Config::data_dir_from(None, None);
        assert!(
            d.ends_with(std::path::Path::new(".herd")),
            "default must end with .herd, got {}",
            d.display()
        );
    }

    #[test]
    fn data_dir_from_env_wins() {
        let d = super::Config::data_dir_from(Some("/var/lib/herd"), None);
        assert_eq!(d, std::path::PathBuf::from("/var/lib/herd"));
    }

    #[test]
    fn data_dir_from_config_used_when_env_absent() {
        let d = super::Config::data_dir_from(None, Some("/cfg/data"));
        assert_eq!(d, std::path::PathBuf::from("/cfg/data"));
    }

    #[test]
    fn data_dir_from_env_beats_config() {
        let d = super::Config::data_dir_from(Some("/env"), Some("/cfg"));
        assert_eq!(d, std::path::PathBuf::from("/env"));
    }

    #[test]
    fn publish_dir_from_env_still_wins_over_data_root() {
        let data_dir = super::Config::data_dir_from(Some("/var/lib/herd"), None);
        let p = super::FleetConfig::publish_dir_from(Some("/srv/bin"), None, &data_dir);
        assert_eq!(p, std::path::PathBuf::from("/srv/bin"));
    }

    #[test]
    fn publish_dir_from_default_re_roots_under_data_dir() {
        let data_dir = std::path::PathBuf::from("/var/lib/herd");
        let p = super::FleetConfig::publish_dir_from(None, None, &data_dir);
        assert_eq!(p, std::path::PathBuf::from("/var/lib/herd/binaries"));
    }

    #[test]
    fn fleet_invalid_target_version_warns_and_clears() {
        let yaml = "fleet:\n  target_agent_version: \"../../etc\"\n";
        let mut config: Config = serde_yaml::from_str(yaml).unwrap();
        config.validate().unwrap();
        assert!(config.fleet.target_agent_version.is_none());
    }

    #[test]
    fn respawn_mode_defaults_to_self_and_parses_both_values() {
        let config = Config::default();
        assert_eq!(config.fleet.respawn_mode, super::RespawnMode::SelfSpawn);

        assert_eq!(
            "self".parse::<super::RespawnMode>().unwrap(),
            super::RespawnMode::SelfSpawn
        );
        assert_eq!(
            "supervised".parse::<super::RespawnMode>().unwrap(),
            super::RespawnMode::Supervised
        );
        assert!("nssm".parse::<super::RespawnMode>().is_err());
    }

    #[test]
    fn respawn_mode_deserializes_from_fleet_yaml() {
        let yaml = "fleet:\n  respawn_mode: supervised\n";
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.fleet.respawn_mode, super::RespawnMode::Supervised);
    }

    #[test]
    fn fleet_valid_target_version_survives_validate() {
        let yaml = "fleet:\n  target_agent_version: \"1.2.0-rc.1\"\n";
        let mut config: Config = serde_yaml::from_str(yaml).unwrap();
        config.validate().unwrap();
        assert_eq!(
            config.fleet.target_agent_version.as_deref(),
            Some("1.2.0-rc.1")
        );
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

    #[test]
    fn frontier_config_defaults() {
        let f = super::FrontierConfig::default();
        assert!(!f.enabled);
        assert!(!f.allow_auto_escalation);
        assert!(f.require_header);
        assert!(f.log_all_requests);
        assert!((f.warn_threshold - 0.80).abs() < 0.001);
        assert!((f.block_threshold - 1.00).abs() < 0.001);
    }

    #[test]
    fn provider_config_deserializes() {
        let yaml = r#"
providers:
  - name: openai
    type: frontier
    api_url: https://api.openai.com
    api_key_env: OPENAI_API_KEY
    models:
      - gpt-4o
      - gpt-4o-mini
    rate_limit: 60
    monthly_budget: 100.0
    priority: 10
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.providers.len(), 1);
        let p = &config.providers[0];
        assert_eq!(p.name, "openai");
        assert_eq!(p.r#type, "frontier");
        assert_eq!(p.api_url, "https://api.openai.com");
        assert_eq!(p.api_key_env, "OPENAI_API_KEY");
        assert_eq!(p.models, vec!["gpt-4o", "gpt-4o-mini"]);
        assert_eq!(p.rate_limit, 60);
        assert!((p.monthly_budget - 100.0).abs() < 0.001);
        assert_eq!(p.priority, 10);
        assert!(p.pricing.is_empty());
    }

    #[test]
    fn provider_config_with_pricing_overrides() {
        let yaml = r#"
providers:
  - name: anthropic
    api_url: https://api.anthropic.com
    pricing:
      claude-3-5-sonnet-20241022:
        input_per_mtok: 3.0
        output_per_mtok: 15.0
      claude-3-haiku-20240307:
        input_per_mtok: 0.25
        output_per_mtok: 1.25
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let p = &config.providers[0];
        assert_eq!(p.pricing.len(), 2);
        let sonnet = &p.pricing["claude-3-5-sonnet-20241022"];
        assert!((sonnet.input_per_mtok - 3.0).abs() < 0.001);
        assert!((sonnet.output_per_mtok - 15.0).abs() < 0.001);
        let haiku = &p.pricing["claude-3-haiku-20240307"];
        assert!((haiku.input_per_mtok - 0.25).abs() < 0.001);
        assert!((haiku.output_per_mtok - 1.25).abs() < 0.001);
    }

    #[test]
    fn config_without_frontier_section_backward_compat() {
        let yaml = r#"
server:
  host: 0.0.0.0
  port: 40114
backends:
  - name: gpu1
    url: http://localhost:11434
    priority: 100
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(!config.frontier.enabled);
        assert!(config.providers.is_empty());
    }

    // ── Acceptance test #11: name-uniqueness enforcement (impl-delta #8) ─────

    #[test]
    fn validate_drops_duplicate_backend_name_keeps_first() {
        let yaml = r#"
backends:
  - name: gpu1
    url: http://host1:11434
    priority: 100
  - name: gpu1
    url: http://host2:11434
    priority: 50
  - name: gpu2
    url: http://host3:11434
    priority: 75
"#;
        let mut config: Config = serde_yaml::from_str(yaml).unwrap();
        config.validate().unwrap();
        assert_eq!(config.backends.len(), 2, "duplicate 'gpu1' must be dropped");
        assert_eq!(config.backends[0].name, "gpu1");
        assert_eq!(config.backends[0].url, "http://host1:11434"); // first kept
        assert_eq!(config.backends[1].name, "gpu2");
    }

    #[test]
    fn validate_drops_reserved_prefix_agent() {
        let yaml = r#"
backends:
  - name: agent:citadel
    url: http://host1:11434
    priority: 50
  - name: good-backend
    url: http://host2:11434
    priority: 50
"#;
        let mut config: Config = serde_yaml::from_str(yaml).unwrap();
        config.validate().unwrap();
        assert_eq!(config.backends.len(), 1);
        assert_eq!(config.backends[0].name, "good-backend");
    }

    #[test]
    fn validate_drops_reserved_prefix_node() {
        let yaml = r#"
backends:
  - name: node:my-machine
    url: http://host1:11434
    priority: 50
  - name: valid-name
    url: http://host2:11434
    priority: 50
"#;
        let mut config: Config = serde_yaml::from_str(yaml).unwrap();
        config.validate().unwrap();
        assert_eq!(config.backends.len(), 1);
        assert_eq!(config.backends[0].name, "valid-name");
    }

    // ── Acceptance test #9: config backward-compat + unknown-key warn ────────

    #[test]
    fn scored_config_defaults_when_block_absent() {
        // Existing herd.yaml with no 'scored' block must deserialize cleanly
        // and apply all defaults — no panic, no bail.
        let yaml = r#"
routing:
  strategy: scored
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        use super::RoutingStrategy;
        assert_eq!(config.routing.strategy, RoutingStrategy::Scored);
        // Defaults table.
        assert_eq!(config.routing.scored.weights.model_resident, 5.0);
        assert_eq!(config.routing.scored.weights.gpu_utilization, 3.0);
        // Phase 2: live-load dims carry latency-aware balanced weights.
        assert_eq!(config.routing.scored.weights.queue_depth, 2.0);
        assert_eq!(config.routing.scored.weights.ttft_p50, 3.0);
        assert_eq!(config.routing.scored.weights.precise_vram_free, 2.0);
        // Dim 12 (Slice 2): lower weight than dim 10, coexisting.
        assert_eq!(config.routing.scored.weights.concurrency_saturation, 1.0);
    }

    #[test]
    fn scored_config_partial_weights_override_only_named_keys() {
        let yaml = r#"
routing:
  strategy: scored
  scored:
    weights:
      gpu_utilization: 10.0
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.routing.scored.weights.gpu_utilization, 10.0);
        // Other keys remain at their defaults.
        assert_eq!(config.routing.scored.weights.model_resident, 5.0);
        assert_eq!(config.routing.scored.weights.vram_headroom, 2.0);
    }

    #[test]
    fn unknown_weight_keys_returns_extras() {
        use super::{unknown_weight_keys, SCORED_WEIGHT_FIELD_NAMES};
        let yaml = "gpu_utilization: 3.0\ntypo_key: 1.0\nanother_bogus: 5.0\n";
        let mapping: serde_yaml::Mapping = serde_yaml::from_str(yaml).unwrap();
        let unknowns = unknown_weight_keys(&mapping);
        assert!(
            unknowns.contains(&"typo_key".to_string()),
            "typo_key must be flagged"
        );
        assert!(
            unknowns.contains(&"another_bogus".to_string()),
            "another_bogus must be flagged"
        );
        // Known key must NOT appear in unknowns.
        assert!(
            !unknowns.contains(&"gpu_utilization".to_string()),
            "gpu_utilization is a known key"
        );
        // All 23 known keys must not appear.
        for &known in SCORED_WEIGHT_FIELD_NAMES {
            assert!(!unknowns.contains(&known.to_string()));
        }
    }

    #[test]
    fn scored_config_backward_compat_no_scored_block() {
        // A herd.yaml with no 'scored' section at all must parse cleanly with defaults.
        let yaml = r#"
server:
  host: 0.0.0.0
  port: 40114
routing:
  strategy: model_aware
backends:
  - name: gpu1
    url: http://localhost:11434
    priority: 100
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        // scored block not present → all defaults.
        assert_eq!(config.routing.scored.weights.model_resident, 5.0);
        assert_eq!(config.routing.scored.weights.gpu_utilization, 3.0);
        assert_eq!(config.routing.scored.weights.queue_depth, 2.0);
        // model_gate defaults to Relaxed.
        use super::ModelGate;
        assert_eq!(config.routing.scored.model_gate, ModelGate::Relaxed);
    }

    // ── Acceptance #9 wired-path tests (call validate() / mutate config directly) ──
    // These assert on the WIRED path so un-wiring sanitize_weights breaks them.

    #[test]
    fn validate_wired_all_zero_phase1_weights_restores_defaults() {
        // All Phase-1-active dims set to 0.0 in config → after validate(), defaults restored.
        let yaml = r#"
routing:
  strategy: scored
  scored:
    weights:
      model_resident: 0.0
      model_fits_vram: 0.0
      prompt_size_vs_capacity: 0.0
      gpu_utilization: 0.0
      vram_headroom: 0.0
      gpu_temperature: 0.0
      operator_priority: 0.0
      tag_affinity: 0.0
      backend_type_affinity: 0.0
"#;
        let mut config: Config = serde_yaml::from_str(yaml).unwrap();
        // Before validate: all zeros as set.
        assert_eq!(config.routing.scored.weights.gpu_utilization, 0.0);
        config.validate().unwrap();
        // After validate: sanitize_weights ran via the wired path → defaults restored.
        assert_eq!(config.routing.scored.weights.gpu_utilization, 3.0);
        assert_eq!(config.routing.scored.weights.model_resident, 5.0);
        assert_eq!(config.routing.scored.weights.vram_headroom, 2.0);
    }

    #[test]
    fn validate_wired_negative_weight_reset_to_default() {
        // A negative weight in the config → after validate(), reset to per-key default.
        let yaml = r#"
routing:
  strategy: scored
  scored:
    weights:
      gpu_utilization: -5.0
"#;
        let mut config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.routing.scored.weights.gpu_utilization, -5.0);
        config.validate().unwrap();
        assert_eq!(config.routing.scored.weights.gpu_utilization, 3.0);
        // Other keys unchanged (still at their defaults).
        assert_eq!(config.routing.scored.weights.model_resident, 5.0);
    }

    #[test]
    fn unknown_weight_keys_wired_via_parsed_mapping() {
        // Exercise unknown_weight_keys via the same raw-YAML path from_file uses:
        // parse the weights block as a serde_yaml::Mapping and call the pure fn directly.
        // This is the exact navigation from_file does; a future un-wiring would break
        // the from_file path while leaving this test green — kept separate from from_file
        // because from_file requires a real filesystem path (temp file).
        let weights_yaml = "gpu_utilization: 3.0\nbogus_key: 1.0\n";
        let raw: serde_yaml::Value = serde_yaml::from_str(weights_yaml).unwrap();
        let mapping = match &raw {
            serde_yaml::Value::Mapping(m) => m,
            _ => panic!("expected mapping"),
        };
        let unknowns = super::unknown_weight_keys(mapping);
        assert!(
            unknowns.contains(&"bogus_key".to_string()),
            "bogus_key must be flagged as unknown"
        );
        assert!(
            !unknowns.contains(&"gpu_utilization".to_string()),
            "gpu_utilization must not be flagged"
        );
    }
}
