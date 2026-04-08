use crate::config::BackendType;
use crate::server::AppState;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;

// ---------------------------------------------------------------------------
// Search cache
// ---------------------------------------------------------------------------

struct CacheEntry {
    results: Vec<ModelSearchResult>,
    created_at: Instant,
}

pub struct SearchCache {
    entries: Arc<RwLock<HashMap<String, CacheEntry>>>,
}

impl Default for SearchCache {
    fn default() -> Self {
        Self {
            entries: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl SearchCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get cached results if they exist and are less than `ttl` old.
    async fn get(&self, key: &str, ttl: std::time::Duration) -> Option<Vec<ModelSearchResult>> {
        let map = self.entries.read().await;
        if let Some(entry) = map.get(key) {
            if entry.created_at.elapsed() < ttl {
                return Some(entry.results.clone());
            }
        }
        None
    }

    /// Store results in cache, evicting expired entries when the map grows large.
    async fn put(&self, key: String, results: Vec<ModelSearchResult>) {
        let mut map = self.entries.write().await;
        // Evict expired entries if cache is large
        if map.len() > 100 {
            let now = Instant::now();
            let ttl = std::time::Duration::from_secs(300);
            map.retain(|_, e| now.duration_since(e.created_at) < ttl);
        }
        map.insert(
            key,
            CacheEntry {
                results,
                created_at: Instant::now(),
            },
        );
    }
}

// Lazy-initialized global search cache, separate from AppState for simplicity.
static SEARCH_CACHE: std::sync::OnceLock<SearchCache> = std::sync::OnceLock::new();

fn cache() -> &'static SearchCache {
    SEARCH_CACHE.get_or_init(SearchCache::new)
}

const CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(300); // 5 minutes

// ---------------------------------------------------------------------------
// Request / response types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct ModelSearchParams {
    pub q: String,
    #[serde(default)]
    pub quant: Option<String>,
    #[serde(default)]
    pub max_size_gb: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelSearchResult {
    pub repo_id: String,
    pub author: String,
    pub model_name: String,
    pub quant: Option<String>,
    pub file_name: String,
    pub file_size_bytes: u64,
    pub downloads: u64,
    pub updated_at: Option<String>,
    pub fits_on: Vec<String>,
}

#[derive(Serialize)]
pub struct ModelSearchResponse {
    pub results: Vec<ModelSearchResult>,
    pub cached: bool,
}

#[derive(Deserialize)]
pub struct DownloadRequest {
    pub repo_id: String,
    pub file_name: String,
    /// Reserved for future llama-server direct download support.
    #[serde(default)]
    #[allow(dead_code)]
    pub target_path: Option<String>,
}

#[derive(Serialize)]
pub struct DownloadResponse {
    pub download_id: String,
    pub status: String,
    pub message: String,
}

#[derive(Serialize)]
pub struct NodeModelsResponse {
    pub node_id: String,
    pub hostname: String,
    pub backend: String,
    pub models_loaded: Vec<String>,
    pub models_available: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_paths: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_registry: Option<Vec<crate::nodes::ModelRegistryEntry>>,
}

// ---------------------------------------------------------------------------
// HuggingFace API types (internal)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct HfModelEntry {
    id: String,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    downloads: u64,
    #[serde(default, alias = "lastModified")]
    last_modified: Option<String>,
    #[serde(default)]
    siblings: Vec<HfSibling>,
}

