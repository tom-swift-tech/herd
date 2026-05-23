use crate::config::RateLimitConfig;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

// ---------------------------------------------------------------------------
// Token bucket
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct TokenBucket {
    tokens: AtomicU64,
    max_tokens: u64,
    refill_interval: Duration,
    last_refill: Mutex<Instant>,
}

impl TokenBucket {
    fn new(max_tokens: u64) -> Self {
        Self {
            tokens: AtomicU64::new(max_tokens),
            max_tokens,
            refill_interval: Duration::from_secs(1),
            last_refill: Mutex::new(Instant::now()),
        }
    }

    fn refill_if_due(&self) {
        let mut last_refill = match self.last_refill.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if last_refill.elapsed() >= self.refill_interval {
            self.tokens.store(self.max_tokens, Ordering::Relaxed);
            *last_refill = Instant::now();
        }
    }

    fn try_acquire(&self) -> bool {
        self.refill_if_due();
        loop {
            let current = self.tokens.load(Ordering::Relaxed);
            if current == 0 {
                return false;
            }
            if self
                .tokens
                .compare_exchange_weak(current, current - 1, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return true;
            }
        }
    }

    fn remaining(&self) -> u64 {
        self.refill_if_due();
        self.tokens.load(Ordering::Relaxed)
    }

    fn limit(&self) -> u64 {
        self.max_tokens
    }
}

// ---------------------------------------------------------------------------
// Rate limit check result
// ---------------------------------------------------------------------------

/// Information about rate limit state, returned on both allowed and denied requests.
#[derive(Debug, Clone)]
pub struct RateLimitInfo {
    /// The client's rate limit (requests/sec). 0 = unlimited.
    pub limit: u64,
    /// Tokens remaining in the bucket
    pub remaining: u64,
    /// Unix timestamp (seconds) when bucket next refills
    pub reset_at: u64,
}

// ---------------------------------------------------------------------------
// Client entry — either rate-limited or unlimited
// ---------------------------------------------------------------------------

enum ClientEntry {
    Limited(Arc<TokenBucket>),
    Unlimited,
}

// ---------------------------------------------------------------------------
// Per-client rate limiter
// ---------------------------------------------------------------------------

pub struct RateLimiter {
    global_bucket: Option<Arc<TokenBucket>>,
    /// Map from API key -> client entry
    client_entries: Arc<RwLock<HashMap<String, ClientEntry>>>,
}

impl RateLimiter {
    /// Creates a new rate limiter from config.
    pub fn new(config: &RateLimitConfig) -> Self {
        let global_bucket = if config.global > 0 {
            Some(Arc::new(TokenBucket::new(config.global)))
        } else {
            None
        };

        let mut client_map: HashMap<String, ClientEntry> = HashMap::new();
        for client in &config.clients {
            if client.rate_limit > 0 {
                client_map.insert(
                    client.api_key.clone(),
                    ClientEntry::Limited(Arc::new(TokenBucket::new(client.rate_limit))),
                );
            } else {
                client_map.insert(client.api_key.clone(), ClientEntry::Unlimited);
            }
        }

        let client_entries = Arc::new(RwLock::new(client_map));

        Self {
            global_bucket,
            client_entries,
        }
    }

