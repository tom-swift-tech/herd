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

/// Result of one heartbeat attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BeatOutcome {
    /// Gateway accepted the heartbeat.
    Success {
        registered: bool,
        /// Cadence the gateway asked for (`next_heartbeat_secs`), if present.
        next_heartbeat_secs: Option<u64>,
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

#[derive(Serialize)]
struct HeartbeatRequestBody<'a> {
    capabilities: &'a AgentCapabilities,
    /// Wall-clock timestamp. The v1.2 gateway accepts but ignores this (its
    /// registry runs on a monotonic clock); sent for the wire format and
    /// future skew diagnostics.
    timestamp: String,
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
}

/// Thin HTTP driver that POSTs capability snapshots to the gateway.
pub struct HeartbeatClient {
    http: reqwest::Client,
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
        Ok(Self {
            http,
            endpoint: format!(
                "{}/api/internal/nodes/heartbeat",
                gateway.trim_end_matches('/')
            ),
            token: token.filter(|t| !t.is_empty()),
        })
    }

    pub async fn send(&self, caps: &AgentCapabilities) -> BeatOutcome {
        let body = HeartbeatRequestBody {
            capabilities: caps,
            timestamp: chrono::Utc::now().to_rfc3339(),
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
}