#[derive(Debug, Deserialize)]
struct HfSibling {
    rfilename: String,
    #[serde(default)]
    size: Option<u64>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract quant type from a GGUF filename.
/// Examples:
///   "model-Q4_K_M.gguf"       -> Some("Q4_K_M")
///   "gemma-4-26B-A4B-it-UD-Q4_K_M.gguf" -> Some("Q4_K_M")
///   "model-IQ3_XXS.gguf"      -> Some("IQ3_XXS")
///   "readme.md"                -> None
pub(crate) fn extract_quant(filename: &str) -> Option<String> {
    let stem = filename.strip_suffix(".gguf").unwrap_or(filename);
    extract_quant_manual(stem)
}

fn extract_quant_manual(stem: &str) -> Option<String> {
    // Split by '-' or '.' and look for common quant tokens
    let parts: Vec<&str> = stem.split(['-', '.']).collect();
    for part in parts.iter().rev() {
        let upper = part.to_uppercase();
        if is_quant_token(&upper) {
            return Some(upper);
        }
    }
    // Also try underscore-joined pairs from the end: "UD-Q4_K_M" => look for "Q4_K_M" in original
    // Search the full stem for well-known quant patterns
    for pat in QUANT_PATTERNS {
        if let Some(pos) = stem.to_uppercase().find(pat) {
            // Extract the matched portion from original string at same position
            let end = pos + pat.len();
            if end <= stem.len() {
                let matched = &stem[pos..end];
                return Some(matched.to_uppercase());
            }
        }
    }
    None
}

const QUANT_PATTERNS: &[&str] = &[
    "IQ4_NL", "IQ4_XS", "IQ3_XXS", "IQ3_XS", "IQ3_S", "IQ3_M", "IQ2_XXS", "IQ2_XS", "IQ2_S",
    "IQ2_M", "IQ1_S", "IQ1_M", "Q8_0", "Q6_K", "Q5_K_S", "Q5_K_M", "Q5_K_L", "Q5_K", "Q5_0",
    "Q5_1", "Q4_K_S", "Q4_K_M", "Q4_K_L", "Q4_K", "Q4_0", "Q4_1", "Q3_K_S", "Q3_K_M", "Q3_K_L",
    "Q3_K", "Q2_K_S", "Q2_K", "F32", "F16", "BF16",
];

fn is_quant_token(s: &str) -> bool {
    QUANT_PATTERNS.contains(&s)
}

/// Build a cache key from search params.
fn cache_key(params: &ModelSearchParams) -> String {
    format!(
        "{}|{}|{}",
        params.q,
        params.quant.as_deref().unwrap_or(""),
        params
            .max_size_gb
            .map(|v| format!("{:.1}", v))
            .unwrap_or_default()
    )
}

fn get_node_or_404(
    node_db: &crate::nodes::NodeDb,
    node_id: &str,
) -> Result<crate::nodes::Node, (StatusCode, Json<serde_json::Value>)> {
    node_db
        .get_node(node_id)
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("Database error: {}", e)})),
            )
        })?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Node not found"})),
            )
        })
}

// ---------------------------------------------------------------------------
// GET /api/models/search
// ---------------------------------------------------------------------------

pub async fn search_models(
    State(state): State<AppState>,
    Query(params): Query<ModelSearchParams>,
) -> Result<Json<ModelSearchResponse>, (StatusCode, Json<serde_json::Value>)> {
    let q = params.q.trim();
    if q.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Query parameter 'q' is required"})),
        ));
    }

    let key = cache_key(&params);

    // Check cache
    if let Some(cached) = cache().get(&key, CACHE_TTL).await {
        return Ok(Json(ModelSearchResponse {
            results: cached,
            cached: true,
        }));
    }

    // Query HuggingFace API
    let resp = state
        .client
        .get("https://huggingface.co/api/models")
        .query(&[
            ("search", q),
            ("filter", "gguf"),
            ("sort", "downloads"),
            ("direction", "-1"),
            ("limit", "20"),
        ])
        .header("User-Agent", "herd/0.9.0")
        .send()
        .await
        .map_err(|e| {
            tracing::error!("HuggingFace API request failed: {}", e);
            (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": format!("HuggingFace API error: {}", e)})),
            )
        })?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        tracing::warn!("HuggingFace API returned {}: {}", status, body);
        return Err((
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({
                "error": format!("HuggingFace API returned HTTP {}", status)
            })),
        ));
    }

    let hf_models: Vec<HfModelEntry> = resp.json().await.map_err(|e| {
        tracing::error!("Failed to parse HuggingFace response: {}", e);
        (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"error": "Failed to parse HuggingFace response"})),
        )
    })?;

    // Get registered nodes for VRAM comparison
    let nodes = state.node_db.list_nodes().unwrap_or_else(|e| {
        tracing::warn!("Failed to list nodes for VRAM compatibility: {}", e);
        vec![]
    });

    let mut results = Vec::new();

    for model in &hf_models {
        let author = model
            .author
            .clone()
            .unwrap_or_else(|| model.id.split('/').next().unwrap_or("unknown").to_string());
        let model_name = model.id.split('/').nth(1).unwrap_or(&model.id).to_string();

        // Find GGUF siblings
        for sib in &model.siblings {
            if !sib.rfilename.ends_with(".gguf") {
                continue;
            }

            let file_size = sib.size.unwrap_or(0);
            let quant = extract_quant(&sib.rfilename);

            // Apply quant filter
            if let Some(ref filter_quant) = params.quant {
                if let Some(ref q) = quant {
                    if !q.to_uppercase().contains(&filter_quant.to_uppercase()) {
                        continue;
                    }
                } else {
                    continue; // no quant detected, skip if filter is set
                }
            }

            // Apply size filter
            if let Some(max_gb) = params.max_size_gb {
                let size_gb = file_size as f64 / (1024.0 * 1024.0 * 1024.0);
                if size_gb > max_gb as f64 {
                    continue;
                }
            }

            // Compute fits_on: which nodes have enough VRAM
            let fits_on = compute_fits_on(&nodes, file_size);

            results.push(ModelSearchResult {
                repo_id: model.id.clone(),
                author: author.clone(),
                model_name: model_name.clone(),
                quant,
                file_name: sib.rfilename.clone(),
                file_size_bytes: file_size,
                downloads: model.downloads,
                updated_at: model.last_modified.clone(),
                fits_on,
            });
        }
    }

    // Cache results
    cache().put(key, results.clone()).await;

    Ok(Json(ModelSearchResponse {
        results,
        cached: false,
    }))
}

