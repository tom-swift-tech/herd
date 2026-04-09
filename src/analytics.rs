use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestLog {
    pub timestamp: i64, // Unix timestamp in seconds
    pub model: Option<String>,
    pub backend: String,
    pub duration_ms: u64,
    pub status: String, // "success" | "error"
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classified_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_in: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_out: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_per_second: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_eval_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eval_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_tier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_capability: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_model: Option<String>,
}

pub struct Analytics {
    log_path: PathBuf,
    file_lock: Arc<Mutex<()>>,
}

impl Analytics {
    pub fn new() -> Result<Self> {
        let log_dir = dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("Could not find home directory"))?
            .join(".herd");

        std::fs::create_dir_all(&log_dir)?;
        let log_path = log_dir.join("requests.jsonl");

        // Touch the file to ensure it exists, then drop the handle
        let _file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;
        drop(_file);

        Ok(Self {
            log_path,
            file_lock: Arc::new(Mutex::new(())),
        })
    }

    pub async fn log_request(&self, log: RequestLog) -> Result<()> {
        let _guard = self.file_lock.lock().await;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)?;
        let json = serde_json::to_string(&log)?;
        writeln!(file, "{}", json)?;
        file.flush()?;
        Ok(())
    }

    pub async fn get_stats(&self, since_seconds: i64) -> Result<AnalyticsStats> {
        let _guard = self.file_lock.lock().await;
        let cutoff = chrono::Utc::now().timestamp() - since_seconds;

        let file = std::fs::File::open(&self.log_path)?;
        let reader = BufReader::new(file);

        let mut total_requests = 0u64;
        let mut model_counts: HashMap<String, u64> = HashMap::new();
        let mut backend_counts: HashMap<String, u64> = HashMap::new();
        let mut minute_buckets: HashMap<i64, u64> = HashMap::new();
        let mut durations: Vec<u64> = Vec::new();

        // Token tracking
        let mut total_tokens_in = 0u64;
        let mut total_tokens_out = 0u64;
        let mut model_token_counts: HashMap<String, (u64, u64)> = HashMap::new();
        let mut tps_values: Vec<f32> = Vec::new();

        // Per-backend and per-model durations for latency breakdown
        let mut backend_durations: HashMap<String, Vec<u64>> = HashMap::new();
        let mut model_durations: HashMap<String, Vec<u64>> = HashMap::new();

        for line in reader.lines() {
            let line = line?;
            if let Ok(log) = serde_json::from_str::<RequestLog>(&line) {
                if log.timestamp >= cutoff {
                    total_requests += 1;

                    // Count by model
                    if let Some(model) = &log.model {
                        *model_counts.entry(model.clone()).or_insert(0) += 1;
                    }

                    // Count by backend
                    *backend_counts.entry(log.backend.clone()).or_insert(0) += 1;

                    // Timeline (group by minute)
                    let minute = (log.timestamp / 60) * 60;
                    *minute_buckets.entry(minute).or_insert(0) += 1;

                    // Durations for percentiles
                    if log.status == "success" {
                        durations.push(log.duration_ms);

                        // Per-backend latency
                        backend_durations
                            .entry(log.backend.clone())
                            .or_default()
                            .push(log.duration_ms);

                        // Per-model latency
                        if let Some(model) = &log.model {
                            model_durations
                                .entry(model.clone())
                                .or_default()
                                .push(log.duration_ms);
                        }
                    }

                    // Token aggregation
                    if let Some(tin) = log.tokens_in {
                        total_tokens_in += tin as u64;
                        if let Some(model) = &log.model {
                            model_token_counts.entry(model.clone()).or_insert((0, 0)).0 +=
                                tin as u64;
                        }
                    }
                    if let Some(tout) = log.tokens_out {
                        total_tokens_out += tout as u64;
                        if let Some(model) = &log.model {
                            model_token_counts.entry(model.clone()).or_insert((0, 0)).1 +=
                                tout as u64;
                        }
                    }

                    if let Some(tps) = log.tokens_per_second {
                        tps_values.push(tps);
                    }
                }
            }
        }

        // Convert minute buckets to sorted timeline
        let mut timeline_vec: Vec<(i64, u64)> = minute_buckets.into_iter().collect();
        timeline_vec.sort_by_key(|(ts, _)| *ts);

        // Calculate overall percentiles
        durations.sort();
        let overall = compute_percentiles(&durations);

        // Calculate per-backend latency percentiles
        let backend_latency: HashMap<String, LatencyPercentiles> = backend_durations
            .iter_mut()
            .map(|(k, v)| {
                v.sort();
                (k.clone(), compute_percentiles(v))
            })
            .collect();

        // Calculate per-model latency percentiles
        let model_latency: HashMap<String, LatencyPercentiles> = model_durations
            .iter_mut()
            .map(|(k, v)| {
                v.sort();
                (k.clone(), compute_percentiles(v))
            })
            .collect();

        // Average tokens per second
        let tokens_per_second_avg = if tps_values.is_empty() {
            0.0
        } else {
            tps_values.iter().sum::<f32>() / tps_values.len() as f32
        };

        // Estimated API cost
        let estimated_api_cost_usd = model_token_counts
            .iter()
            .map(|(model, (tin, tout))| estimate_api_cost(model, *tin, *tout))
            .sum();

        Ok(AnalyticsStats {
            total_requests,
            model_counts,
            backend_counts,
            timeline: timeline_vec,
            latency_p50: overall.p50,
            latency_p95: overall.p95,
            latency_p99: overall.p99,
            total_tokens_in,
            total_tokens_out,
            model_token_counts,
            backend_latency,
            model_latency,
            tokens_per_second_avg,
            estimated_api_cost_usd,
        })
    }

    /// Rotates the log file if it exceeds max_size_mb.
    /// Keeps up to max_files rotated files (.1, .2, etc.)
    pub async fn rotate_if_needed(&self, max_size_mb: u64, max_files: u32) -> Result<bool> {
        if max_size_mb == 0 || max_files == 0 {
            return Ok(false); // rotation disabled
        }

        let _guard = self.file_lock.lock().await;

        let metadata = match std::fs::metadata(&self.log_path) {
            Ok(m) => m,
            Err(_) => return Ok(false),
        };

        let size_mb = metadata.len() / (1024 * 1024);
        if size_mb < max_size_mb {
            return Ok(false); // not yet at limit
        }

        // Shift existing rotated files: .4 → .5 (deleted if > max_files), .3 → .4, .2 → .3, .1 → .2
        for i in (1..max_files).rev() {
            let from = self.log_path.with_extension(format!("jsonl.{}", i));
            let to = self.log_path.with_extension(format!("jsonl.{}", i + 1));
            if from.exists() {
                if i + 1 > max_files {
                    let _ = std::fs::remove_file(&from);
                } else {
                    let _ = std::fs::rename(&from, &to);
                }
            }
        }

        // Delete the oldest if it exceeds max_files
        let oldest = self
            .log_path
            .with_extension(format!("jsonl.{}", max_files + 1));
        if oldest.exists() {
            let _ = std::fs::remove_file(&oldest);
        }

        // Current → .1
        let rotated = self.log_path.with_extension("jsonl.1");
        if rotated.exists() {
            let _ = std::fs::remove_file(&rotated);
        }
        std::fs::rename(&self.log_path, &rotated)?;

        // Create fresh empty log file
        let _file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)?;

        tracing::info!("Rotated log file (was {}MB)", size_mb);
        Ok(true)
    }

    pub async fn cleanup_old(&self, days: i64) -> Result<usize> {
        let _guard = self.file_lock.lock().await;
        let cutoff = chrono::Utc::now().timestamp() - (days * 86400);

        let file = std::fs::File::open(&self.log_path)?;
        let reader = BufReader::new(file);

        let temp_path = self.log_path.with_extension("tmp");
        let mut temp_file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&temp_path)?;

        let mut _kept = 0;
        let mut removed = 0;

        for line in reader.lines() {
            let line = line?;
            if let Ok(log) = serde_json::from_str::<RequestLog>(&line) {
                if log.timestamp >= cutoff {
                    writeln!(temp_file, "{}", line)?;
                    _kept += 1;
                } else {
                    removed += 1;
                }
            }
        }

        temp_file.flush()?;
        replace_file(&temp_path, &self.log_path)?;

        Ok(removed)
    }
}

