//! Capability snapshot building for `herd agent`.
//!
//! GPU detection ports the probe order and tool invocations of
//! `scripts/herd-tune.sh` / `herd-tune.ps1` (nvidia → rocm → cpu) into Rust.
//! Parsing is kept in pure functions so it is unit-testable against canned
//! tool output. Multi-GPU hosts reduce to the FIRST GPU (CSV line 1) — the
//! same reduction herd-tune (`head -1`) and `backend/discovery.rs` use.

use crate::config::BackendType;
use crate::nodes::AgentCapabilities;

pub const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuVendor {
    Nvidia,
    Amd,
    None,
}

/// Static GPU facts probed once at agent startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuStatic {
    pub vendor: GpuVendor,
    pub model: Option<String>,
    pub vram_total_mb: u64,
    pub driver_version: Option<String>,
}

impl GpuStatic {
    fn cpu_only() -> Self {
        Self {
            vendor: GpuVendor::None,
            model: None,
            vram_total_mb: 0,
            driver_version: None,
        }
    }
}

fn run_tool(cmd: &str, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new(cmd).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Detect the GPU once at startup. Probe order matches herd-tune:
/// nvidia-smi, then rocm-smi, then fall back to cpu-only.
pub fn detect_gpu_static() -> GpuStatic {
    if let Some(out) = run_tool(
        "nvidia-smi",
        &[
            "--query-gpu=name,memory.total,driver_version",
            "--format=csv,noheader,nounits",
        ],
    ) {
        if let Some(gpu) = parse_nvidia_static_csv(&out) {
            return gpu;
        }
    }

    if let Some(out) = run_tool("rocm-smi", &["--showproductname", "--csv"]) {
        if let Some(model) = parse_rocm_product_csv(&out) {
            let vram_total_mb = run_tool("rocm-smi", &["--showmeminfo", "vram", "--csv"])
                .and_then(|o| parse_rocm_vram_csv(&o))
                .map(|(total, _used)| total)
                .unwrap_or(0);
            return GpuStatic {
                vendor: GpuVendor::Amd,
                model: Some(model),
                vram_total_mb,
                driver_version: None,
            };
        }
    }

    GpuStatic::cpu_only()
}

/// Probe current free VRAM. Called every heartbeat; returns `None` when the
/// probe tool is unavailable or its output is malformed (callers keep the
/// last known value).
pub fn probe_vram_free_mb(vendor: GpuVendor) -> Option<u64> {
    match vendor {
        GpuVendor::Nvidia => run_tool(
            "nvidia-smi",
            &["--query-gpu=memory.free", "--format=csv,noheader,nounits"],
        )
        .and_then(|out| parse_first_u64(&out)),
        GpuVendor::Amd => run_tool("rocm-smi", &["--showmeminfo", "vram", "--csv"])
            .and_then(|out| parse_rocm_vram_csv(&out))
            .map(|(total, used)| total.saturating_sub(used)),
        GpuVendor::None => None,
    }
}

/// Parse `nvidia-smi --query-gpu=name,memory.total,driver_version
/// --format=csv,noheader,nounits` output. First GPU wins on multi-GPU hosts.
pub fn parse_nvidia_static_csv(out: &str) -> Option<GpuStatic> {
    let line = out.lines().find(|l| !l.trim().is_empty())?;
    let mut fields = line.split(',').map(str::trim);
    let name = fields.next().filter(|s| !s.is_empty())?;
    let total_mb = fields.next()?.parse::<u64>().ok()?;
    let driver = fields.next().filter(|s| !s.is_empty());
    Some(GpuStatic {
        vendor: GpuVendor::Nvidia,
        model: Some(name.to_string()),
        vram_total_mb: total_mb,
        driver_version: driver.map(str::to_string),
    })
}

/// Parse `rocm-smi --showproductname --csv` output: a header line then
/// `device,Card series,...` rows. First GPU wins.
pub fn parse_rocm_product_csv(out: &str) -> Option<String> {
    let row = out
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .nth(1)?;
    let model = row
        .split(',')
        .nth(1)
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    Some(model.to_string())
}

/// Parse `rocm-smi --showmeminfo vram --csv` output into (total_mb, used_mb).
/// Values are reported in bytes; first GPU wins.
pub fn parse_rocm_vram_csv(out: &str) -> Option<(u64, u64)> {
    let row = out
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .nth(1)?;
    let mut fields = row.split(',').map(str::trim);
    let _device = fields.next()?;
    let total_bytes = fields.next()?.parse::<u64>().ok()?;
    let used_bytes = fields
        .next()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    Some((total_bytes / 1_048_576, used_bytes / 1_048_576))
}

fn parse_first_u64(out: &str) -> Option<u64> {
    out.lines()
        .find(|l| !l.trim().is_empty())?
        .split(',')
        .next()?
        .trim()
        .parse::<u64>()
        .ok()
}

/// Derive the default node id: lowercase hostname plus the GPU model-number
/// suffix when one exists (`citadel` + "NVIDIA GeForce RTX 5090" →
/// `citadel-5090`), sanitized to the charset the gateway accepts.
pub fn default_node_id(hostname: &str, gpu_model: Option<&str>) -> String {
    let host = sanitize_node_id(hostname);
    match gpu_model.and_then(model_number_suffix) {
        Some(suffix) => format!("{host}-{suffix}"),
        None => host,
    }
}

/// Extract a model-number suffix from a GPU name: the last whitespace token
/// that is all digits ("NVIDIA GeForce RTX 5090" → "5090", "Radeon RX 7900
/// XTX" → "7900"). Returns `None` when no such token exists.
fn model_number_suffix(model: &str) -> Option<String> {
    model
        .split_whitespace()
        .rev()
        .find(|t| t.len() >= 2 && t.chars().all(|c| c.is_ascii_digit()))
        .map(str::to_string)
}

/// Lowercase and restrict to the node_id charset the gateway validates
/// (ASCII alphanumeric plus '-', '_', '.', ':'); other characters become '-'.
fn sanitize_node_id(raw: &str) -> String {
    let sanitized: String = raw
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = sanitized.trim_matches('-');
    if trimmed.is_empty() {
        "node".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Builds `AgentCapabilities` snapshots from startup-probed static facts plus
/// per-beat dynamic state. Reuses the wire type from `nodes::registry`.
#[derive(Debug, Clone)]
pub struct SnapshotBuilder {
    node_id: String,
    gpu: GpuStatic,
}

impl SnapshotBuilder {
    pub fn new(node_id: String, gpu: GpuStatic) -> Self {
        Self { node_id, gpu }
    }

    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    #[allow(clippy::too_many_arguments)]
    pub fn snapshot(
        &self,
        backend: BackendType,
        address: String,
        vram_free_mb: u64,
        models_loaded: Vec<String>,
        queue_depth: Option<u32>,
        max_concurrent: Option<u32>,
        context_len: Option<u32>,
    ) -> AgentCapabilities {
        AgentCapabilities {
            node_id: self.node_id.clone(),
            backend,
            address,
            gpu_model: self.gpu.model.clone(),
            vram_total_mb: self.gpu.vram_total_mb,
            // The gateway rejects free > total; clamp so a racy probe between
            // model unloads can't invalidate the whole snapshot.
            vram_free_mb: vram_free_mb.min(self.gpu.vram_total_mb),
            models_loaded,
            // Measured live load (llama-server); None when unmeasurable.
            queue_depth,
            ttft_p50_ms: None, // deferred — needs --metrics + p50 windowing
            max_concurrent,
            // Context-window size from llama-server /props; None for Ollama
            // and openai-compat (dim 3 absent → neutral, never penalized).
            context_len,
            rpc_capable: false, // v1.4 (pipeline parallel)
            rpc_port: None,
            agent_version: AGENT_VERSION.to_string(),
            // Reported so the gateway can offer the right published binary
            // when this agent is behind the fleet target version (PR #6).
            os: Some(std::env::consts::OS.to_string()),
            arch: Some(std::env::consts::ARCH.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nvidia_single_gpu_csv() {
        let out = "NVIDIA GeForce RTX 5090, 32607, 572.83\n";
        let gpu = parse_nvidia_static_csv(out).unwrap();
        assert_eq!(gpu.vendor, GpuVendor::Nvidia);
        assert_eq!(gpu.model.as_deref(), Some("NVIDIA GeForce RTX 5090"));
        assert_eq!(gpu.vram_total_mb, 32607);
        assert_eq!(gpu.driver_version.as_deref(), Some("572.83"));
    }

    #[test]
    fn nvidia_multi_gpu_reduces_to_first_line() {
        let out =
            "NVIDIA GeForce RTX 5090, 32607, 572.83\nNVIDIA GeForce RTX 4080, 16376, 572.83\n";
        let gpu = parse_nvidia_static_csv(out).unwrap();
        assert_eq!(gpu.model.as_deref(), Some("NVIDIA GeForce RTX 5090"));
        assert_eq!(gpu.vram_total_mb, 32607);
    }

    #[test]
    fn nvidia_malformed_csv_returns_none() {
        assert!(parse_nvidia_static_csv("").is_none());
        assert!(parse_nvidia_static_csv("\n\n").is_none());
        assert!(parse_nvidia_static_csv("RTX 5090").is_none());
        assert!(parse_nvidia_static_csv("RTX 5090, not-a-number, 572.83").is_none());
    }

    #[test]
    fn parses_rocm_product_csv() {
        let out = "device,Card series,Card model,Card vendor,Card SKU\ncard0,Radeon RX 7900 XTX,0x744c,Advanced Micro Devices,EXT94393\n";
        assert_eq!(
            parse_rocm_product_csv(out).as_deref(),
            Some("Radeon RX 7900 XTX")
        );
    }

    #[test]
    fn rocm_product_csv_without_rows_returns_none() {
        assert!(parse_rocm_product_csv("device,Card series\n").is_none());
        assert!(parse_rocm_product_csv("").is_none());
    }

    #[test]
    fn parses_rocm_vram_csv_bytes_to_mb() {
        let out = "device,VRAM Total Memory (B),VRAM Total Used Memory (B)\ncard0,25753026560,305135616\n";
        let (total, used) = parse_rocm_vram_csv(out).unwrap();
        assert_eq!(total, 24560);
        assert_eq!(used, 291);
    }

    #[test]
    fn parses_first_u64_from_multi_gpu_output() {
        assert_eq!(parse_first_u64("30142\n15032\n"), Some(30142));
        assert_eq!(parse_first_u64("  29000  \n"), Some(29000));
        assert_eq!(parse_first_u64("garbage\n"), None);
        assert_eq!(parse_first_u64(""), None);
    }

    #[test]
    fn default_node_id_appends_gpu_suffix() {
        assert_eq!(
            default_node_id("CITADEL", Some("NVIDIA GeForce RTX 5090")),
            "citadel-5090"
        );
        assert_eq!(
            default_node_id("warden", Some("Radeon RX 7900 XTX")),
            "warden-7900"
        );
    }

    #[test]
    fn default_node_id_without_gpu_is_bare_hostname() {
        assert_eq!(default_node_id("minipc", None), "minipc");
        assert_eq!(
            default_node_id("box", Some("Some GPU Without Numbers")),
            "box"
        );
    }

    #[test]
    fn default_node_id_sanitizes_hostname() {
        assert_eq!(default_node_id("Tom's PC", None), "tom-s-pc");
        assert_eq!(default_node_id("", None), "node");
    }

    #[test]
    fn snapshot_clamps_free_vram_to_total() {
        let builder = SnapshotBuilder::new(
            "citadel-5090".into(),
            GpuStatic {
                vendor: GpuVendor::Nvidia,
                model: Some("NVIDIA GeForce RTX 5090".into()),
                vram_total_mb: 32607,
                driver_version: Some("572.83".into()),
            },
        );
        let caps = builder.snapshot(
            BackendType::LlamaServer,
            "http://127.0.0.1:8080".into(),
            99_999,
            vec!["qwen3-32b".into()],
            Some(2),
            Some(4),
            Some(32_768),
        );
        assert_eq!(caps.vram_free_mb, 32607);
        assert_eq!(caps.vram_total_mb, 32607);
        assert_eq!(caps.node_id, "citadel-5090");
        assert_eq!(caps.models_loaded, vec!["qwen3-32b"]);
        assert_eq!(caps.agent_version, AGENT_VERSION);
        assert_eq!(caps.queue_depth, Some(2));
        assert_eq!(caps.max_concurrent, Some(4));
        assert_eq!(caps.context_len, Some(32_768));
        assert!(!caps.rpc_capable);
        assert_eq!(caps.os.as_deref(), Some(std::env::consts::OS));
        assert_eq!(caps.arch.as_deref(), Some(std::env::consts::ARCH));
    }

    #[test]
    fn cpu_only_snapshot_is_valid() {
        let builder = SnapshotBuilder::new("minipc".into(), GpuStatic::cpu_only());
        let caps = builder.snapshot(
            BackendType::Ollama,
            "http://127.0.0.1:11434".into(),
            0,
            vec![],
            None,
            None,
            None,
        );
        assert_eq!(caps.vram_total_mb, 0);
        assert_eq!(caps.vram_free_mb, 0);
        assert!(caps.gpu_model.is_none());
        // Ollama can't report load or context window → honest None, not a fake 0.
        assert_eq!(caps.queue_depth, None);
        assert_eq!(caps.max_concurrent, None);
        assert_eq!(caps.context_len, None);
    }
}
