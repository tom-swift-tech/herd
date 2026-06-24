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
    /// Measured in-flight request count (llama-server `/slots` busy count).
    /// `None` when the backend can't report it (Ollama, `/slots` disabled, or
    /// any probe failure) — an honest "unmeasured", never a fake 0.
    pub queue_depth: Option<u32>,
    /// Concurrency limit the backend launched with (llama-server `/props`
    /// `total_slots`). `None` when unavailable.
    pub max_concurrent: Option<u32>,
    /// Context-window size reported by llama-server `/props`
    /// `default_generation_settings.n_ctx`. `None` for Ollama,
    /// openai-compat, or any probe failure — best-effort, never affects
    /// reachability.
    pub context_len: Option<u32>,
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

/// `default_generation_settings` block inside a llama-server `/props` response.
/// Only the fields Herd reads are listed; the server may send many more.
#[derive(Debug, Deserialize, Default)]
struct LlamaGenSettings {
    /// The context-window size the server was launched with.
    #[serde(default)]
    n_ctx: Option<u32>,
}

/// llama-server `/props` (subset): `total_slots` is the `--parallel` limit;
/// `default_generation_settings.n_ctx` is the served context-window size.
#[derive(Debug, Deserialize)]
struct LlamaProps {
    #[serde(default)]
    total_slots: Option<u32>,
    #[serde(default)]
    default_generation_settings: LlamaGenSettings,
}