fn replace_file(from: &std::path::Path, to: &std::path::Path) -> Result<()> {
    if to.exists() {
        std::fs::remove_file(to)?;
    }
    std::fs::rename(from, to)?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatencyPercentiles {
    pub p50: u64,
    pub p95: u64,
    pub p99: u64,
}

/// Extract parameter count (in billions) from a model name.
/// Looks for a number followed by "b" (case-insensitive), e.g. "llama3:8b", "qwen2-72B".
/// Returns None if no match found.
pub fn extract_param_billions(model: &str) -> Option<u64> {
    let lower = model.to_lowercase();
    let bytes = lower.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'b' {
                // Check next char is not a letter (avoid matching "bin", "build", etc.)
                let next_is_letter = i + 1 < bytes.len() && bytes[i + 1].is_ascii_alphabetic();
                if !next_is_letter {
                    if let Ok(n) = lower[start..i].parse::<f64>() {
                        return Some(n as u64);
                    }
                }
            }
        }
        i += 1;
    }
    None
}

/// Estimate what the given token usage would cost on a commercial API.
/// Tier pricing (per million tokens):
///   <=8B: $0.10 input, $0.30 output
///   9-32B: $0.25 input, $0.75 output
///   33B+: $0.50 input, $1.50 output
pub fn estimate_api_cost(model: &str, tokens_in: u64, tokens_out: u64) -> f32 {
    let params = extract_param_billions(model);
    let (cost_in_per_m, cost_out_per_m) = match params {
        Some(b) if b <= 8 => (0.10_f32, 0.30_f32),
        Some(b) if b <= 32 => (0.25_f32, 0.75_f32),
        Some(_) => (0.50_f32, 1.50_f32),
        None => (0.25_f32, 0.75_f32), // default to middle tier
    };
    (tokens_in as f32 * cost_in_per_m / 1_000_000.0)
        + (tokens_out as f32 * cost_out_per_m / 1_000_000.0)
}

