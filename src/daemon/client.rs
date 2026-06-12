//! Heartbeat client for `herd agent`.
//!
//! Split into a synchronous schedule state machine (`HeartbeatSchedule`) and a
//! thin async HTTP driver (`HeartbeatClient`). All time-dependent logic lives
//! in the state machine behind the injectable `Clock` pattern established in
//! PR #2 (`nodes/registry.rs`), so retry/backoff is unit-testable without
//! real sleeps; only the driver loop in `daemon::run` actually sleeps.

use crate::nodes::AgentCapabilities;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(crate) type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// Cap on the exponential backoff while the gateway is unreachable.
pub const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Cap on the doubling exponent so the multiplication can't overflow.
const MAX_BACKOFF_EXPONENT: u32 = 16;
/// Truncation limit for error bodies echoed into logs.
const MAX_ERROR_BODY_LEN: usize = 200;

/// A complete, actionable update offer from the gateway: present only when
/// the reply carried both a target version and the sha256 of a published
/// binary for this platform. A target without a sha is advertising only —
/// nothing is downloadable, so no offer is surfaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateOffer {
    pub target_version: String,
    /// Explicit external download location (the gateway's
    /// fleet.download_url_base). Absent in the default local case — presence
    /// ⇔ external override. When absent the agent constructs the URL from
    /// its own --gateway address ([`HeartbeatClient::binary_url`]); it never
    /// fetches a URL the gateway derived from the request's Host header.
    pub download_url: Option<String>,
    /// Hex sha256 the downloaded binary must hash to before any swap.
    pub sha256: String,
}

/// Result of one heartbeat attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BeatOutcome {
    /// Gateway accepted the heartbeat.
    Success {
        registered: bool,
        /// Cadence the gateway asked for (`next_heartbeat_secs`), if present.
        next_heartbeat_secs: Option<u64>,
        /// Self-update offer, when the gateway has a downloadable binary for
        /// a version it wants this fleet on.
        update_offer: Option<UpdateOffer>,
    },
    /// Gateway answered with a non-2xx status (bad token, invalid payload,
    /// registry full, ...).
    Rejected { status: u16, body: String },
    /// Gateway could not be reached at all.
    Unreachable(String),
}

/// Decides how long to wait before the next heartbeat: the steady cadence on
/// success (honoring the gateway's `next_heartbeat_secs` when it sends one),
/// exponential backoff (base, 2×base, 4×base, ... capped at [`MAX_BACKOFF`])
/// on failure, reset on the next success.
pub struct HeartbeatSchedule {
    base: Duration,
    max_backoff: Duration,
    consecutive_failures: u32,
    last_success: Option<Instant>,
    clock: Clock,
}

impl HeartbeatSchedule {
    pub fn new(base: Duration) -> Self {
        Self::with_clock(base, Arc::new(Instant::now))
    }

    pub(crate) fn with_clock(base: Duration, clock: Clock) -> Self {
        Self {
            base,
            max_backoff: MAX_BACKOFF,
            consecutive_failures: 0,
            last_success: None,
            clock,
        }
    }

    /// Record an outcome and return the delay before the next attempt.
    pub fn record(&mut self, outcome: &BeatOutcome) -> Duration {
        match outcome {
            BeatOutcome::Success {
                next_heartbeat_secs,
                ..
            } => {
                self.consecutive_failures = 0;
                self.last_success = Some((self.clock)());
                next_heartbeat_secs
                    .filter(|s| *s > 0)
                    .map(Duration::from_secs)
                    .unwrap_or(self.base)
            }
            BeatOutcome::Rejected { .. } | BeatOutcome::Unreachable(_) => {
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                let exponent = (self.consecutive_failures - 1).min(MAX_BACKOFF_EXPONENT);
                self.base
                    .saturating_mul(1u32 << exponent)
                    .min(self.max_backoff)
            }
        }
    }

    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }

    /// How long ago the last successful heartbeat was, if any.
    pub fn time_since_success(&self) -> Option<Duration> {
        self.last_success
            .map(|at| (self.clock)().duration_since(at))
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Serialize)]
struct HeartbeatRequestBody<'a> {
    capabilities: &'a AgentCapabilities,
    /// Wall-clock timestamp. The v1.2 gateway accepts but ignores this (its
    /// registry runs on a monotonic clock); sent for the wire format and
    /// future skew diagnostics.
    timestamp: String,
    /// True only on the final beat before a self-update restart, so the
    /// gateway grants an eviction grace window. Omitted (not `false`) on
    /// normal beats to keep the steady-state wire format unchanged.
    #[serde(skip_serializing_if = "is_false")]
    updating: bool,
}

