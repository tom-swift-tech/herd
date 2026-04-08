use crate::backend::BackendPool;
use crate::nodes::NodeDb;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;

/// Ollama /api/ps response
#[derive(Debug, Deserialize)]
struct OllamaPsResponse {
    #[serde(default)]
    models: Vec<OllamaPsModel>,
}

#[derive(Debug, Deserialize)]
struct OllamaPsModel {
    name: String,
}

/// Ollama /api/tags response
#[derive(Debug, Deserialize)]
struct OllamaTagsResponse {
    #[serde(default)]
    models: Vec<OllamaTagModel>,
}

#[derive(Debug, Deserialize)]
struct OllamaTagModel {
    #[allow(dead_code)]
    name: String,
}

pub struct NodeHealthPoller {
    client: reqwest::Client,
    poll_interval: Duration,
    tags_interval: Duration,
}

impl NodeHealthPoller {
    pub fn new(poll_interval_secs: u64, tags_interval_secs: u64) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            poll_interval: Duration::from_secs(poll_interval_secs),
            tags_interval: Duration::from_secs(tags_interval_secs),
        }
    }

    /// Spawn the poller as a background tokio task.
    /// After each poll cycle, routable nodes are synced into the BackendPool
    /// so the existing routing strategies can route to them.
    pub fn spawn(self, node_db: Arc<NodeDb>, pool: Arc<BackendPool>) {
        tokio::spawn(async move {
            let mut poll_ticker = tokio::time::interval(self.poll_interval);
            let mut tags_counter: u64 = 0;
            let tags_every = (self.tags_interval.as_secs() / self.poll_interval.as_secs()).max(1);

            loop {
                poll_ticker.tick().await;
                tags_counter += 1;
                let check_tags = tags_counter >= tags_every;
                if check_tags {
                    tags_counter = 0;
                }
                self.poll_all(&node_db, check_tags).await;
                self.sync_to_pool(&node_db, &pool).await;
            }
        });
    }

    /// Syncs routable registered nodes into the BackendPool so existing
    /// routing strategies can route to them alongside static config backends.
    async fn sync_to_pool(&self, node_db: &NodeDb, pool: &BackendPool) {
        let nodes = match node_db.get_routable_nodes() {
            Ok(n) => n,
            Err(e) => {
                tracing::error!("Failed to get routable nodes for pool sync: {}", e);
                return;
            }
        };

        // Get current pool entries to detect removals
        let current_names = pool.all().await;
        let node_names: std::collections::HashSet<String> = nodes
            .iter()
            .map(|n| format!("node:{}", n.hostname))
            .collect();

        // Remove node entries that are no longer routable
        for name in &current_names {
            if name.starts_with("node:") && !node_names.contains(name) {
                pool.remove(name).await;
                tracing::debug!("Removed node backend {} from pool", name);
            }
        }

        // Add or update node entries
        for node in &nodes {
            let backend_name = format!("node:{}", node.hostname);
            let backend = crate::config::Backend {
                name: backend_name.clone(),
                url: node.backend_url.clone(),
                priority: node.priority,
                tags: node.tags.clone(),
                ..Default::default()
            };

            if current_names.contains(&backend_name) {
                // Update existing entry
                if let Some(mut state) = pool.get(&backend_name).await {
                    state.config = backend;
                    state.models = node.models_loaded.clone();
                    state.healthy =
                        node.status == "healthy" || node.status == "degraded";
                    if node.vram_mb > 0 {
                        state.vram_total_mb = Some(node.vram_mb as u64);
                        state.vram_populated = true;
                    }
                    pool.update(state).await;
                }
            } else {
                // Add new entry
                pool.add(backend).await;
                // Immediately set models and VRAM
                pool.update_models(&backend_name, node.models_loaded.clone())
                    .await;
                if node.vram_mb > 0 {
                    pool.set_vram(&backend_name, node.vram_mb as u64).await;
                }
                tracing::info!(
                    "Added node backend {} to pool ({})",
                    backend_name,
                    node.backend_url
                );
            }
        }
    }

    async fn poll_all(&self, node_db: &NodeDb, check_tags: bool) {
        let nodes = match node_db.get_pollable_nodes() {
            Ok(n) => n,
            Err(e) => {
                tracing::error!("Failed to get pollable nodes: {}", e);
                return;
            }
        };

        for node in &nodes {
            self.poll_node(node_db, node, check_tags).await;
        }
    }

    async fn poll_node(&self, node_db: &NodeDb, node: &crate::nodes::Node, check_tags: bool) {
        let base_url = node.backend_url.trim_end_matches('/');

        // GET /api/ps — loaded models
        let ps_url = format!("{}/api/ps", base_url);
        let ps_result = self.client.get(&ps_url).send().await;

        match ps_result {
            Ok(resp) if resp.status().is_success() => {
                let models_loaded: Vec<String> = match resp.json::<OllamaPsResponse>().await {
                    Ok(ps) => ps.models.into_iter().map(|m| m.name).collect(),
                    Err(_) => vec![],
                };

                let status = "healthy";

                // Optionally check /api/tags for available model count
                let models_available = if check_tags {
                    let tags_url = format!("{}/api/tags", base_url);
                    match self.client.get(&tags_url).send().await {
                        Ok(r) if r.status().is_success() => {
                            match r.json::<OllamaTagsResponse>().await {
                                Ok(tags) => Some(tags.models.len() as u32),
                                Err(_) => None,
                            }
                        }
                        _ => None,
                    }
                } else {
                    None
                };

                if let Err(e) =
                    node_db.update_health(&node.id, status, &models_loaded, models_available)
                {
                    tracing::error!(
                        "Failed to update health for node {}: {}",
                        node.hostname,
                        e
                    );
                }
            }
            Ok(resp) => {
                // Non-success status — mark degraded or unreachable
                tracing::warn!(
                    "Node {} ({}) returned status {} from /api/ps",
                    node.hostname,
                    base_url,
                    resp.status()
                );
                let new_status = if node.status == "healthy" || node.status == "degraded" {
                    "degraded"
                } else {
                    "unreachable"
                };
                if let Err(e) = node_db.update_health(&node.id, new_status, &[], None) {
                    tracing::error!(
                        "Failed to update health for node {}: {}",
                        node.hostname,
                        e
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    "Node {} ({}) health check failed: {}",
                    node.hostname,
                    base_url,
                    e
                );
                let new_status = match node.status.as_str() {
                    "healthy" => "degraded",
                    "degraded" => "unreachable",
                    _ => "unreachable",
                };
                if let Err(e) = node_db.update_health(&node.id, new_status, &[], None) {
                    tracing::error!(
                        "Failed to update health for node {}: {}",
                        node.hostname,
                        e
                    );
                }
            }
        }
    }
}