/// Compute p50/p95/p99 from a sorted slice of durations.
pub fn compute_percentiles(sorted: &[u64]) -> LatencyPercentiles {
    if sorted.is_empty() {
        return LatencyPercentiles {
            p50: 0,
            p95: 0,
            p99: 0,
        };
    }
    LatencyPercentiles {
        p50: sorted[sorted.len() / 2],
        p95: sorted[(sorted.len() * 95) / 100],
        p99: sorted[(sorted.len() * 99) / 100],
    }
}

#[derive(Debug, Serialize)]
pub struct AnalyticsStats {
    pub total_requests: u64,
    pub model_counts: HashMap<String, u64>,
    pub backend_counts: HashMap<String, u64>,
    pub timeline: Vec<(i64, u64)>, // (timestamp, count)
    pub latency_p50: u64,
    pub latency_p95: u64,
    pub latency_p99: u64,
    pub total_tokens_in: u64,
    pub total_tokens_out: u64,
    pub model_token_counts: HashMap<String, (u64, u64)>, // model -> (in, out)
    pub backend_latency: HashMap<String, LatencyPercentiles>,
    pub model_latency: HashMap<String, LatencyPercentiles>,
    pub tokens_per_second_avg: f32,
    pub estimated_api_cost_usd: f32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn request_log_with_request_id_serializes() {
        let log = RequestLog {
            timestamp: 1000,
            model: Some("test".into()),
            backend: "b1".into(),
            duration_ms: 100,
            status: "success".into(),
            path: "/api/generate".into(),
            request_id: Some("abc-123".into()),
            tier: None,
            classified_by: None,
            tokens_in: None,
            tokens_out: None,
            tokens_per_second: None,
            prompt_eval_ms: None,
            eval_ms: None,
            backend_type: None,
            auto_tier: None,
            auto_capability: None,
            auto_model: None,
        };
        let json = serde_json::to_string(&log).unwrap();
        assert!(json.contains("abc-123"));
    }