/// Determine which registered nodes have enough VRAM for the given file size.
/// Applies a 1.2x safety multiplier to account for KV cache and runtime buffers.
fn compute_fits_on(nodes: &[crate::nodes::Node], file_size_bytes: u64) -> Vec<String> {
    let file_size_mb = (file_size_bytes as f64 / (1024.0 * 1024.0)).ceil() as u64;
    let estimated_vram_mb = file_size_mb * 6 / 5; // ~1.2x safety margin for KV cache
    nodes
        .iter()
        .filter(|n| n.enabled && n.vram_mb as u64 >= estimated_vram_mb)
        .map(|n| n.hostname.clone())
        .collect()
}

// ---------------------------------------------------------------------------
// POST /api/nodes/:id/models/download
// ---------------------------------------------------------------------------

pub async fn download_model(
    State(state): State<AppState>,
    Path(node_id): Path<String>,
    Json(req): Json<DownloadRequest>,
) -> Result<Json<DownloadResponse>, (StatusCode, Json<serde_json::Value>)> {
    let node = get_node_or_404(&state.node_db, &node_id)?;

    match node.backend {
        BackendType::LlamaServer | BackendType::OpenAICompat => Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(serde_json::json!({
                "error": "Model download not supported for this backend type. Ollama backends only."
            })),
        )),
        BackendType::Ollama => {
            // Proxy to Ollama's pull API
            let pull_url = format!("{}/api/pull", node.backend_url.trim_end_matches('/'));

            // For Ollama, the model name is the repo_id (or a short tag)
            let pull_body = serde_json::json!({
                "name": req.repo_id,
                "stream": false,
            });

            // NOTE: download_id is generated for client tracking but not yet persisted
            // to the model_downloads table. Full progress tracking is a future enhancement.
            let download_id = uuid::Uuid::new_v4().to_string();

            // Fire the pull request using the long-timeout management client
            let client = state.mgmt_client.clone();
            let pull_url_clone = pull_url.clone();
            let pull_body_clone = pull_body.clone();

            tokio::spawn(async move {
                match client
                    .post(&pull_url_clone)
                    .json(&pull_body_clone)
                    .send()
                    .await
                {
                    Ok(resp) if resp.status().is_success() => {
                        tracing::info!("Ollama pull started for {}", pull_body_clone["name"]);
                    }
                    Ok(resp) => {
                        tracing::warn!(
                            "Ollama pull returned {}: {}",
                            resp.status(),
                            resp.text().await.unwrap_or_default()
                        );
                    }
                    Err(e) => {
                        tracing::error!("Ollama pull request failed: {}", e);
                    }
                }
            });

            Ok(Json(DownloadResponse {
                download_id,
                status: "pulling".to_string(),
                message: format!("Pull started on node {} via Ollama API", node.hostname),
            }))
        }
    }
}

// ---------------------------------------------------------------------------
// GET /api/nodes/:id/models
// ---------------------------------------------------------------------------