/// Lenient view of the gateway's heartbeat response. `deployments_assigned`
/// is deliberately not modeled — it is always `[]` in v1.2 and nothing here
/// may depend on it.
#[derive(Debug, Default, Deserialize)]
struct HeartbeatReply {
    #[serde(default)]
    registered: bool,
    #[serde(default)]
    next_heartbeat_secs: Option<u64>,
    /// Version the gateway wants agents on (fleet version authority, PR #6a).
    #[serde(default)]
    target_version: Option<String>,
    /// External download override — present only when the gateway has
    /// fleet.download_url_base configured.
    #[serde(default)]
    download_url: Option<String>,
    /// Hex sha256 of the published binary for this agent's platform; present
    /// iff one is published for `target_version`.
    #[serde(default)]
    sha256: Option<String>,
}

impl HeartbeatReply {
    fn into_update_offer(self) -> Option<UpdateOffer> {
        match (self.target_version, self.sha256) {
            (Some(target_version), Some(sha256)) => Some(UpdateOffer {
                target_version,
                download_url: self.download_url,
                sha256,
            }),
            _ => None,
        }
    }
}

/// Thin HTTP driver that POSTs capability snapshots to the gateway.
pub struct HeartbeatClient {
    http: reqwest::Client,
    /// Gateway base URL as given on the command line, without trailing slash.
    base: String,
    endpoint: String,
    token: Option<String>,
}

impl HeartbeatClient {
    /// `token` is the shared bearer from `HERD_AGENT_TOKEN`: when `None`, no
    /// Authorization header is sent at all.
    pub fn new(gateway: &str, token: Option<String>) -> anyhow::Result<Self> {
        let url = reqwest::Url::parse(gateway)?;
        if !matches!(url.scheme(), "http" | "https") {
            anyhow::bail!("gateway URL must use http or https: {gateway}");
        }
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()?;
        let base = gateway.trim_end_matches('/').to_string();
        Ok(Self {
            http,
            endpoint: format!("{base}/api/internal/nodes/heartbeat"),
            base,
            token: token.filter(|t| !t.is_empty()),
        })
    }

    /// The agent's bearer token, for authenticated binary downloads.
    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    /// Gateway binary-serving URL for `version` on this host's platform.
    /// Used when a heartbeat reply carries no download_url (the local case):
    /// the agent trusts only its own --gateway address, never a URL the
    /// gateway derived from request headers.
    pub fn binary_url(&self, version: &str) -> String {
        Self::binary_url_for(
            &self.base,
            version,
            std::env::consts::OS,
            std::env::consts::ARCH,
        )
    }

    fn binary_url_for(base: &str, version: &str, os: &str, arch: &str) -> String {
        format!("{base}/api/internal/nodes/binary/{version}/{os}-{arch}")
    }

    pub async fn send(&self, caps: &AgentCapabilities, updating: bool) -> BeatOutcome {
        let body = HeartbeatRequestBody {
            capabilities: caps,
            timestamp: chrono::Utc::now().to_rfc3339(),
            updating,
        };
        let mut request = self.http.post(&self.endpoint).json(&body);
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }

        let response = match request.send().await {
            Ok(r) => r,
            Err(e) => return BeatOutcome::Unreachable(e.to_string()),
        };