    #[test]
    fn request_log_without_request_id_omits_field() {
        let log = RequestLog {
            timestamp: 1000,
            model: None,
            backend: "b1".into(),
            duration_ms: 100,
            status: "success".into(),
            path: "/test".into(),
            request_id: None,
            tier: None,
            classified_by: None,
            tokens_in: None,
            tokens_out: None,
            tokens_per_second: None,
            prompt_eval_ms: None,
            eval_ms: None,
            backend_type: None,
            auto_tier: None,
            auto_capability: None,
            auto_model: None,
        };
        let json = serde_json::to_string(&log).unwrap();
        assert!(!json.contains("request_id"));
    }

    #[test]
    fn request_log_deserializes_without_request_id() {
        // Old logs without request_id field should still deserialize
        let json = r#"{"timestamp":1000,"model":null,"backend":"b1","duration_ms":100,"status":"success","path":"/test"}"#;
        let log: RequestLog = serde_json::from_str(json).unwrap();
        assert!(log.request_id.is_none());
    }

    #[test]
    fn config_defaults() {
        let config: crate::config::ObservabilityConfig = Default::default();
        assert_eq!(config.log_retention_days, 7);
        assert_eq!(config.log_max_size_mb, 100);
        assert_eq!(config.log_max_files, 5);
    }

