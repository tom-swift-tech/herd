/// Session→backend affinity store for Phase-4 scoring dim 18 (`session_stickiness`).
///
/// Records which backend served a session's most recent turn so the scorer can
/// prefer it on the next turn (prompt / KV-cache warmth). Writes happen
/// **off-path** (post-request hook); reads happen **on-path** inside
/// `ScoredRouter::route_scored` — one lookup per request, only when the request
/// carries a session id (the `X-Herd-Session` header).
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Soft ceiling on tracked sessions. Session ids come from an untrusted client
/// header, so the map is bounded: at the ceiling the least-recently-used entry
/// is evicted before inserting a new one — never a silent drop of the new write.
/// An evicted session simply loses its affinity hint (dim 18 goes neutral).
pub const MAX_SESSIONS_TRACKED: usize = 50_000;

/// Shared, in-memory session→backend affinity map.
///
/// `BTreeMap` for deterministic iteration/eviction (house rule). Keyed by the
/// owned session-id string.
#[derive(Clone, Debug, Default)]
pub struct SessionAffinity {
    inner: Arc<RwLock<Inner>>,
}

#[derive(Debug, Default)]
struct Inner {
    map: BTreeMap<String, Entry>,
    /// Global monotonic tick, bumped once per `record()`. Each entry records the
    /// tick at its last write; the lowest-tick entry is the LRU eviction victim.
    tick: u64,
}

#[derive(Clone, Debug)]
struct Entry {
    backend: String,
    last_tick: u64,
}

impl SessionAffinity {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `session_id`'s most recent turn was served by `backend`.
    ///
    /// Called on the **post-request hook** — off the scoring path. No-op for an
    /// empty session id. Evicts the LRU session when the ceiling is reached.
    pub async fn record(&self, session_id: &str, backend: &str) {
        if session_id.is_empty() {
            return;
        }
        let mut guard = self.inner.write().await;
        let tick = guard.tick + 1;
        guard.tick = tick;

        // Evict the LRU session when a NEW session would exceed the ceiling
        // (re-recording an existing session is free).
        if !guard.map.contains_key(session_id) && guard.map.len() >= MAX_SESSIONS_TRACKED {
            let lru = guard
                .map
                .iter()
                .min_by_key(|(_, e)| e.last_tick)
                .map(|(k, _)| k.clone());
            if let Some(k) = lru {
                guard.map.remove(&k);
            }
        }

        guard.map.insert(
            session_id.to_string(),
            Entry {
                backend: backend.to_string(),
                last_tick: tick,
            },
        );
    }

    /// The backend that served this session's last turn, if known.
    ///
    /// Called once per request on the scoring path when a session id is present.
    /// `None` for an empty/unknown session id → dim 18 is absent (neutral).
    pub async fn get(&self, session_id: &str) -> Option<String> {
        if session_id.is_empty() {
            return None;
        }
        self.inner
            .read()
            .await
            .map
            .get(session_id)
            .map(|e| e.backend.clone())
    }

    /// Number of tracked sessions. Primarily for tests.
    pub async fn len(&self) -> usize {
        self.inner.read().await.map.len()
    }

    /// True if no sessions are tracked yet.
    pub async fn is_empty(&self) -> bool {
        self.inner.read().await.map.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn record_then_get_returns_backend() {
        let aff = SessionAffinity::new();
        assert!(aff.is_empty().await);

        aff.record("sess-1", "gpu1").await;
        assert_eq!(aff.get("sess-1").await.as_deref(), Some("gpu1"));
        assert_eq!(aff.len().await, 1);

        // Re-recording updates the backend (latest turn wins).
        aff.record("sess-1", "gpu2").await;
        assert_eq!(aff.get("sess-1").await.as_deref(), Some("gpu2"));
        assert_eq!(aff.len().await, 1, "re-record updates in place");
    }

    #[tokio::test]
    async fn unknown_and_empty_session_return_none() {
        let aff = SessionAffinity::new();
        assert_eq!(aff.get("never-seen").await, None);

        // Empty session id is a no-op on both write and read.
        aff.record("", "gpu1").await;
        assert!(aff.is_empty().await);
        assert_eq!(aff.get("").await, None);
    }

    #[tokio::test]
    async fn lru_eviction_at_ceiling() {
        let aff = SessionAffinity::new();

        // The first session recorded is the LRU victim once we exceed the cap.
        aff.record("oldest", "gpu1").await;
        for i in 0..MAX_SESSIONS_TRACKED {
            aff.record(&format!("s{i}"), "gpu1").await;
        }

        assert_eq!(aff.len().await, MAX_SESSIONS_TRACKED, "capped at ceiling");
        assert_eq!(
            aff.get("oldest").await,
            None,
            "least-recently-used session evicted"
        );

        // Re-recording an existing session must not grow the map.
        aff.record("s0", "gpu2").await;
        assert_eq!(aff.len().await, MAX_SESSIONS_TRACKED, "re-record no growth");
    }
}
