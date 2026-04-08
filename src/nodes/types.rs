use crate::config::BackendType;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Registration payload from herd-tune scripts
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeRegistration {
    pub hostname: String,

    /// Legacy field — used by Ollama backends. For llama-server, use backend_url.
    #[serde(default)]
    pub ollama_url: String,

    /// Preferred URL field for all backend types. Falls back to ollama_url if not set.
    #[serde(default)]
    pub backend_url: Option<String>,

    /// Backend type: "ollama" (default) or "llama-server"
    #[serde(default)]
    pub backend: BackendType,

    /// Stable machine identifier (preferred over hostname for upsert).
    #[serde(default)]
    pub node_id: Option<String>,

    #[serde(default)]
    pub gpu: Option<String>,

    /// GPU vendor: "nvidia", "amd", "intel", "none"
    #[serde(default)]
    pub gpu_vendor: Option<String>,

    /// GPU model name (e.g., "NVIDIA GeForce RTX 5090")
    #[serde(default)]
    pub gpu_model: Option<String>,

    /// GPU compute backend: "cuda", "rocm", "sycl", "vulkan", "cpu"
    #[serde(default)]
    pub gpu_backend: Option<String>,

    /// CUDA version (NVIDIA only, e.g., "13.1")
    #[serde(default)]
    pub cuda_version: Option<String>,

    #[serde(default)]
    pub vram_mb: u32,
    #[serde(default)]
    pub ram_mb: u32,
    #[serde(default)]
    pub ollama_version: Option<String>,

    /// llama.cpp build version (e.g., "b8678")
    #[serde(default)]
    pub backend_version: Option<String>,

    #[serde(default)]
    pub models_available: u32,
    #[serde(default)]
    pub models_loaded: Vec<String>,

    /// GGUF file paths (llama-server only)
    #[serde(default)]
    pub model_paths: Vec<String>,

    /// Capabilities: ["cuda", "flash_attn", "moe", etc.]
    #[serde(default)]
    pub capabilities: Vec<String>,

    /// GPU driver version string (e.g., "572.83")
    #[serde(default)]
    pub gpu_driver_version: Option<String>,

    /// Max context length the backend supports
    #[serde(default = "default_max_context_len")]
    pub max_context_len: u32,

    #[serde(default)]
    pub recommended_config: serde_json::Value,
    #[serde(default)]
    pub config_applied: bool,
    #[serde(default)]
    pub herd_tune_version: Option<String>,
    #[serde(default)]
    pub os: Option<String>,
    #[serde(default)]
    pub registered_at: Option<String>,
}

fn default_max_context_len() -> u32 {
    4096
}

impl NodeRegistration {
    /// Returns the effective backend URL, preferring backend_url over ollama_url.
    pub fn effective_url(&self) -> &str {
        self.backend_url.as_deref().unwrap_or(&self.ollama_url)
    }
}

impl Default for NodeRegistration {
    fn default() -> Self {
        Self {
            hostname: String::new(),
            ollama_url: String::new(),
            backend_url: None,
            backend: BackendType::default(),
            node_id: None,
            gpu: None,
            gpu_vendor: None,
            gpu_model: None,
            gpu_backend: None,
            cuda_version: None,
            vram_mb: 0,
            ram_mb: 0,
            ollama_version: None,
            backend_version: None,
            models_available: 0,
            models_loaded: Vec::new(),
            model_paths: Vec::new(),
            capabilities: Vec::new(),
            gpu_driver_version: None,
            max_context_len: 4096,
            recommended_config: serde_json::Value::Object(Default::default()),
            config_applied: false,
            herd_tune_version: None,
            os: None,
            registered_at: None,
        }
    }
}