/// llama-server `/slots` entry (subset): `is_processing` marks a busy slot.
#[derive(Debug, Deserialize)]
struct LlamaSlot {
    #[serde(default)]
    is_processing: bool,
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

/// Parse a llama-server `/props` body into its `total_slots` (concurrency limit).
pub fn parse_llama_total_slots(body: &str) -> Option<u32> {
    serde_json::from_str::<LlamaProps>(body).ok()?.total_slots
}

/// Parse a llama-server `/props` body into the served context-window size
/// (`default_generation_settings.n_ctx`). Returns `None` when the field is
/// absent or the body is malformed — caller treats absence as neutral.
pub fn parse_llama_ctx_len(body: &str) -> Option<u32> {
    serde_json::from_str::<LlamaProps>(body)
        .ok()?
        .default_generation_settings
        .n_ctx
}

/// Parse a llama-server `/slots` body into the count of busy slots. `[]` →
/// `Some(0)` (idle, a real signal); a non-array / malformed body → `None`.
pub fn parse_llama_busy_slots(body: &str) -> Option<u32> {
    let slots: Vec<LlamaSlot> = serde_json::from_str(body).ok()?;
    Some(slots.iter().filter(|s| s.is_processing).count() as u32)
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

    /// Best-effort llama-server load probe: `max_concurrent` and `context_len`
    /// from `/props`, `queue_depth` from `/slots` (busy count). Each is
    /// independent — a missing or disabled endpoint yields `None` for just that
    /// value and never affects reachability.
    ///
    /// Returns `(queue_depth, max_concurrent, context_len)`.
    async fn probe_llama_load(&self) -> (Option<u32>, Option<u32>, Option<u32>) {
        let props_body = self.get_body("/props").await;
        let max_concurrent = props_body.as_deref().and_then(parse_llama_total_slots);
        let context_len = props_body.as_deref().and_then(parse_llama_ctx_len);
        let queue_depth = self
            .get_body("/slots")
            .await
            .and_then(|b| parse_llama_busy_slots(&b));
        (queue_depth, max_concurrent, context_len)
    }

    pub async fn probe(&self) -> ProbeOutcome {
        match self.override_backend {
            Some(BackendType::Ollama) => {
                // Ollama exposes no slot/concurrency/context telemetry → all stay None.
                self.outcome(
                    BackendType::Ollama,
                    self.probe_ollama().await,
                    None,
                    None,
                    None,
                )
            }
            Some(backend @ (BackendType::LlamaServer | BackendType::OpenAICompat)) => {
                let models = self.probe_openai().await;
                let (queue_depth, max_concurrent, context_len) = if models.is_some() {
                    self.probe_llama_load().await
                } else {
                    (None, None, None)
                };
                self.outcome(backend, models, queue_depth, max_concurrent, context_len)
            }
            None => {
                if let Some(models) = self.probe_ollama().await {
                    return ProbeOutcome {
                        backend: BackendType::Ollama,
                        models_loaded: models,
                        reachable: true,
                        queue_depth: None,
                        max_concurrent: None,
                        context_len: None,
                    };
                }
                let models = self.probe_openai().await;
                let (queue_depth, max_concurrent, context_len) = if models.is_some() {
                    self.probe_llama_load().await
                } else {
                    (None, None, None)
                };
                self.outcome(
                    BackendType::LlamaServer,
                    models,
                    queue_depth,
                    max_concurrent,
                    context_len,
                )
            }
        }
    }

    fn outcome(
        &self,
        backend: BackendType,
        models: Option<Vec<String>>,
        queue_depth: Option<u32>,
        max_concurrent: Option<u32>,
        context_len: Option<u32>,
    ) -> ProbeOutcome {
        match models {
            Some(models_loaded) => ProbeOutcome {
                backend,
                models_loaded,
                reachable: true,
                queue_depth,
                max_concurrent,
                context_len,
            },
            None => ProbeOutcome {
                backend,
                models_loaded: Vec::new(),
                reachable: false,
                queue_depth: None,
                max_concurrent: None,
                context_len: None,
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
        // Unmeasured load and context_len must be None, never a fake 0.
        assert_eq!(outcome.queue_depth, None);
        assert_eq!(outcome.max_concurrent, None);
        assert_eq!(outcome.context_len, None);
    }

    #[test]
    fn parses_total_slots_from_props() {
        let body = r#"{"total_slots":4,"model_path":"m.gguf","is_sleeping":false}"#;
        assert_eq!(parse_llama_total_slots(body), Some(4));
    }

    #[test]
    fn props_without_total_slots_is_none() {
        assert_eq!(parse_llama_total_slots(r#"{"model_path":"m.gguf"}"#), None);
        assert_eq!(parse_llama_total_slots("not json"), None);
    }

    #[test]
    fn counts_busy_slots() {
        // Two slots, one processing → busy count 1.
        let body = r#"[{"id":0,"is_processing":true},{"id":1,"is_processing":false}]"#;
        assert_eq!(parse_llama_busy_slots(body), Some(1));
    }

    #[test]
    fn empty_slots_array_is_zero_busy() {
        // Idle is a REAL signal (Some(0)), distinct from unmeasured (None).
        assert_eq!(parse_llama_busy_slots("[]"), Some(0));
    }

    #[test]
    fn all_slots_busy_counts_all() {
        let body = r#"[{"is_processing":true},{"is_processing":true},{"is_processing":true}]"#;
        assert_eq!(parse_llama_busy_slots(body), Some(3));
    }

    #[test]
    fn malformed_or_disabled_slots_is_none() {
        // `/slots` disabled returns a non-array error object → None (unmeasured).
        assert_eq!(
            parse_llama_busy_slots(r#"{"error":"slots disabled"}"#),
            None
        );
        assert_eq!(parse_llama_busy_slots("not json"), None);
    }

    #[test]
    fn parses_ctx_len_from_props() {
        // Full /props body with default_generation_settings.n_ctx present.
        let body = r#"{
            "total_slots": 4,
            "model_path": "qwen3-32b.gguf",
            "default_generation_settings": {
                "n_ctx": 32768,
                "temperature": 0.8
            }
        }"#;
        assert_eq!(parse_llama_ctx_len(body), Some(32768));
    }

    #[test]
    fn ctx_len_absent_when_field_missing() {
        // /props without default_generation_settings → None (old server).
        let body = r#"{"total_slots":4,"model_path":"m.gguf","is_sleeping":false}"#;
        assert_eq!(parse_llama_ctx_len(body), None);
    }

    #[test]
    fn ctx_len_absent_when_n_ctx_missing_from_settings() {
        // default_generation_settings present but n_ctx absent.
        let body = r#"{"total_slots":2,"default_generation_settings":{"temperature":0.7}}"#;
        assert_eq!(parse_llama_ctx_len(body), None);
    }

    #[test]
    fn ctx_len_absent_on_malformed_body() {
        assert_eq!(parse_llama_ctx_len("not json"), None);
        assert_eq!(parse_llama_ctx_len(""), None);
    }

    #[test]
    fn props_body_populates_both_slots_and_ctx_len() {
        // Both total_slots and n_ctx coexist; each parser is independent.
        let body = r#"{
            "total_slots": 8,
            "default_generation_settings": { "n_ctx": 131072 }
        }"#;
        assert_eq!(parse_llama_total_slots(body), Some(8));
        assert_eq!(parse_llama_ctx_len(body), Some(131_072));
    }
}