    #[test]
    fn config_deserializes_log_settings() {
        let yaml = r#"
            metrics: true
            log_retention_days: 14
            log_max_size_mb: 50
            log_max_files: 3
        "#;
        let config: crate::config::ObservabilityConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.log_retention_days, 14);
        assert_eq!(config.log_max_size_mb, 50);
        assert_eq!(config.log_max_files, 3);
    }

    #[test]
    fn request_log_backward_compat_deserialize_old_format() {
        // Old JSONL without any of the new token/backend_type fields should still deserialize
        let json = r#"{"timestamp":1000,"model":"llama3:8b","backend":"gpu1","duration_ms":200,"status":"success","path":"/api/generate"}"#;
        let log: RequestLog = serde_json::from_str(json).unwrap();
        assert!(log.tokens_in.is_none());
        assert!(log.tokens_out.is_none());
        assert!(log.tokens_per_second.is_none());
        assert!(log.prompt_eval_ms.is_none());
        assert!(log.eval_ms.is_none());
        assert!(log.backend_type.is_none());
        assert!(log.request_id.is_none());
        assert!(log.tier.is_none());
        assert!(log.classified_by.is_none());
    }

    #[test]
    fn request_log_new_fields_round_trip() {
        let log = RequestLog {
            timestamp: 2000,
            model: Some("llama3:70b".into()),
            backend: "gpu2".into(),
            duration_ms: 500,
            status: "success".into(),
            path: "/v1/chat/completions".into(),
            request_id: Some("req-42".into()),
            tier: Some("heavy".into()),
            classified_by: Some("keyword".into()),
            tokens_in: Some(128),
            tokens_out: Some(256),
            tokens_per_second: Some(45.5),
            prompt_eval_ms: Some(80),
            eval_ms: Some(420),
            backend_type: Some("llama-server".into()),
            auto_tier: None,
            auto_capability: None,
            auto_model: None,
        };
        let json = serde_json::to_string(&log).unwrap();
        let deserialized: RequestLog = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.tokens_in, Some(128));
        assert_eq!(deserialized.tokens_out, Some(256));
        assert!((deserialized.tokens_per_second.unwrap() - 45.5).abs() < 0.01);
        assert_eq!(deserialized.prompt_eval_ms, Some(80));
        assert_eq!(deserialized.eval_ms, Some(420));
        assert_eq!(deserialized.backend_type.as_deref(), Some("llama-server"));
    }

    #[test]
    fn request_log_new_fields_omitted_when_none() {
        let log = RequestLog {
            timestamp: 1000,
            model: None,
            backend: "b1".into(),
            duration_ms: 100,
            status: "success".into(),
            path: "/test".into(),
            request_id: None,
            tier: None,
            classified_by: None,
            tokens_in: None,
            tokens_out: None,
            tokens_per_second: None,
            prompt_eval_ms: None,
            eval_ms: None,
            backend_type: None,
            auto_tier: None,
            auto_capability: None,
            auto_model: None,
        };
        let json = serde_json::to_string(&log).unwrap();
        assert!(!json.contains("tokens_in"));
        assert!(!json.contains("tokens_out"));
        assert!(!json.contains("tokens_per_second"));
        assert!(!json.contains("prompt_eval_ms"));
        assert!(!json.contains("eval_ms"));
        assert!(!json.contains("backend_type"));
    }

    #[test]
    fn cost_estimation_small_model() {
        // <=8B: $0.10/M in, $0.30/M out
        let cost = estimate_api_cost("llama3:8b", 1_000_000, 1_000_000);
        assert!((cost - 0.40).abs() < 0.001); // 0.10 + 0.30
    }

    #[test]
    fn cost_estimation_medium_model() {
        // 9-32B: $0.25/M in, $0.75/M out
        let cost = estimate_api_cost("qwen2:14b", 1_000_000, 1_000_000);
        assert!((cost - 1.0).abs() < 0.001); // 0.25 + 0.75
    }

    #[test]
    fn cost_estimation_large_model() {
        // 33B+: $0.50/M in, $1.50/M out
        let cost = estimate_api_cost("llama3:70b", 1_000_000, 1_000_000);
        assert!((cost - 2.0).abs() < 0.001); // 0.50 + 1.50
    }

    #[test]
    fn cost_estimation_unknown_model_defaults_to_medium() {
        let cost = estimate_api_cost("custom-model", 1_000_000, 1_000_000);
        assert!((cost - 1.0).abs() < 0.001); // middle tier: 0.25 + 0.75
    }

    #[test]
    fn extract_param_billions_various_formats() {
        assert_eq!(extract_param_billions("llama3:8b"), Some(8));
        assert_eq!(extract_param_billions("qwen2-72B"), Some(72));
        assert_eq!(extract_param_billions("phi3:3.8b"), Some(3));
        assert_eq!(extract_param_billions("gemma:2b"), Some(2));
        assert_eq!(extract_param_billions("custom-model"), None);
        assert_eq!(extract_param_billions("mistral"), None);
    }

    #[test]
    fn latency_percentiles_empty() {
        let p = compute_percentiles(&[]);
        assert_eq!(p.p50, 0);
        assert_eq!(p.p95, 0);
        assert_eq!(p.p99, 0);
    }

    #[test]
    fn latency_percentiles_known_values() {
        // 100 values: 1..=100
        let values: Vec<u64> = (1..=100).collect();
        let p = compute_percentiles(&values);
        assert_eq!(p.p50, 51); // index 50
        assert_eq!(p.p95, 96); // index 95
        assert_eq!(p.p99, 100); // index 99
    }

    #[test]
    fn token_aggregation_across_models() {
        // Simulate what get_stats would compute from multiple logs
        let logs = vec![
            RequestLog {
                timestamp: chrono::Utc::now().timestamp(),
                model: Some("llama3:8b".into()),
                backend: "gpu1".into(),
                duration_ms: 100,
                status: "success".into(),
                path: "/v1/chat/completions".into(),
                request_id: None,
                tier: None,
                classified_by: None,
                tokens_in: Some(100),
                tokens_out: Some(200),
                tokens_per_second: Some(40.0),
                prompt_eval_ms: None,
                eval_ms: None,
                backend_type: Some("ollama".into()),
                auto_tier: None,
                auto_capability: None,
                auto_model: None,
            },
            RequestLog {
                timestamp: chrono::Utc::now().timestamp(),
                model: Some("llama3:8b".into()),
                backend: "gpu1".into(),
                duration_ms: 150,
                status: "success".into(),
                path: "/v1/chat/completions".into(),
                request_id: None,
                tier: None,
                classified_by: None,
                tokens_in: Some(50),
                tokens_out: Some(100),
                tokens_per_second: Some(35.0),
                prompt_eval_ms: None,
                eval_ms: None,
                backend_type: Some("ollama".into()),
                auto_tier: None,
                auto_capability: None,
                auto_model: None,
            },
            RequestLog {
                timestamp: chrono::Utc::now().timestamp(),
                model: Some("qwen2:14b".into()),
                backend: "gpu2".into(),
                duration_ms: 300,
                status: "success".into(),
                path: "/v1/chat/completions".into(),
                request_id: None,
                tier: None,
                classified_by: None,
                tokens_in: Some(200),
                tokens_out: Some(400),
                tokens_per_second: Some(50.0),
                prompt_eval_ms: None,
                eval_ms: None,
                backend_type: Some("llama-server".into()),
                auto_tier: None,
                auto_capability: None,
                auto_model: None,
            },
        ];

        // Manually aggregate like get_stats does
        let mut total_in = 0u64;
        let mut total_out = 0u64;
        let mut model_tokens: HashMap<String, (u64, u64)> = HashMap::new();
        for log in &logs {
            if let Some(tin) = log.tokens_in {
                total_in += tin as u64;
                if let Some(m) = &log.model {
                    model_tokens.entry(m.clone()).or_insert((0, 0)).0 += tin as u64;
                }
            }
            if let Some(tout) = log.tokens_out {
                total_out += tout as u64;
                if let Some(m) = &log.model {
                    model_tokens.entry(m.clone()).or_insert((0, 0)).1 += tout as u64;
                }
            }
        }

        assert_eq!(total_in, 350);
        assert_eq!(total_out, 700);
        assert_eq!(model_tokens["llama3:8b"], (150, 300));
        assert_eq!(model_tokens["qwen2:14b"], (200, 400));
    }

    #[test]
    fn request_log_auto_fields_round_trip() {
        let log = RequestLog {
            timestamp: 1000,
            model: Some("qwen3:8b".into()),
            backend: "node1".into(),
            duration_ms: 500,
            status: "success".into(),
            path: "/v1/chat/completions".into(),
            request_id: None,
            tier: None,
            classified_by: None,
            tokens_in: None,
            tokens_out: None,
            tokens_per_second: None,
            prompt_eval_ms: None,
            eval_ms: None,
            backend_type: None,
            auto_tier: Some("standard".into()),
            auto_capability: Some("code".into()),
            auto_model: Some("qwen2.5-coder:32b".into()),
        };
        let json = serde_json::to_string(&log).unwrap();
        assert!(json.contains("auto_tier"));
        assert!(json.contains("standard"));
        let deser: RequestLog = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.auto_tier.as_deref(), Some("standard"));
        assert_eq!(deser.auto_capability.as_deref(), Some("code"));
        assert_eq!(deser.auto_model.as_deref(), Some("qwen2.5-coder:32b"));
    }

    #[test]
    fn request_log_without_auto_fields_backward_compat() {
        let json = r#"{"timestamp":1000,"model":null,"backend":"b1","duration_ms":100,"status":"success","path":"/test"}"#;
        let log: RequestLog = serde_json::from_str(json).unwrap();
        assert!(log.auto_tier.is_none());
        assert!(log.auto_capability.is_none());
        assert!(log.auto_model.is_none());
    }

    #[test]
    fn replace_file_overwrites_destination() {
        let base = std::env::temp_dir().join(format!("herd-analytics-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&base);
        let from = base.join("from.txt");
        let to = base.join("to.txt");

        std::fs::write(&from, "new").unwrap();
        std::fs::write(&to, "old").unwrap();

        replace_file(Path::new(&from), Path::new(&to)).unwrap();

        assert!(!from.exists());
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "new");

        let _ = std::fs::remove_file(&to);
        let _ = std::fs::remove_dir(&base);
    }
}