        let status = response.status();
        if status.is_success() {
            let reply: HeartbeatReply = response.json().await.unwrap_or_default();
            BeatOutcome::Success {
                registered: reply.registered,
                next_heartbeat_secs: reply.next_heartbeat_secs,
                update_offer: reply.into_update_offer(),
            }
        } else {
            let body = response
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(MAX_ERROR_BODY_LEN)
                .collect();
            BeatOutcome::Rejected {
                status: status.as_u16(),
                body,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Same manual-clock pattern as `nodes/registry.rs` (PR #2).
    #[derive(Clone)]
    struct TestClock {
        now: Arc<Mutex<Instant>>,
    }

    impl TestClock {
        fn new() -> Self {
            Self {
                now: Arc::new(Mutex::new(Instant::now())),
            }
        }

        fn advance(&self, delta: Duration) {
            let mut guard = self.now.lock().unwrap();
            *guard += delta;
        }

        fn as_fn(&self) -> Clock {
            let now = self.now.clone();
            Arc::new(move || *now.lock().unwrap())
        }
    }

    fn schedule() -> (HeartbeatSchedule, TestClock) {
        let clock = TestClock::new();
        let sched = HeartbeatSchedule::with_clock(Duration::from_secs(2), clock.as_fn());
        (sched, clock)
    }

    fn success(next: Option<u64>) -> BeatOutcome {
        BeatOutcome::Success {
            registered: false,
            next_heartbeat_secs: next,
            update_offer: None,
        }
    }

    fn unreachable() -> BeatOutcome {
        BeatOutcome::Unreachable("connection refused".into())
    }

    #[test]
    fn success_returns_base_cadence() {
        let (mut sched, _clock) = schedule();
        assert_eq!(sched.record(&success(None)), Duration::from_secs(2));
    }

    #[test]
    fn success_honors_server_cadence() {
        let (mut sched, _clock) = schedule();
        assert_eq!(sched.record(&success(Some(7))), Duration::from_secs(7));
        // A zero from the server is nonsense — fall back to base.
        assert_eq!(sched.record(&success(Some(0))), Duration::from_secs(2));
    }

    #[test]
    fn unreachable_backs_off_exponentially_capped_at_30s() {
        let (mut sched, _clock) = schedule();
        let delays: Vec<u64> = (0..6)
            .map(|_| sched.record(&unreachable()).as_secs())
            .collect();
        assert_eq!(delays, vec![2, 4, 8, 16, 30, 30]);
        assert_eq!(sched.consecutive_failures(), 6);
    }

    #[test]
    fn rejected_backs_off_like_unreachable() {
        let (mut sched, _clock) = schedule();
        let rejected = BeatOutcome::Rejected {
            status: 401,
            body: "bad token".into(),
        };
        assert_eq!(sched.record(&rejected), Duration::from_secs(2));
        assert_eq!(sched.record(&rejected), Duration::from_secs(4));
    }

    #[test]
    fn success_resets_backoff() {
        let (mut sched, _clock) = schedule();
        for _ in 0..4 {
            sched.record(&unreachable());
        }
        assert_eq!(sched.record(&success(None)), Duration::from_secs(2));
        assert_eq!(sched.consecutive_failures(), 0);
        // Backoff restarts from the base after a success.
        assert_eq!(sched.record(&unreachable()), Duration::from_secs(2));
        assert_eq!(sched.record(&unreachable()), Duration::from_secs(4));
    }

    #[test]
    fn deep_failure_count_does_not_overflow() {
        let (mut sched, _clock) = schedule();
        let mut last = Duration::ZERO;
        for _ in 0..100 {
            last = sched.record(&unreachable());
        }
        assert_eq!(last, MAX_BACKOFF);
    }

    #[test]
    fn time_since_success_tracks_the_clock() {
        let (mut sched, clock) = schedule();
        assert!(sched.time_since_success().is_none());
        sched.record(&success(None));
        clock.advance(Duration::from_secs(12));
        assert_eq!(sched.time_since_success(), Some(Duration::from_secs(12)));
    }

    #[test]
    fn client_rejects_non_http_gateway() {
        assert!(HeartbeatClient::new("ftp://gw:21", None).is_err());
        assert!(HeartbeatClient::new("not a url", None).is_err());
    }

    #[test]
    fn client_builds_endpoint_without_double_slash() {
        let client = HeartbeatClient::new("http://gw:40114/", Some("tok".into())).unwrap();
        assert_eq!(
            client.endpoint,
            "http://gw:40114/api/internal/nodes/heartbeat"
        );
        assert_eq!(client.token.as_deref(), Some("tok"));
    }

    #[test]
    fn empty_token_is_treated_as_unset() {
        let client = HeartbeatClient::new("http://gw:40114", Some(String::new())).unwrap();
        assert!(client.token.is_none());
    }

    #[test]
    fn heartbeat_reply_tolerates_unknown_and_missing_fields() {
        let reply: HeartbeatReply = serde_json::from_str(
            r#"{"registered":true,"deployments_assigned":[],"next_heartbeat_secs":2,"future_field":1}"#,
        )
        .unwrap();
        assert!(reply.registered);
        assert_eq!(reply.next_heartbeat_secs, Some(2));

        let bare: HeartbeatReply = serde_json::from_str("{}").unwrap();
        assert!(!bare.registered);
        assert!(bare.next_heartbeat_secs.is_none());
    }

    // ---- self-update offer plumbing (PR #6b) ----

    #[test]
    fn reply_with_target_and_sha_yields_offer_without_url() {
        // The local case: gateway sends no download_url; the agent will
        // construct it from its own --gateway address.
        let reply: HeartbeatReply = serde_json::from_str(
            r#"{"registered":false,"target_version":"1.3.0","sha256":"abc123"}"#,
        )
        .unwrap();
        let offer = reply.into_update_offer().unwrap();
        assert_eq!(offer.target_version, "1.3.0");
        assert_eq!(offer.sha256, "abc123");
        assert!(offer.download_url.is_none());
    }

    #[test]
    fn reply_with_external_url_yields_offer_with_url() {
        let reply: HeartbeatReply = serde_json::from_str(
            r#"{"target_version":"1.3.0","download_url":"https://cdn.example/herd/1.3.0/linux-x86_64/herd","sha256":"abc123"}"#,
        )
        .unwrap();
        let offer = reply.into_update_offer().unwrap();
        assert_eq!(
            offer.download_url.as_deref(),
            Some("https://cdn.example/herd/1.3.0/linux-x86_64/herd")
        );
    }

    #[test]
    fn reply_without_sha_yields_no_offer() {
        // Target advertised but nothing published — advertising only.
        let reply: HeartbeatReply = serde_json::from_str(r#"{"target_version":"1.3.0"}"#).unwrap();
        assert!(reply.into_update_offer().is_none());

        let bare: HeartbeatReply = serde_json::from_str("{}").unwrap();
        assert!(bare.into_update_offer().is_none());
    }

    fn request_body_json(updating: bool) -> serde_json::Value {
        let caps = AgentCapabilities {
            node_id: "a".to_string(),
            backend: crate::config::BackendType::LlamaServer,
            address: "http://127.0.0.1:8080".to_string(),
            gpu_model: None,
            vram_total_mb: 1,
            vram_free_mb: 1,
            models_loaded: vec![],
            queue_depth: 0,
            ttft_p50_ms: None,
            rpc_capable: false,
            rpc_port: None,
            agent_version: "1.2.0".to_string(),
            os: None,
            arch: None,
        };
        serde_json::to_value(HeartbeatRequestBody {
            capabilities: &caps,
            timestamp: "2026-06-10T00:00:00Z".to_string(),
            updating,
        })
        .unwrap()
    }

    #[test]
    fn updating_flag_is_omitted_on_normal_beats_and_sent_when_true() {
        let normal = request_body_json(false);
        assert!(
            normal.get("updating").is_none(),
            "steady-state wire format must not grow a field"
        );
        let last = request_body_json(true);
        assert_eq!(last.get("updating"), Some(&serde_json::json!(true)));
    }

    #[test]
    fn binary_url_is_built_from_the_agents_own_gateway_address() {
        assert_eq!(
            HeartbeatClient::binary_url_for("http://gw:40114", "1.3.0", "windows", "x86_64"),
            "http://gw:40114/api/internal/nodes/binary/1.3.0/windows-x86_64"
        );
        let client = HeartbeatClient::new("http://gw:40114/", Some("tok".into())).unwrap();
        assert!(client
            .binary_url("1.3.0")
            .starts_with("http://gw:40114/api/internal/nodes/binary/1.3.0/"));
        assert_eq!(client.token(), Some("tok"));
    }
}