pub async fn list_node_models(
    State(state): State<AppState>,
    Path(node_id): Path<String>,
) -> Result<Json<NodeModelsResponse>, (StatusCode, Json<serde_json::Value>)> {
    let node = get_node_or_404(&state.node_db, &node_id)?;

    let (model_paths, model_registry) = match node.backend {
        BackendType::LlamaServer => {
            let registry = node.model_registry();
            (Some(node.model_paths.clone()), Some(registry))
        }
        BackendType::Ollama | BackendType::OpenAICompat => (None, None),
    };

    Ok(Json(NodeModelsResponse {
        node_id: node.id,
        hostname: node.hostname,
        backend: node.backend.to_string(),
        models_loaded: node.models_loaded,
        models_available: node.models_available,
        model_paths,
        model_registry,
    }))
}

// ---------------------------------------------------------------------------
// DELETE /api/nodes/:id/models/:model_name
// ---------------------------------------------------------------------------

pub async fn delete_node_model(
    State(state): State<AppState>,
    Path((node_id, model_name)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let node = get_node_or_404(&state.node_db, &node_id)?;

    match node.backend {
        BackendType::LlamaServer | BackendType::OpenAICompat => Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(serde_json::json!({
                "error": "Model deletion not supported for this backend type. Ollama backends only."
            })),
        )),
        BackendType::Ollama => {
            let delete_url = format!("{}/api/delete", node.backend_url.trim_end_matches('/'));
            let delete_body = serde_json::json!({ "name": model_name });

            let resp = state
                .client
                .delete(&delete_url)
                .json(&delete_body)
                .send()
                .await
                .map_err(|e| {
                    (
                        StatusCode::BAD_GATEWAY,
                        Json(
                            serde_json::json!({"error": format!("Failed to contact node: {}", e)}),
                        ),
                    )
                })?;

            if resp.status().is_success() {
                Ok(Json(serde_json::json!({
                    "status": "deleted",
                    "model": model_name,
                    "node": node.hostname
                })))
            } else {
                let status = resp.status().as_u16();
                let body = resp.text().await.unwrap_or_default();
                Err((
                    StatusCode::BAD_GATEWAY,
                    Json(serde_json::json!({
                        "error": format!("Ollama delete returned HTTP {}: {}", status, body)
                    })),
                ))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_quant_q4_k_m() {
        assert_eq!(
            extract_quant("gemma-4-26B-A4B-it-UD-Q4_K_M.gguf"),
            Some("Q4_K_M".to_string())
        );
    }

    #[test]
    fn extract_quant_q8_0() {
        assert_eq!(extract_quant("model-Q8_0.gguf"), Some("Q8_0".to_string()));
    }

    #[test]
    fn extract_quant_iq3_xxs() {
        assert_eq!(
            extract_quant("some-model-IQ3_XXS.gguf"),
            Some("IQ3_XXS".to_string())
        );
    }

    #[test]
    fn extract_quant_f16() {
        assert_eq!(extract_quant("model-F16.gguf"), Some("F16".to_string()));
    }

    #[test]
    fn extract_quant_none_for_non_gguf() {
        assert_eq!(extract_quant("README.md"), None);
    }

    #[test]
    fn extract_quant_none_for_no_quant_in_name() {
        assert_eq!(extract_quant("some-model.gguf"), None);
    }

    #[test]
    fn vram_compatibility_fits() {
        let nodes = vec![test_node("citadel", 32768)]; // 32 GB VRAM
        let file_size = 15_700_000_000u64; // 15.7 GB
        let fits = compute_fits_on(&nodes, file_size);
        assert_eq!(fits, vec!["citadel"]);
    }

    #[test]
    fn vram_compatibility_does_not_fit() {
        let nodes = vec![test_node("minipc", 16384)]; // 16 GB VRAM
        let file_size = 17_500_000_000u64; // 17.5 GB ≈ 16689 MB > 16384 MB
        let fits = compute_fits_on(&nodes, file_size);
        assert!(fits.is_empty());
    }

    #[test]
    fn vram_compatibility_mixed_fleet() {
        let nodes = vec![
            test_node("big-node", 32768),
            test_node("small-node", 8192),
            test_node("medium-node", 24576),
        ];
        // ~15 GB file — with 1.2x safety margin (~17167 MB), fits on big-node and medium-node
        let file_size = 15_000_000_000u64; // 14305 MB raw, ~17167 MB with 1.2x
        let fits = compute_fits_on(&nodes, file_size);
        assert!(fits.contains(&"big-node".to_string()));
        assert!(fits.contains(&"medium-node".to_string()));
        assert!(!fits.contains(&"small-node".to_string()));
    }

    #[test]
    fn parse_hf_model_response() {
        let json = r#"[{
            "id": "unsloth/gemma-4-26B-A4B-it-UD-Q4_K_M",
            "author": "unsloth",
            "downloads": 12345,
            "lastModified": "2026-04-01T00:00:00Z",
            "siblings": [
                {"rfilename": "gemma-4-26B-A4B-it-UD-Q4_K_M.gguf", "size": 15700000000},
                {"rfilename": "README.md", "size": 1234}
            ]
        }]"#;

        let models: Vec<HfModelEntry> = serde_json::from_str(json).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "unsloth/gemma-4-26B-A4B-it-UD-Q4_K_M");
        assert_eq!(models[0].author, Some("unsloth".to_string()));
        assert_eq!(models[0].downloads, 12345);

        let gguf_siblings: Vec<_> = models[0]
            .siblings
            .iter()
            .filter(|s| s.rfilename.ends_with(".gguf"))
            .collect();
        assert_eq!(gguf_siblings.len(), 1);
        assert_eq!(gguf_siblings[0].size, Some(15700000000));
    }

    #[test]
    fn download_request_validation() {
        let json = r#"{"repo_id": "unsloth/gemma-4", "file_name": "model.gguf"}"#;
        let req: DownloadRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.repo_id, "unsloth/gemma-4");
        assert_eq!(req.file_name, "model.gguf");
        assert!(req.target_path.is_none());

        let json2 = r#"{"repo_id": "foo/bar", "file_name": "x.gguf", "target_path": "/models"}"#;
        let req2: DownloadRequest = serde_json::from_str(json2).unwrap();
        assert_eq!(req2.target_path.as_deref(), Some("/models"));
    }

    #[tokio::test]
    async fn search_cache_hit_and_expiry() {
        let cache = SearchCache::new();
        let ttl = std::time::Duration::from_millis(100);

        // Put and get
        cache.put("test".to_string(), vec![dummy_result("a")]).await;
        let hit = cache.get("test", ttl).await;
        assert!(hit.is_some());
        assert_eq!(hit.unwrap().len(), 1);

        // Wait for expiry
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let miss = cache.get("test", ttl).await;
        assert!(miss.is_none());
    }

    #[test]
    fn cache_key_includes_all_params() {
        let p1 = ModelSearchParams {
            q: "gemma".to_string(),
            quant: Some("Q4_K_M".to_string()),
            max_size_gb: Some(16.0),
        };
        let p2 = ModelSearchParams {
            q: "gemma".to_string(),
            quant: None,
            max_size_gb: None,
        };
        assert_ne!(cache_key(&p1), cache_key(&p2));
    }

    // -- helpers --

    fn test_node(hostname: &str, vram_mb: u32) -> crate::nodes::Node {
        crate::nodes::Node {
            id: uuid::Uuid::new_v4().to_string(),
            node_id: None,
            hostname: hostname.to_string(),
            backend_url: format!("http://{}:11434", hostname),
            backend: BackendType::Ollama,
            backend_version: None,
            gpu: None,
            gpu_vendor: None,
            gpu_model: None,
            gpu_backend: None,
            cuda_version: None,
            vram_mb,
            ram_mb: 65536,
            max_concurrent: 1,
            ollama_version: None,
            os: None,
            status: "healthy".to_string(),
            priority: 10,
            enabled: true,
            tags: vec![],
            models_available: 0,
            models_loaded: vec![],
            model_paths: vec![],
            capabilities: vec![],
            gpu_driver_version: None,
            max_context_len: 4096,
            recommended_config: serde_json::json!({}),
            config_applied: false,
            last_health_check: None,
            registered_at: "2026-04-08T00:00:00Z".to_string(),
            updated_at: "2026-04-08T00:00:00Z".to_string(),
        }
    }

    fn dummy_result(name: &str) -> ModelSearchResult {
        ModelSearchResult {
            repo_id: format!("test/{}", name),
            author: "test".to_string(),
            model_name: name.to_string(),
            quant: Some("Q4_K_M".to_string()),
            file_name: format!("{}-Q4_K_M.gguf", name),
            file_size_bytes: 1_000_000,
            downloads: 100,
            updated_at: None,
            fits_on: vec![],
        }
    }
}