/// Stored node record from SQLite
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub node_id: Option<String>,
    pub hostname: String,
    /// Effective backend URL (unified field)
    pub backend_url: String,
    pub backend: BackendType,
    pub backend_version: Option<String>,
    pub gpu: Option<String>,
    pub gpu_vendor: Option<String>,
    pub gpu_model: Option<String>,
    pub gpu_backend: Option<String>,
    pub cuda_version: Option<String>,
    pub vram_mb: u32,
    pub ram_mb: u32,
    pub max_concurrent: u32,
    pub ollama_version: Option<String>,
    pub os: Option<String>,
    pub status: String,
    pub priority: u32,
    pub enabled: bool,
    pub tags: Vec<String>,
    pub models_available: u32,
    pub models_loaded: Vec<String>,
    pub model_paths: Vec<String>,
    pub capabilities: Vec<String>,
    pub gpu_driver_version: Option<String>,
    pub max_context_len: u32,
    pub recommended_config: serde_json::Value,
    pub config_applied: bool,
    pub last_health_check: Option<String>,
    pub registered_at: String,
    pub updated_at: String,
}

/// Entry in a node's model registry, derived from model_paths and models_loaded.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRegistryEntry {
    pub file_name: String,
    pub file_path: String,
    pub file_size_bytes: Option<u64>,
    pub loaded: bool,
}

impl Node {
    /// For llama-server nodes, compute model registry from model_paths + models_loaded.
    pub fn model_registry(&self) -> Vec<ModelRegistryEntry> {
        self.model_paths
            .iter()
            .map(|path| {
                let file_name = Path::new(path)
                    .file_name()
                    .map(|f| f.to_string_lossy().to_string())
                    .unwrap_or_else(|| path.clone());
                let loaded = self
                    .models_loaded
                    .iter()
                    .any(|m| m == &file_name || m == path);
                ModelRegistryEntry {
                    file_name,
                    file_path: path.clone(),
                    file_size_bytes: None,
                    loaded,
                }
            })
            .collect()
    }
}

/// Update payload for PUT /api/nodes/:id
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeUpdate {
    pub priority: Option<u32>,
    pub tags: Option<Vec<String>>,
    pub enabled: Option<bool>,
}

