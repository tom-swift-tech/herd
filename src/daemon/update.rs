//! Agent-side self-update decisions for `herd agent` (v1.2 PR #6b).
//!
//! Pure decision logic only — whether to act on a gateway update offer, and
//! how to bring the new binary up after the swap. The actual
//! download/verify/replace lives in `crate::updater::update_from_url`; the
//! driver loop in `daemon::run` wires the two together. Time-dependent state
//! uses the injectable `Clock` pattern shared with `client.rs`.

use super::client::{Clock, UpdateOffer};
use crate::config::RespawnMode;
use anyhow::Context;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long a failed (version, sha256) offer is suppressed before the agent
/// will try it again. A bad offer is re-advertised on every ~2s beat; without
/// the memo that would hammer the download endpoint ~150x per window.
pub const FAILED_OFFER_BACKOFF: Duration = Duration::from_secs(300);

/// Remembers offers that failed download or verification, keyed by
/// (target_version, sha256). The same pair is retried at most once per
/// [`FAILED_OFFER_BACKOFF`]; a *different* pair (republished binary, new
/// sha) is always eligible immediately.
pub struct FailureMemo {
    failed: HashMap<(String, String), Instant>,
    backoff: Duration,
    clock: Clock,
}

impl FailureMemo {
    pub fn new() -> Self {
        Self::with_clock(FAILED_OFFER_BACKOFF, Arc::new(Instant::now))
    }

    pub(crate) fn with_clock(backoff: Duration, clock: Clock) -> Self {
        Self {
            failed: HashMap::new(),
            backoff,
            clock,
        }
    }

    /// True when this (version, sha) pair has no failure recorded inside the
    /// backoff window.
    pub fn should_attempt(&self, version: &str, sha256: &str) -> bool {
        let now = (self.clock)();
        self.failed
            .get(&(version.to_string(), sha256.to_string()))
            .is_none_or(|failed_at| now.duration_since(*failed_at) > self.backoff)
    }

    /// Record a failed attempt and drop entries old enough to be retryable
    /// anyway, so a long-lived agent's memo stays bounded.
    pub fn record_failure(&mut self, version: &str, sha256: &str) {
        let now = (self.clock)();
        let backoff = self.backoff;
        self.failed
            .retain(|_, failed_at| now.duration_since(*failed_at) <= backoff);
        self.failed
            .insert((version.to_string(), sha256.to_string()), now);
    }
}

impl Default for FailureMemo {
    fn default() -> Self {
        Self::new()
    }
}

/// Pure decision: act on an offer iff it targets a strictly newer version
/// than the running agent (same or older is steady state, not an error) and
/// the pair isn't memoized as recently failed.
pub fn should_apply(current_version: &str, offer: &UpdateOffer, memo: &FailureMemo) -> bool {
    crate::updater::version_is_newer(current_version, &offer.target_version)
        && memo.should_attempt(&offer.target_version, &offer.sha256)
}

/// Brings the new binary up after a successful swap. Behind a trait so the
/// run loop's post-update path is testable — the real implementation
/// terminates the process and never returns `Ok`.
pub trait Respawner: Send + Sync {
    fn restart(&self, mode: RespawnMode) -> anyhow::Result<()>;
}

/// The real thing: `self` spawns the replaced executable with this process's
/// argv and env then exits; `supervised` just exits and lets the service
/// manager (NSSM, systemd Restart=always) start the new binary — spawning
/// here too would double-run the agent.
pub struct ProcessRespawner;

impl Respawner for ProcessRespawner {
    fn restart(&self, mode: RespawnMode) -> anyhow::Result<()> {
        match mode {
            RespawnMode::SelfSpawn => {
                // current_exe() is the original path, which now holds the new
                // binary (self_replace moved the old one aside).
                let exe = std::env::current_exe()
                    .context("cannot resolve current executable for respawn")?;
                std::process::Command::new(exe)
                    .args(std::env::args_os().skip(1))
                    .spawn()
                    .context("failed to spawn the updated agent")?;
                tracing::info!("spawned updated agent; exiting old process");
            }
            RespawnMode::Supervised => {
                tracing::info!("exiting for supervisor to restart the updated agent");
            }
        }
        std::process::exit(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Same manual-clock pattern as `client.rs` / `nodes/registry.rs`.
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

    fn memo() -> (FailureMemo, TestClock) {
        let clock = TestClock::new();
        let memo = FailureMemo::with_clock(Duration::from_secs(300), clock.as_fn());
        (memo, clock)
    }

    fn offer(version: &str, sha: &str) -> UpdateOffer {
        UpdateOffer {
            target_version: version.to_string(),
            download_url: None,
            sha256: sha.to_string(),
        }
    }

    #[test]
    fn applies_only_strictly_newer_versions() {
        let (memo, _clock) = memo();
        assert!(should_apply("1.2.0", &offer("1.3.0", "s"), &memo));
        assert!(!should_apply("1.2.0", &offer("1.2.0", "s"), &memo));
        assert!(!should_apply("1.2.0", &offer("1.1.0", "s"), &memo));
    }

    #[test]
    fn failed_offer_is_suppressed_within_backoff_window() {
        let (mut memo, clock) = memo();
        let o = offer("1.3.0", "sha-a");
        assert!(should_apply("1.2.0", &o, &memo));

        memo.record_failure(&o.target_version, &o.sha256);
        assert!(!should_apply("1.2.0", &o, &memo));

        clock.advance(Duration::from_secs(299));
        assert!(!should_apply("1.2.0", &o, &memo), "still inside the window");

        clock.advance(Duration::from_secs(2));
        assert!(
            should_apply("1.2.0", &o, &memo),
            "window expired — one retry"
        );
    }

    #[test]
    fn republished_binary_with_new_sha_is_not_suppressed() {
        let (mut memo, _clock) = memo();
        memo.record_failure("1.3.0", "sha-bad");
        assert!(!should_apply("1.2.0", &offer("1.3.0", "sha-bad"), &memo));
        // Operator republished the binary — different sha, same version.
        assert!(should_apply("1.2.0", &offer("1.3.0", "sha-fixed"), &memo));
    }

    #[test]
    fn memo_prunes_expired_entries_on_record() {
        let (mut memo, clock) = memo();
        memo.record_failure("1.3.0", "a");
        memo.record_failure("1.3.1", "b");
        clock.advance(Duration::from_secs(301));
        memo.record_failure("1.4.0", "c");
        assert_eq!(memo.failed.len(), 1, "expired entries must be pruned");
    }

    /// Test double proving the run loop can be exercised without exiting the
    /// test process — records the mode instead of restarting.
    struct RecordingRespawner {
        restarted_with: Mutex<Option<RespawnMode>>,
    }

    impl Respawner for RecordingRespawner {
        fn restart(&self, mode: RespawnMode) -> anyhow::Result<()> {
            *self.restarted_with.lock().unwrap() = Some(mode);
            Ok(())
        }
    }

    #[test]
    fn respawner_trait_is_mockable_for_loop_tests() {
        let respawner = RecordingRespawner {
            restarted_with: Mutex::new(None),
        };
        respawner.restart(RespawnMode::Supervised).unwrap();
        assert_eq!(
            *respawner.restarted_with.lock().unwrap(),
            Some(RespawnMode::Supervised)
        );
    }
}
