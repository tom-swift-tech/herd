use crate::agent::session::Session;
use crate::agent::types::{AgentMessage, MessageRole, SessionStatus};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

#[derive(Clone)]
pub struct SessionStore {
    sessions: Arc<RwLock<HashMap<String, Session>>>,
    /// Per-session locks to prevent read-modify-write races on concurrent messages
    session_locks: Arc<RwLock<HashMap<String, Arc<Mutex<()>>>>>,
    max_sessions: usize,
    persist_dir: Option<PathBuf>,
}

impl SessionStore {
    pub fn new(max_sessions: usize) -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            session_locks: Arc::new(RwLock::new(HashMap::new())),
            max_sessions,
            persist_dir: None,
        }
    }

    /// Create a persistent store that saves sessions to disk.
    /// Loads any existing sessions from the directory on construction.
    pub fn persistent(max_sessions: usize, dir: PathBuf) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&dir)?;

        let mut sessions = HashMap::new();
        let mut locks = HashMap::new();
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                match std::fs::read_to_string(&path) {
                    Ok(content) => match serde_json::from_str::<Session>(&content) {
                        Ok(session) => {
                            tracing::info!(
                                "Restored session {} (model: {})",
                                session.id,
                                session.model
                            );
                            locks.insert(session.id.clone(), Arc::new(Mutex::new(())));
                            sessions.insert(session.id.clone(), session);
                        }
                        Err(e) => {
                            tracing::warn!("Skipping corrupt session file {:?}: {}", path, e);
                        }
                    },
                    Err(e) => {
                        tracing::warn!("Failed to read session file {:?}: {}", path, e);
                    }
                }
            }
        }

        let count = sessions.len();
        if count > 0 {
            tracing::info!("Loaded {} sessions from disk", count);
        }

        Ok(Self {
            sessions: Arc::new(RwLock::new(sessions)),
            session_locks: Arc::new(RwLock::new(locks)),
            max_sessions,
            persist_dir: Some(dir),
        })
    }

    /// Acquire a per-session lock for exclusive mutation.
    /// Returns None if the session doesn't exist.
    pub async fn lock_session(&self, id: &str) -> Option<SessionLockGuard> {
        let locks = self.session_locks.read().await;
        let lock = locks.get(id)?.clone();
        drop(locks);
        let guard = lock.lock_owned().await;
        Some(SessionLockGuard { _guard: guard })
    }

    pub async fn create(
        &self,
        model: String,
        system_prompt: Option<String>,
    ) -> Result<Session, String> {
        let mut sessions = self.sessions.write().await;
        if sessions.len() >= self.max_sessions {
            return Err(format!("Maximum sessions ({}) reached", self.max_sessions));
        }

        let now = chrono::Utc::now().timestamp();
        let id = uuid::Uuid::new_v4().to_string();

        let mut messages = Vec::new();
        if let Some(prompt) = system_prompt {
            messages.push(AgentMessage {
                role: MessageRole::System,
                content: prompt,
                tool_calls: None,
                tool_call_id: None,
            });
        }

        let session = Session {
            id: id.clone(),
            model,
            messages,
            status: SessionStatus::Active,
            created_at: now,
            updated_at: now,
        };

        sessions.insert(id.clone(), session.clone());
        drop(sessions);

        // Create lock for this session
        {
            let mut locks = self.session_locks.write().await;
            locks.insert(id, Arc::new(Mutex::new(())));
        }

        self.persist_session(&session);
        Ok(session)
    }

    pub async fn get(&self, id: &str) -> Option<Session> {
        let sessions = self.sessions.read().await;
        sessions.get(id).cloned()
    }

    pub async fn list(&self) -> Vec<Session> {
        let sessions = self.sessions.read().await;
        let mut list: Vec<Session> = sessions.values().cloned().collect();
        list.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        list
    }

    pub async fn delete(&self, id: &str) -> bool {
        let removed = {
            let mut sessions = self.sessions.write().await;
            sessions.remove(id).is_some()
        };
        if removed {
            // Remove the per-session lock
            let mut locks = self.session_locks.write().await;
            locks.remove(id);
            drop(locks);
            self.remove_persisted(id);
        }
        removed
    }

    pub async fn update(&self, session: Session) {
        {
            let mut sessions = self.sessions.write().await;
            sessions.insert(session.id.clone(), session.clone());
        }
        self.persist_session(&session);
    }

    /// Remove sessions whose `updated_at` is older than `max_age_secs` seconds ago.
    /// Returns the number of sessions removed.
    pub async fn reap_expired(&self, max_age_secs: i64) -> usize {
        let cutoff = chrono::Utc::now().timestamp() - max_age_secs;
        let expired_ids: Vec<String> = {
            let sessions = self.sessions.read().await;
            sessions
                .iter()
                .filter(|(_, s)| s.updated_at < cutoff)
                .map(|(id, _)| id.clone())
                .collect()
        };
        let count = expired_ids.len();
        if count > 0 {
            let mut sessions = self.sessions.write().await;
            let mut locks = self.session_locks.write().await;
            for id in &expired_ids {
                sessions.remove(id);
                locks.remove(id);
            }
            drop(sessions);
            drop(locks);
            for id in &expired_ids {
                self.remove_persisted(id);
            }
        }
        count
    }

    fn is_safe_id(id: &str) -> bool {
        !id.is_empty()
            && !id.contains('/')
            && !id.contains('\\')
            && !id.contains("..")
            && id.len() < 128
    }

    /// Write a session to disk (fire-and-forget, logs errors).
    fn persist_session(&self, session: &Session) {
        if let Some(dir) = &self.persist_dir {
            if !Self::is_safe_id(&session.id) {
                tracing::error!("Refusing to persist session with unsafe ID: {}", session.id);
                return;
            }
            let path = dir.join(format!("{}.json", session.id));
            match serde_json::to_string_pretty(session) {
                Ok(json) => {
                    if let Err(e) = std::fs::write(&path, json) {
                        tracing::error!("Failed to persist session {}: {}", session.id, e);
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to serialize session {}: {}", session.id, e);
                }
            }
        }
    }

    /// Remove a session file from disk.
    fn remove_persisted(&self, id: &str) {
        if let Some(dir) = &self.persist_dir {
            if !Self::is_safe_id(id) {
                return;
            }
            let path = dir.join(format!("{}.json", id));
            if path.exists() {
                if let Err(e) = std::fs::remove_file(&path) {
                    tracing::error!("Failed to remove session file {}: {}", id, e);
                }
            }
        }
    }
}