/// Response after registration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeRegistrationResponse {
    pub id: String,
    pub hostname: String,
    pub status: String,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_registration_defaults_to_ollama_backend() {
        let json = r#"{
            "hostname": "test",
            "ollama_url": "http://test:11434"
        }"#;
        let reg: NodeRegistration = serde_json::from_str(json).unwrap();
        assert_eq!(reg.backend, crate::config::BackendType::Ollama);
        assert!(reg.backend_url.is_none());
    }

    #[test]
    fn node_registration_llama_server_fields() {
        let json = r#"{
            "hostname": "citadel",
            "backend": "llama-server",
            "backend_url": "http://citadel:8090",
            "gpu_vendor": "nvidia",
            "gpu_model": "NVIDIA GeForce RTX 5090",
            "gpu_backend": "cuda",
            "cuda_version": "13.1",
            "vram_mb": 32768,
            "ram_mb": 131072,
            "models_loaded": ["gemma-4-26B.gguf"],
            "capabilities": ["cuda", "flash_attn"]
        }"#;
        let reg: NodeRegistration = serde_json::from_str(json).unwrap();
        assert_eq!(reg.backend, crate::config::BackendType::LlamaServer);
        assert_eq!(reg.backend_url.as_deref(), Some("http://citadel:8090"));
        assert_eq!(reg.gpu_vendor.as_deref(), Some("nvidia"));
        assert_eq!(reg.capabilities, vec!["cuda", "flash_attn"]);
    }

    #[test]
    fn node_registration_backward_compat_ollama() {
        // Old herd-tune scripts only send ollama_url, no backend field
        let json = r#"{
            "hostname": "minipc",
            "ollama_url": "http://minipc:11434",
            "ollama_version": "0.16.1",
            "models_available": 42,
            "models_loaded": ["qwen3:32b"]
        }"#;
        let reg: NodeRegistration = serde_json::from_str(json).unwrap();
        assert_eq!(reg.backend, crate::config::BackendType::Ollama);
        assert_eq!(reg.ollama_url, "http://minipc:11434");
        assert!(reg.backend_url.is_none());
    }

    #[test]
    fn effective_url_prefers_backend_url() {
        let reg = NodeRegistration {
            hostname: "test".to_string(),
            ollama_url: "http://test:11434".to_string(),
            backend_url: Some("http://test:8090".to_string()),
            ..Default::default()
        };
        assert_eq!(reg.effective_url(), "http://test:8090");
    }

    #[test]
    fn effective_url_falls_back_to_ollama_url() {
        let reg = NodeRegistration {
            hostname: "test".to_string(),
            ollama_url: "http://test:11434".to_string(),
            ..Default::default()
        };
        assert_eq!(reg.effective_url(), "http://test:11434");
    }

    #[test]
    fn model_registry_exact_match_only() {
        let node = Node {
            id: "test".to_string(),
            node_id: None,
            hostname: "test".to_string(),
            backend_url: "http://test:8090".to_string(),
            backend: crate::config::BackendType::LlamaServer,
            backend_version: None,
            gpu: None,
            gpu_vendor: None,
            gpu_model: None,
            gpu_backend: None,
            cuda_version: None,
            vram_mb: 0,
            ram_mb: 0,
            max_concurrent: 1,
            ollama_version: None,
            os: None,
            status: "healthy".to_string(),
            priority: 10,
            enabled: true,
            tags: vec![],
            models_available: 0,
            models_loaded: vec!["qwen".to_string()],
            model_paths: vec![
                "/models/qwen3-32b.gguf".to_string(),
                "/models/qwen.gguf".to_string(),
            ],
            capabilities: vec![],
            gpu_driver_version: None,
            max_context_len: 4096,
            recommended_config: serde_json::Value::Object(Default::default()),
            config_applied: false,
            last_health_check: None,
            registered_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        };
        let registry = node.model_registry();
        // "qwen" should NOT match "qwen3-32b.gguf" (no contains matching)
        assert!(
            !registry[0].loaded,
            "qwen3-32b.gguf should not match 'qwen'"
        );
        // "qwen" SHOULD match "qwen.gguf" (exact filename match — but filename is "qwen.gguf", not "qwen")
        assert!(
            !registry[1].loaded,
            "qwen.gguf filename does not exactly match 'qwen'"
        );
    }

    #[test]
    fn model_registry_matches_by_path() {
        let node = Node {
            id: "test".to_string(),
            node_id: None,
            hostname: "test".to_string(),
            backend_url: "http://test:8090".to_string(),
            backend: crate::config::BackendType::LlamaServer,
            backend_version: None,
            gpu: None,
            gpu_vendor: None,
            gpu_model: None,
            gpu_backend: None,
            cuda_version: None,
            vram_mb: 0,
            ram_mb: 0,
            max_concurrent: 1,
            ollama_version: None,
            os: None,
            status: "healthy".to_string(),
            priority: 10,
            enabled: true,
            tags: vec![],
            models_available: 0,
            models_loaded: vec![
                "gemma-4-26B.gguf".to_string(),
                "/models/qwen3-32b.gguf".to_string(),
            ],
            model_paths: vec![
                "/models/gemma-4-26B.gguf".to_string(),
                "/models/qwen3-32b.gguf".to_string(),
            ],
            capabilities: vec![],
            gpu_driver_version: None,
            max_context_len: 4096,
            recommended_config: serde_json::Value::Object(Default::default()),
            config_applied: false,
            last_health_check: None,
            registered_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        };
        let registry = node.model_registry();
        // Match by filename
        assert!(registry[0].loaded, "gemma-4-26B.gguf matches by filename");
        // Match by full path
        assert!(registry[1].loaded, "/models/qwen3-32b.gguf matches by path");
    }
}
