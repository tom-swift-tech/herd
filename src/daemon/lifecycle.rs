//! Local inference process probing for `herd agent`.
//!
//! v1.2 only *observes* an already-running backend (Ollama or
//! llama-server/openai-compat); it does not spawn or supervise processes.
//! Spawn/supervision — including the v1.4 `rpc-server` role for
//! pipeline-parallel groups — slots in here when those PRs land. Probe paths
//! mirror `backend/discovery.rs`: `/api/ps` for Ollama loaded models,
//! `/v1/models` for OpenAI-compatible servers.

use crate::config::BackendType;
use serde::Deserialize;
use std::time::Duration;

/// Roles the local inference process can take.
///
/// v1.4 adds an `RpcServer` variant here (llama.cpp pipeline-parallel
/// worker); it is intentionally absent until the daemon can actually spawn
/// and supervise one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalBackend {
    Ollama,
    LlamaServer,
    OpenAICompat,
}

impl LocalBackend {
    pub fn backend_type(&self) -> BackendType {
        match self {
            LocalBackend::Ollama => BackendType::Ollama,
            LocalBackend::LlamaServer => BackendType::LlamaServer,
            LocalBackend::OpenAICompat => BackendType::OpenAICompat,
        }
    }
}

/// Result of probing the local backend for one heartbeat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeOutcome {
    pub backend: BackendType,
    pub models_loaded: Vec<String>,
    pub reachable: bool,
}

/// Ollama `/api/ps` response (currently loaded models).
#[derive(Debug, Deserialize)]
struct OllamaPs {
    #[serde(default)]
    models: Vec<OllamaPsModel>,
}

#[derive(Debug, Deserialize)]
struct OllamaPsModel {
    #[serde(default)]
    name: String,
    #[serde(default)]
    model: String,
}

/// OpenAI-compatible `/v1/models` response (llama-server always reports its
/// loaded model here).
#[derive(Debug, Deserialize)]
struct OpenAIModels {
    #[serde(default)]
    data: Vec<OpenAIModel>,
}

#[derive(Debug, Deserialize)]
struct OpenAIModel {
    id: String,
}

/// Parse an Ollama `/api/ps` body into loaded model names. Prefers the
/// fully-qualified `model` field, falling back to `name` (same preference as
/// `backend/discovery.rs`).
pub fn parse_ollama_ps(body: &str) -> Option<Vec<String>> {
    let ps: OllamaPs = serde_json::from_str(body).ok()?;
    Some(
        ps.models
            .into_iter()
            .map(|m| if m.model.is_empty() { m.name } else { m.model })
            .filter(|m| !m.is_empty())
            .collect(),
    )
}

/// Parse an OpenAI-compatible `/v1/models` body into model ids.
pub fn parse_openai_models(body: &str) -> Option<Vec<String>> {
    let models: OpenAIModels = serde_json::from_str(body).ok()?;
    Some(models.data.into_iter().map(|m| m.id).collect())
}

/// Probes the local inference backend. With no explicit backend type, it
/// auto-detects: `/api/ps` answers → Ollama; else `/v1/models` answers →
/// llama-server. (Ollama also serves `/v1/models`, so the Ollama-only
/// endpoint must be probed first.)
pub struct LocalProbe {
    client: reqwest::Client,
    base_url: String,
    override_backend: Option<BackendType>,
}

impl LocalProbe {
    pub fn new(base_url: String, override_backend: Option<BackendType>) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .build()?;
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            override_backend,
        })
    }

    async fn get_body(&self, path: &str) -> Option<String> {
        let url = format!("{}{}", self.base_url, path);
        let resp = self.client.get(&url).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.text().await.ok()
    }

    async fn probe_ollama(&self) -> Option<Vec<String>> {
        let body = self.get_body("/api/ps").await?;
        parse_ollama_ps(&body)
    }

    async fn probe_openai(&self) -> Option<Vec<String>> {
        let body = self.get_body("/v1/models").await?;
        parse_openai_models(&body)
    }

    pub async fn probe(&self) -> ProbeOutcome {
        match self.override_backend {
            Some(BackendType::Ollama) => {
                self.outcome(BackendType::Ollama, self.probe_ollama().await)
            }
            Some(backend @ (BackendType::LlamaServer | BackendType::OpenAICompat)) => {
                self.outcome(backend, self.probe_openai().await)
            }
            None => {
                if let Some(models) = self.probe_ollama().await {
                    return ProbeOutcome {
                        backend: BackendType::Ollama,
                        models_loaded: models,
                        reachable: true,
                    };
                }
                self.outcome(BackendType::LlamaServer, self.probe_openai().await)
            }
        }
    }

    fn outcome(&self, backend: BackendType, models: Option<Vec<String>>) -> ProbeOutcome {
        match models {
            Some(models_loaded) => ProbeOutcome {
                backend,
                models_loaded,
                reachable: true,
            },
            None => ProbeOutcome {
                backend,
                models_loaded: Vec::new(),
                reachable: false,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ollama_ps_preferring_model_field() {
        let body = r#"{"models":[{"name":"qwen3:32b","model":"qwen3:32b-q4_K_M"},{"name":"gemma3:4b","model":""}]}"#;
        assert_eq!(
            parse_ollama_ps(body).unwrap(),
            vec!["qwen3:32b-q4_K_M".to_string(), "gemma3:4b".to_string()]
        );
    }

    #[test]
    fn parses_ollama_ps_with_nothing_loaded() {
        assert_eq!(
            parse_ollama_ps(r#"{"models":[]}"#).unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(parse_ollama_ps(r#"{}"#).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn malformed_ollama_ps_returns_none() {
        assert!(parse_ollama_ps("not json").is_none());
    }

    #[test]
    fn parses_openai_models_body() {
        let body = r#"{"object":"list","data":[{"id":"qwen3-32b.gguf","object":"model"}]}"#;
        assert_eq!(
            parse_openai_models(body).unwrap(),
            vec!["qwen3-32b.gguf".to_string()]
        );
    }

    #[test]
    fn openai_models_missing_data_defaults_empty() {
        assert_eq!(parse_openai_models(r#"{}"#).unwrap(), Vec::<String>::new());
        assert!(parse_openai_models("not json").is_none());
    }

    #[test]
    fn local_backend_maps_to_backend_type() {
        assert_eq!(LocalBackend::Ollama.backend_type(), BackendType::Ollama);
        assert_eq!(
            LocalBackend::LlamaServer.backend_type(),
            BackendType::LlamaServer
        );
        assert_eq!(
            LocalBackend::OpenAICompat.backend_type(),
            BackendType::OpenAICompat
        );
    }

    #[tokio::test]
    async fn unreachable_backend_yields_unreachable_outcome() {
        // Port 9 (discard) on localhost is virtually never listening.
        let probe = LocalProbe::new("http://127.0.0.1:9".into(), None).unwrap();
        let outcome = probe.probe().await;
        assert!(!outcome.reachable);
        assert!(outcome.models_loaded.is_empty());
        assert_eq!(outcome.backend, BackendType::LlamaServer);
    }
}