    /// Check rate limit for a request.
    ///
    /// - Known client with rate_limit > 0: uses that client's bucket.
    /// - Known client with rate_limit == 0: always allowed (unlimited).
    /// - Unknown key or no key: uses the global bucket.
    /// - Global == 0: always allowed (unlimited).
    ///
    /// Returns `Ok(RateLimitInfo)` if allowed, `Err(RateLimitInfo)` if rate limited.
    pub async fn check_rate_limit(
        &self,
        api_key: Option<&str>,
    ) -> Result<RateLimitInfo, RateLimitInfo> {
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let reset_at = now_secs + 1; // buckets refill every second

        // Check if there's a client-specific entry
        if let Some(key) = api_key {
            let clients = self.client_entries.read().await;
            if let Some(entry) = clients.get(key) {
                match entry {
                    ClientEntry::Unlimited => {
                        return Ok(RateLimitInfo {
                            limit: 0,
                            remaining: 0,
                            reset_at,
                        });
                    }
                    ClientEntry::Limited(bucket) => {
                        if bucket.try_acquire() {
                            return Ok(RateLimitInfo {
                                limit: bucket.limit(),
                                remaining: bucket.remaining(),
                                reset_at,
                            });
                        } else {
                            return Err(RateLimitInfo {
                                limit: bucket.limit(),
                                remaining: 0,
                                reset_at,
                            });
                        }
                    }
                }
            }
        }

        // Fall back to global bucket
        match &self.global_bucket {
            Some(bucket) => {
                if bucket.try_acquire() {
                    Ok(RateLimitInfo {
                        limit: bucket.limit(),
                        remaining: bucket.remaining(),
                        reset_at,
                    })
                } else {
                    Err(RateLimitInfo {
                        limit: bucket.limit(),
                        remaining: 0,
                        reset_at,
                    })
                }
            }
            None => {
                // No global limit — unlimited
                Ok(RateLimitInfo {
                    limit: 0,
                    remaining: 0,
                    reset_at,
                })
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ClientRateLimit, RateLimitConfig};

    fn make_config(global: u64, clients: Vec<ClientRateLimit>) -> RateLimitConfig {
        RateLimitConfig { global, clients }
    }

    #[test]
    fn constructing_rate_limiter_does_not_require_tokio_runtime() {
        let config = make_config(1, vec![]);
        let limiter = RateLimiter::new(&config);

        assert_eq!(limiter.global_bucket.as_ref().unwrap().limit(), 1);
    }

    #[tokio::test]
    async fn global_bucket_allows_up_to_limit() {
        let config = make_config(3, vec![]);
        let limiter = RateLimiter::new(&config);

        assert!(limiter.check_rate_limit(None).await.is_ok());
        assert!(limiter.check_rate_limit(None).await.is_ok());
        assert!(limiter.check_rate_limit(None).await.is_ok());
        assert!(limiter.check_rate_limit(None).await.is_err());
    }

    #[tokio::test]
    async fn global_zero_means_unlimited() {
        let config = make_config(0, vec![]);
        let limiter = RateLimiter::new(&config);

        for _ in 0..100 {
            assert!(limiter.check_rate_limit(None).await.is_ok());
        }
    }

    #[tokio::test]
    async fn client_bucket_independent_from_global() {
        let config = make_config(
            2,
            vec![ClientRateLimit {
                api_key: "sk-client-1".into(),
                rate_limit: 5,
                name: Some("client-1".into()),
            }],
        );
        let limiter = RateLimiter::new(&config);

        // Client gets 5 requests
        for _ in 0..5 {
            assert!(limiter.check_rate_limit(Some("sk-client-1")).await.is_ok());
        }
        assert!(limiter.check_rate_limit(Some("sk-client-1")).await.is_err());

        // Global still has its own 2
        assert!(limiter.check_rate_limit(None).await.is_ok());
        assert!(limiter.check_rate_limit(None).await.is_ok());
        assert!(limiter.check_rate_limit(None).await.is_err());
    }

    #[tokio::test]
    async fn unknown_key_falls_back_to_global() {
        let config = make_config(2, vec![]);
        let limiter = RateLimiter::new(&config);

        assert!(limiter.check_rate_limit(Some("sk-unknown")).await.is_ok());
        assert!(limiter.check_rate_limit(Some("sk-unknown")).await.is_ok());
        assert!(limiter.check_rate_limit(Some("sk-unknown")).await.is_err());
    }

    #[tokio::test]
    async fn multiple_clients_get_independent_buckets() {
        let config = make_config(
            0,
            vec![
                ClientRateLimit {
                    api_key: "sk-a".into(),
                    rate_limit: 2,
                    name: Some("alpha".into()),
                },
                ClientRateLimit {
                    api_key: "sk-b".into(),
                    rate_limit: 3,
                    name: Some("beta".into()),
                },
            ],
        );
        let limiter = RateLimiter::new(&config);

        // Client A gets 2
        assert!(limiter.check_rate_limit(Some("sk-a")).await.is_ok());
        assert!(limiter.check_rate_limit(Some("sk-a")).await.is_ok());
        assert!(limiter.check_rate_limit(Some("sk-a")).await.is_err());

        // Client B still gets 3
        assert!(limiter.check_rate_limit(Some("sk-b")).await.is_ok());
        assert!(limiter.check_rate_limit(Some("sk-b")).await.is_ok());
        assert!(limiter.check_rate_limit(Some("sk-b")).await.is_ok());
        assert!(limiter.check_rate_limit(Some("sk-b")).await.is_err());
    }

    #[tokio::test]
    async fn client_with_zero_rate_limit_is_unlimited() {
        let config = make_config(
            1,
            vec![ClientRateLimit {
                api_key: "sk-unlimited".into(),
                rate_limit: 0,
                name: Some("vip".into()),
            }],
        );
        let limiter = RateLimiter::new(&config);

        // Client with rate_limit=0 should be truly unlimited, bypassing global
        for _ in 0..100 {
            assert!(limiter.check_rate_limit(Some("sk-unlimited")).await.is_ok());
        }

        // Global still has its own limit of 1
        assert!(limiter.check_rate_limit(None).await.is_ok());
        assert!(limiter.check_rate_limit(None).await.is_err());
    }

    #[tokio::test]
    async fn rate_limit_info_has_correct_fields() {
        let config = make_config(10, vec![]);
        let limiter = RateLimiter::new(&config);

        let info = limiter.check_rate_limit(None).await.unwrap();
        assert_eq!(info.limit, 10);
        assert_eq!(info.remaining, 9);
        assert!(info.reset_at > 0);
    }

    #[tokio::test]
    async fn rate_limit_exceeded_info() {
        let config = make_config(1, vec![]);
        let limiter = RateLimiter::new(&config);

        let _ = limiter.check_rate_limit(None).await;
        let err = limiter.check_rate_limit(None).await.unwrap_err();
        assert_eq!(err.limit, 1);
        assert_eq!(err.remaining, 0);
    }

    #[tokio::test]
    async fn token_bucket_refills_after_interval() {
        let config = make_config(2, vec![]);
        let limiter = RateLimiter::new(&config);

        assert!(limiter.check_rate_limit(None).await.is_ok());
        assert!(limiter.check_rate_limit(None).await.is_ok());
        assert!(limiter.check_rate_limit(None).await.is_err());

        // Wait for refill
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

        assert!(limiter.check_rate_limit(None).await.is_ok());
    }
}
