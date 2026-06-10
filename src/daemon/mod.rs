//! Node-side daemon for `herd agent` (v1.2 distributed-inference foundation).
//!
//! Runs on each GPU node, probes the local inference process, and heartbeats a
//! capability snapshot to the gateway's `POST /api/internal/nodes/heartbeat`
//! every 2s (configurable). Named `daemon` (not `agent`) to avoid colliding
//! with the existing `src/agent/` sessions module; the user-facing CLI term
//! remains `herd agent`.

pub mod capabilities;
pub mod client;
pub mod lifecycle;

use crate::config::BackendType;
use anyhow::Context;
use std::time::Duration;

/// Arguments for the `herd agent` subcommand.
#[derive(clap::Args, Debug, Clone)]
pub struct AgentArgs {
    /// Gateway base URL (e.g. http://herd.starbase:40114). Required — no
    /// auto-discovery in v1.2.
    #[arg(long)]
    pub gateway: String,

    /// Node identifier. Defaults to hostname-derived (`hostname-gpu`, e.g.
    /// citadel-5090).
    #[arg(long)]
    pub node_id: Option<String>,

    /// Heartbeat cadence in seconds
    #[arg(long, default_value = "2", env = "HERD_HEARTBEAT_SECS")]
    pub heartbeat_secs: u64,

    /// Local inference backend URL to probe and advertise
    #[arg(long, default_value = "http://127.0.0.1:11434")]
    pub backend_url: String,

    /// Address the gateway should use to reach this node's inference backend
    /// (defaults to --backend-url; set this when the backend listens on
    /// localhost but is reachable via a Tailscale hostname)
    #[arg(long)]
    pub advertise_url: Option<String>,

    /// Backend type override (ollama | llama-server | openai-compat).
    /// Auto-detected by probing when omitted.
    #[arg(long, value_parser = parse_backend_type)]
    pub backend: Option<BackendType>,
}

fn parse_backend_type(s: &str) -> Result<BackendType, String> {
    match s {
        "ollama" => Ok(BackendType::Ollama),
        "llama-server" => Ok(BackendType::LlamaServer),
        "openai-compat" => Ok(BackendType::OpenAICompat),
        other => Err(format!(
            "unknown backend type '{other}' (expected ollama, llama-server, or openai-compat)"
        )),
    }
}

/// Entry point for `herd agent`. Probes static node facts once (GPU model,
/// total VRAM, node identity), then loops: refresh dynamic state (free VRAM,
/// loaded models), POST the snapshot to the gateway, and sleep for the delay
/// the schedule state machine picks (steady cadence, exponential backoff while
/// the gateway is unreachable).
pub async fn run(args: AgentArgs) -> anyhow::Result<()> {
    let token = std::env::var("HERD_AGENT_TOKEN")
        .ok()
        .filter(|s| !s.is_empty());
    if token.is_none() {
        tracing::warn!(
            "HERD_AGENT_TOKEN not set — sending unauthenticated heartbeats \
             (the gateway rejects these unless HERD_ALLOW_UNAUTHENTICATED_AGENT_HEARTBEAT=true)"
        );
    }

    // Static facts: probed once at startup, not re-derived every beat.
    let gpu = tokio::task::spawn_blocking(capabilities::detect_gpu_static)
        .await
        .context("GPU detection task panicked")?;
    let hostname = hostname::get()
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "node".to_string());
    let node_id = args
        .node_id
        .clone()
        .unwrap_or_else(|| capabilities::default_node_id(&hostname, gpu.model.as_deref()));

    tracing::info!(
        "herd agent starting: node_id={} gateway={} gpu={} vram={}MB backend_url={}",
        node_id,
        args.gateway,
        gpu.model.as_deref().unwrap_or("none"),
        gpu.vram_total_mb,
        args.backend_url
    );

    let advertise_url = args
        .advertise_url
        .clone()
        .unwrap_or_else(|| args.backend_url.clone());
    let snapshotter = capabilities::SnapshotBuilder::new(node_id, gpu.clone());
    let probe = lifecycle::LocalProbe::new(args.backend_url.clone(), args.backend)
        .context("failed to build local backend probe")?;
    let heartbeat = client::HeartbeatClient::new(&args.gateway, token)
        .context("failed to build heartbeat client")?;
    let mut schedule =
        client::HeartbeatSchedule::new(Duration::from_secs(args.heartbeat_secs.max(1)));

    let mut last_vram_free_mb = gpu.vram_total_mb;
    let mut gateway_up = false;
    let mut backend_reachable = true;

    loop {
        let vendor = gpu.vendor;
        if let Ok(Some(free)) =
            tokio::task::spawn_blocking(move || capabilities::probe_vram_free_mb(vendor)).await
        {
            last_vram_free_mb = free;
        }

        let local = probe.probe().await;
        if local.reachable != backend_reachable {
            backend_reachable = local.reachable;
            if backend_reachable {
                tracing::info!(
                    "local backend reachable at {} ({} models loaded)",
                    args.backend_url,
                    local.models_loaded.len()
                );
            } else {
                tracing::warn!(
                    "local backend unreachable at {} — heartbeating with empty model list",
                    args.backend_url
                );
            }
        }

        let caps = snapshotter.snapshot(
            local.backend,
            advertise_url.clone(),
            last_vram_free_mb,
            local.models_loaded,
        );

        let outcome = heartbeat.send(&caps).await;
        match &outcome {
            client::BeatOutcome::Success { registered, .. } => {
                if *registered {
                    tracing::info!("registered with gateway {}", args.gateway);
                } else if !gateway_up {
                    tracing::info!("gateway {} reachable again", args.gateway);
                }
                gateway_up = true;
            }
            client::BeatOutcome::Rejected { status, body } => {
                gateway_up = false;
                tracing::error!("gateway rejected heartbeat: HTTP {} {}", status, body);
            }
            client::BeatOutcome::Unreachable(err) => {
                if gateway_up || schedule.consecutive_failures() == 0 {
                    tracing::warn!("gateway unreachable: {}", err);
                }
                gateway_up = false;
            }
        }

        let delay = schedule.record(&outcome);
        if !gateway_up && schedule.consecutive_failures() > 1 {
            tracing::debug!(
                "retrying heartbeat in {:?} ({} consecutive failures)",
                delay,
                schedule.consecutive_failures()
            );
        }
        tokio::time::sleep(delay).await;
    }
}
