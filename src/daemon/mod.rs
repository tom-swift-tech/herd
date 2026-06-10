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
pub mod update;

use crate::config::{BackendType, RespawnMode};
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

    /// How to restart after a self-update: 'self' spawns the new binary then
    /// exits (bare terminal runs); 'supervised' exits only and lets the
    /// service manager (NSSM, systemd Restart=always) bring the new binary up.
    #[arg(long, default_value = "self", env = "HERD_RESPAWN_MODE")]
    pub respawn_mode: RespawnMode,
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
    let mut failed_offers = update::FailureMemo::new();
    let respawner = update::ProcessRespawner;

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

        let outcome = heartbeat.send(&caps, false).await;
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

        // Fleet self-update (PR #6b): act on a complete offer for a strictly
        // newer version, unless the same (version, sha) pair recently failed.
        // On success this never returns — the process restarts.
        if let client::BeatOutcome::Success {
            update_offer: Some(offer),
            ..
        } = &outcome
        {
            if update::should_apply(capabilities::AGENT_VERSION, offer, &failed_offers) {
                apply_update(
                    offer,
                    &caps,
                    &heartbeat,
                    &mut failed_offers,
                    &respawner,
                    args.respawn_mode,
                )
                .await;
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

/// Download, verify, and apply an update offer, then restart per
/// `respawn_mode`. On success this only returns if the respawn itself failed;
/// on any earlier failure it logs, memoizes the offer, and returns so the
/// loop keeps heartbeating on the current binary.
async fn apply_update(
    offer: &client::UpdateOffer,
    caps: &crate::nodes::AgentCapabilities,
    heartbeat: &client::HeartbeatClient,
    failed_offers: &mut update::FailureMemo,
    respawner: &dyn update::Respawner,
    respawn_mode: RespawnMode,
) {
    // Local case (no download_url): the agent constructs the URL from its own
    // --gateway address. A gateway-sent URL is only ever the explicit
    // external override (fleet.download_url_base).
    let url = offer
        .download_url
        .clone()
        .unwrap_or_else(|| heartbeat.binary_url(&offer.target_version));
    tracing::info!(
        "fleet update offered: {} -> {} from {}",
        capabilities::AGENT_VERSION,
        offer.target_version,
        url
    );

    let sha256 = offer.sha256.clone();
    let token = heartbeat.token().map(str::to_string);
    let download_url = url.clone();
    let result = tokio::task::spawn_blocking(move || {
        crate::updater::update_from_url(&download_url, &sha256, token.as_deref())
    })
    .await;

    match result {
        Ok(Ok(())) => {
            // Best-effort: announce the restart so the gateway grants an
            // eviction grace window. The restart proceeds even if this beat
            // fails — worst case the node briefly shows offline and
            // re-registers on its first beat after the restart.
            if let client::BeatOutcome::Rejected { .. } | client::BeatOutcome::Unreachable(_) =
                heartbeat.send(caps, true).await
            {
                tracing::warn!("final updating heartbeat failed; restarting anyway");
            }
            tracing::info!(
                "updated to {} — restarting ({:?})",
                offer.target_version,
                respawn_mode
            );
            if let Err(e) = respawner.restart(respawn_mode) {
                // The binary on disk is already the new version; keep serving
                // heartbeats on the old in-memory code rather than dying.
                tracing::error!("respawn failed after update: {e:#}");
            }
        }
        Ok(Err(e)) => {
            tracing::error!(
                "self-update to {} failed: {e:#} — will not retry this offer for a while",
                offer.target_version
            );
            failed_offers.record_failure(&offer.target_version, &offer.sha256);
        }
        Err(e) => {
            tracing::error!("self-update task panicked: {e}");
            failed_offers.record_failure(&offer.target_version, &offer.sha256);
        }
    }
}
