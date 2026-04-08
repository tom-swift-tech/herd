use crate::config::{Config, RoutingStrategy};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RoutingProfilesConfig {
    #[serde(default)]
    pub enabled: bool,

    /// Default profile name (used when no X-Herd-Profile header). Falls back to global routing config.
    #[serde(default)]
    pub default_profile: Option<String>,

    /// Named profiles
    #[serde(default)]
    pub profiles: HashMap<String, RoutingProfile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingProfile {
    /// Routing strategy for this profile
    pub strategy: RoutingStrategy,

    /// Only route to backends with these tags (empty = all)
    #[serde(default)]
    pub tags: Vec<String>,

    /// Only route to these backend names (empty = all)
    #[serde(default)]
    pub backends: Vec<String>,

    /// Override model preference
    #[serde(default)]
    pub preferred_model: Option<String>,

    /// Description for dashboard display
    #[serde(default)]
    pub description: Option<String>,
}

/// The result of resolving which routing config to use for a request.
#[derive(Debug, Clone)]
pub struct ResolvedRouting {
    pub strategy: RoutingStrategy,
    pub tags: Vec<String>,
    pub backends: Vec<String>,
    pub preferred_model: Option<String>,
    /// Name of the profile that was selected, if any.
    pub profile_name: Option<String>,
}

/// Resolve routing configuration from config and an optional profile header.
///
/// Resolution order:
/// 1. If profiles disabled → global routing config
/// 2. If header names a valid profile → use that profile
/// 3. If header names an invalid profile → fall back to default profile or global config
/// 4. If no header → use default profile (if set) or global config
pub fn resolve_profile(config: &Config, profile_header: Option<&str>) -> ResolvedRouting {
    let profiles_config = &config.routing_profiles;

    if !profiles_config.enabled {
        return global_fallback(config);
    }

    // If a header was provided, try to look up that profile
    if let Some(name) = profile_header {
        if let Some(profile) = profiles_config.profiles.get(name) {
            return from_profile(name, profile);
        }
        // Invalid profile name — fall through to default
        tracing::warn!(
            "Requested routing profile '{}' not found, falling back to default",
            name
        );
    }

    // No header or invalid header — try default profile
    if let Some(ref default_name) = profiles_config.default_profile {
        if let Some(profile) = profiles_config.profiles.get(default_name) {
            return from_profile(default_name, profile);
        }
        tracing::warn!(
            "Default routing profile '{}' not found, using global config",
            default_name
        );
    }

    global_fallback(config)
}

fn from_profile(name: &str, profile: &RoutingProfile) -> ResolvedRouting {
    ResolvedRouting {
        strategy: profile.strategy.clone(),
        tags: profile.tags.clone(),
        backends: profile.backends.clone(),
        preferred_model: profile.preferred_model.clone(),
        profile_name: Some(name.to_string()),
    }
}

fn global_fallback(config: &Config) -> ResolvedRouting {
    ResolvedRouting {
        strategy: config.routing.strategy.clone(),
        tags: Vec::new(),
        backends: Vec::new(),
        preferred_model: None,
        profile_name: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, RoutingStrategy};

    fn test_config_with_profiles() -> Config {
        let yaml = r#"
routing:
  strategy: model_aware

routing_profiles:
  enabled: true
  default_profile: balanced
  profiles:
    fast:
      strategy: priority
      description: "Fastest response"
    balanced:
      strategy: least_busy
      description: "Balanced load"
    gpu-only:
      strategy: model_aware
      tags:
        - gpu
      description: "GPU backends only"
    local:
      strategy: priority
      backends:
        - local-ollama
      description: "Local only"
    coding:
      strategy: model_aware
      preferred_model: "qwen2.5-coder:32b"
      tags:
        - gpu
        - high-vram
      description: "Coding profile"
"#;
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn resolve_no_header_uses_default_profile() {
        let config = test_config_with_profiles();
        let resolved = resolve_profile(&config, None);
        assert_eq!(resolved.strategy, RoutingStrategy::LeastBusy);
        assert_eq!(resolved.profile_name.as_deref(), Some("balanced"));
        assert!(resolved.tags.is_empty());
    }

    #[test]
    fn resolve_valid_header_uses_named_profile() {
        let config = test_config_with_profiles();
        let resolved = resolve_profile(&config, Some("fast"));
        assert_eq!(resolved.strategy, RoutingStrategy::Priority);
        assert_eq!(resolved.profile_name.as_deref(), Some("fast"));
    }

    #[test]
    fn resolve_invalid_header_falls_back_to_default() {
        let config = test_config_with_profiles();
        let resolved = resolve_profile(&config, Some("nonexistent"));
        assert_eq!(resolved.strategy, RoutingStrategy::LeastBusy);
        assert_eq!(resolved.profile_name.as_deref(), Some("balanced"));
    }

    #[test]
    fn resolve_profiles_disabled_uses_global() {
        let yaml = r#"
routing:
  strategy: priority

routing_profiles:
  enabled: false
  default_profile: balanced
  profiles:
    balanced:
      strategy: least_busy
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let resolved = resolve_profile(&config, Some("balanced"));
        assert_eq!(resolved.strategy, RoutingStrategy::Priority);
        assert!(resolved.profile_name.is_none());
    }

    #[test]
    fn resolve_profile_with_tags() {
        let config = test_config_with_profiles();
        let resolved = resolve_profile(&config, Some("gpu-only"));
        assert_eq!(resolved.strategy, RoutingStrategy::ModelAware);
        assert_eq!(resolved.tags, vec!["gpu".to_string()]);
        assert_eq!(resolved.profile_name.as_deref(), Some("gpu-only"));
    }

    #[test]
    fn resolve_profile_with_backends_filter() {
        let config = test_config_with_profiles();
        let resolved = resolve_profile(&config, Some("local"));
        assert_eq!(resolved.strategy, RoutingStrategy::Priority);
        assert_eq!(resolved.backends, vec!["local-ollama".to_string()]);
    }

    #[test]
    fn resolve_profile_with_preferred_model() {
        let config = test_config_with_profiles();
        let resolved = resolve_profile(&config, Some("coding"));
        assert_eq!(
            resolved.preferred_model.as_deref(),
            Some("qwen2.5-coder:32b")
        );
        assert_eq!(resolved.tags, vec!["gpu".to_string(), "high-vram".to_string()]);
    }

    #[test]
    fn resolve_no_default_profile_uses_global() {
        let yaml = r#"
routing:
  strategy: weighted_round_robin

routing_profiles:
  enabled: true
  profiles:
    fast:
      strategy: priority
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let resolved = resolve_profile(&config, None);
        assert_eq!(resolved.strategy, RoutingStrategy::WeightedRoundRobin);
        assert!(resolved.profile_name.is_none());
    }

    #[test]
    fn resolve_missing_default_profile_uses_global() {
        let yaml = r#"
routing:
  strategy: weighted_round_robin

routing_profiles:
  enabled: true
  default_profile: deleted_profile
  profiles:
    fast:
      strategy: priority
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let resolved = resolve_profile(&config, None);
        assert_eq!(resolved.strategy, RoutingStrategy::WeightedRoundRobin);
        assert!(resolved.profile_name.is_none());
    }
}