/// RAII guard for per-session locks. Hold this while mutating a session
/// to prevent concurrent read-modify-write races.
pub struct SessionLockGuard {
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn create_session_basic() {
        let store = SessionStore::new(10);
        let session = store.create("llama3:8b".into(), None).await.unwrap();

        assert_eq!(session.model, "llama3:8b");
        assert_eq!(session.status, SessionStatus::Active);
        assert!(session.messages.is_empty());
        assert!(!session.id.is_empty());
    }

    #[tokio::test]
    async fn create_session_with_system_prompt() {
        let store = SessionStore::new(10);
        let session = store
            .create("llama3:8b".into(), Some("You are helpful.".into()))
            .await
            .unwrap();

        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].role, MessageRole::System);
        assert_eq!(session.messages[0].content, "You are helpful.");
    }

    #[tokio::test]
    async fn get_returns_created_session() {
        let store = SessionStore::new(10);
        let created = store.create("qwen2.5:7b".into(), None).await.unwrap();

        let fetched = store.get(&created.id).await;
        assert!(fetched.is_some());
        assert_eq!(fetched.unwrap().id, created.id);
    }

    #[tokio::test]
    async fn get_returns_none_for_missing() {
        let store = SessionStore::new(10);
        assert!(store.get("nonexistent").await.is_none());
    }

    #[tokio::test]
    async fn list_returns_all_sessions() {
        let store = SessionStore::new(10);
        store.create("model-a".into(), None).await.unwrap();
        store.create("model-b".into(), None).await.unwrap();
        store.create("model-c".into(), None).await.unwrap();

        let list = store.list().await;
        assert_eq!(list.len(), 3);
    }

    #[tokio::test]
    async fn list_empty_store() {
        let store = SessionStore::new(10);
        assert!(store.list().await.is_empty());
    }

    #[tokio::test]
    async fn delete_existing_session() {
        let store = SessionStore::new(10);
        let session = store.create("llama3:8b".into(), None).await.unwrap();

        assert!(store.delete(&session.id).await);
        assert!(store.get(&session.id).await.is_none());
    }

    #[tokio::test]
    async fn delete_nonexistent_returns_false() {
        let store = SessionStore::new(10);
        assert!(!store.delete("nonexistent").await);
    }

    #[tokio::test]
    async fn update_modifies_session() {
        let store = SessionStore::new(10);
        let mut session = store.create("llama3:8b".into(), None).await.unwrap();

        session.messages.push(AgentMessage {
            role: MessageRole::User,
            content: "Hello".into(),
            tool_calls: None,
            tool_call_id: None,
        });
        session.status = SessionStatus::Processing;
        store.update(session.clone()).await;

        let fetched = store.get(&session.id).await.unwrap();
        assert_eq!(fetched.messages.len(), 1);
        assert_eq!(fetched.status, SessionStatus::Processing);
    }

    #[tokio::test]
    async fn max_sessions_enforced() {
        let store = SessionStore::new(2);
        store.create("model-a".into(), None).await.unwrap();
        store.create("model-b".into(), None).await.unwrap();

        let result = store.create("model-c".into(), None).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Maximum sessions"));
    }

    #[tokio::test]
    async fn delete_frees_slot_for_new_session() {
        let store = SessionStore::new(1);
        let session = store.create("model-a".into(), None).await.unwrap();

        store.delete(&session.id).await;
        let result = store.create("model-b".into(), None).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn session_ids_are_unique() {
        let store = SessionStore::new(100);
        let mut ids = std::collections::HashSet::new();
        for _ in 0..50 {
            let session = store.create("model".into(), None).await.unwrap();
            assert!(ids.insert(session.id));
        }
    }

    #[tokio::test]
    async fn session_lock_prevents_concurrent_access() {
        let store = SessionStore::new(10);
        let session = store.create("model".into(), None).await.unwrap();

        // Should be able to acquire lock
        let guard = store.lock_session(&session.id).await;
        assert!(guard.is_some());
        drop(guard);

        // Nonexistent session returns None
        assert!(store.lock_session("nonexistent").await.is_none());
    }

    #[tokio::test]
    async fn persistent_store_roundtrip() {
        let dir = std::env::temp_dir().join(format!("herd-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        // Create and populate
        {
            let store = SessionStore::persistent(10, dir.clone()).unwrap();
            let session = store
                .create("llama3:8b".into(), Some("test".into()))
                .await
                .unwrap();
            assert!(dir.join(format!("{}.json", session.id)).exists());
        }

        // Reload from disk
        {
            let store = SessionStore::persistent(10, dir.clone()).unwrap();
            let list = store.list().await;
            assert_eq!(list.len(), 1);
            assert_eq!(list[0].model, "llama3:8b");
        }

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn persistent_delete_removes_file() {
        let dir = std::env::temp_dir().join(format!("herd-test-{}", uuid::Uuid::new_v4()));
        let store = SessionStore::persistent(10, dir.clone()).unwrap();

        let session = store.create("model".into(), None).await.unwrap();
        let file_path = dir.join(format!("{}.json", session.id));
        assert!(file_path.exists());

        store.delete(&session.id).await;
        assert!(!file_path.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
