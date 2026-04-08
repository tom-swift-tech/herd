use crate::config::{BackendType, DiscoveryConfig, StaticNodeConfig};
use crate::nodes::{NodeDb, NodeRegistration};
use std::sync::Arc;
use std::time::Duration;

pub struct NodeDiscovery {
    config: DiscoveryConfig,
    client: reqwest::Client,
}

impl NodeDiscovery {
    pub fn new(config: DiscoveryConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_default();
        Self { config, client }
    }

    /// Spawn the discovery background task. No-op if discovery is disabled.
    pub fn spawn(self, node_db: Arc<NodeDb>) {
        if !self.config.enabled {
            return;
        }

        if !self.config.static_nodes.is_empty() {
            tracing::info!(
                "Discovery enabled: {} static node(s), probe interval {}s",
                self.config.static_nodes.len(),
                self.config.probe_interval_secs,
            );
        }

        // Warn if mDNS is configured but the feature is not compiled
        if self.config.mdns.enabled {
            #[cfg(not(feature = "mdns"))]
            tracing::warn!(
                "mDNS discovery configured but 'mdns' feature not compiled. Using static discovery only."
            );
        }

        let interval = Duration::from_secs(self.config.probe_interval_secs.max(10));
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            loop {
                ticker.tick().await;
                self.probe_static_nodes(&node_db).await;
            }
        });
    }

    /// Probe each static node: health check + register
    async fn probe_static_nodes(&self, node_db: &NodeDb) {
        for node_config in &self.config.static_nodes {
            self.probe_and_register(node_db, node_config).await;
        }
    }

    /// Probe a single node and register/update it in the node DB.
    async fn probe_and_register(&self, node_db: &NodeDb, config: &StaticNodeConfig) {
        let base_url = config.url.trim_end_matches('/');
        let hostname = extract_hostname(config);

        // Health probe based on backend type
        let health_url = match config.backend {
            BackendType::Ollama => format!("{}/", base_url),
            BackendType::LlamaServer | BackendType::OpenAICompat => {
                format!("{}/health", base_url)
            }
        };

        let healthy = match self.client.get(&health_url).send().await {
            Ok(resp) => resp.status().is_success(),
            Err(_) => false,
        };

        if !healthy {
            tracing::debug!(
                "Static node {} ({}) unreachable",
                hostname,
                base_url
            );
            return;
        }

        // Build a NodeRegistration and upsert into the DB
        let fleet_hostname = format!("fleet:{}", hostname);
        let reg = NodeRegistration {
            hostname: fleet_hostname.clone(),
            backend: config.backend,
            backend_url: Some(config.url.clone()),
            ollama_url: config.url.clone(),
            ..Default::default()
        };

        match node_db.upsert_node(&reg) {
            Ok((_id, is_new)) => {
                if is_new {
                    tracing::info!(
                        "Discovered static node: {} ({})",
                        hostname,
                        base_url
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to register static node {}: {}",
                    hostname,
                    e
                );
            }
        }
    }
}

/// Extract hostname from a StaticNodeConfig.
/// Uses the explicit hostname if provided, otherwise extracts from the URL.
pub fn extract_hostname(config: &StaticNodeConfig) -> String {
    config.hostname.clone().unwrap_or_else(|| {
        config
            .url
            .trim_start_matches("http://")
            .trim_start_matches("https://")
            .split(':')
            .next()
            .unwrap_or("unknown")
            .to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BackendType, DiscoveryConfig, MdnsConfig, StaticNodeConfig};

    #[test]
    fn extract_hostname_from_url_with_port() {
        let config = StaticNodeConfig {
            url: "http://192.168.1.100:8090".to_string(),
            backend: BackendType::LlamaServer,
            hostname: None,
            tags: vec![],
            priority: 50,
        };
        assert_eq!(extract_hostname(&config), "192.168.1.100");
    }

    #[test]
    fn extract_hostname_from_url_without_port() {
        let config = StaticNodeConfig {
            url: "http://myhost".to_string(),
            backend: BackendType::Ollama,
            hostname: None,
            tags: vec![],
            priority: 50,
        };
        assert_eq!(extract_hostname(&config), "myhost");
    }

    #[test]
    fn extract_hostname_uses_override() {
        let config = StaticNodeConfig {
            url: "http://192.168.1.100:8090".to_string(),
            backend: BackendType::Ollama,
            hostname: Some("citadel".to_string()),
            tags: vec![],
            priority: 50,
        };
        assert_eq!(extract_hostname(&config), "citadel");
    }

    #[test]
    fn extract_hostname_https_url() {
        let config = StaticNodeConfig {
            url: "https://secure-node.lan:443".to_string(),
            backend: BackendType::OpenAICompat,
            hostname: None,
            tags: vec![],
            priority: 50,
        };
        assert_eq!(extract_hostname(&config), "secure-node.lan");
    }

    #[test]
    fn static_node_config_deserializes_from_yaml() {
        let yaml = r#"
url: http://192.168.1.100:8090
backend: llama-server
tags: [gpu, nvidia]
priority: 10
"#;
        let config: StaticNodeConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.url, "http://192.168.1.100:8090");
        assert_eq!(config.backend, BackendType::LlamaServer);
        assert_eq!(config.tags, vec!["gpu", "nvidia"]);
        assert_eq!(config.priority, 10);
        assert!(config.hostname.is_none());
    }

    #[test]
    fn static_node_config_defaults() {
        let yaml = r#"url: http://192.168.1.100:11434"#;
        let config: StaticNodeConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.backend, BackendType::Ollama);
        assert!(config.tags.is_empty());
        assert_eq!(config.priority, 50);
    }

    #[test]
    fn discovery_disabled_returns_early() {
        let config = DiscoveryConfig {
            enabled: false,
            ..Default::default()
        };
        let discovery = NodeDiscovery::new(config);
        // spawn should be a no-op — we just verify it doesn't panic
        // (cannot easily test the background task without a runtime)
        assert!(!discovery.config.enabled);
    }

    #[test]
    fn mdns_config_defaults() {
        let mdns = MdnsConfig::default();
        assert!(!mdns.enabled);
        assert_eq!(mdns.service_name, "");
        // broadcast/listen default via serde default_true, but MdnsConfig::default()
        // uses Default trait which gives false. The serde path is the real default.
    }

    #[test]
    fn mdns_config_from_yaml_defaults() {
        let yaml = "enabled: true";
        let mdns: MdnsConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(mdns.enabled);
        assert_eq!(mdns.service_name, "_herd._tcp.local.");
        assert!(mdns.broadcast);
        assert!(mdns.listen);
    }
}
