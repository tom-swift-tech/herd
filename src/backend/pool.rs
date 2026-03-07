use crate::config::Backend;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct BackendState {
    pub config: Backend,
    pub healthy: bool,
    pub models: Vec<String>,
    pub current_model: Option<String>,
    pub gpu_metrics: Option<GpuMetrics>,
    pub failure_count: u32,
    pub last_check: Instant,
    pub last_request: Instant,
}

#[derive(Debug, Clone)]
pub struct GpuMetrics {
    pub utilization: f32,
    pub memory_used: u64,
    pub memory_total: u64,
    pub temperature: f32,
}

#[derive(Clone)]
pub struct BackendPool {
    pub backends: Arc<RwLock<Vec<BackendState>>>,
    failure_threshold: u32,
    recovery_time: Duration,
}

impl BackendPool {
    pub fn new(backends: Vec<Backend>, failure_threshold: u32, recovery_time: Duration) -> Self {
        let now = Instant::now();
        let states = backends
            .into_iter()
            .map(|config| BackendState {
                config,
                healthy: true,
                models: Vec::new(),
                current_model: None,
                gpu_metrics: None,
                failure_count: 0,
                last_check: now,
                last_request: now,
            })
            .collect();

        Self {
            backends: Arc::new(RwLock::new(states)),
            failure_threshold: failure_threshold.max(1),
            recovery_time,
        }
    }

    pub async fn all_healthy(&self) -> Vec<String> {
        let backends = self.backends.read().await;
        backends
            .iter()
            .filter(|b| b.healthy)
            .map(|b| b.config.name.clone())
            .collect()
    }

    pub async fn all(&self) -> Vec<String> {
        let backends = self.backends.read().await;
        backends.iter().map(|b| b.config.name.clone()).collect()
    }

    pub async fn get(&self, name: &str) -> Option<BackendState> {
        let backends = self.backends.read().await;
        backends
            .iter()
            .find(|b| b.config.name == name)
            .cloned()
    }

    pub async fn get_healthy(&self, name: &str) -> Option<BackendState> {
        let backends = self.backends.read().await;
        backends
            .iter()
            .find(|b| b.config.name == name && b.healthy)
            .cloned()
    }

    pub async fn get_by_priority(&self) -> Option<BackendState> {
        let backends = self.backends.read().await;
        backends
            .iter()
            .filter(|b| b.healthy)
            .max_by_key(|b| b.config.priority)
            .cloned()
    }

    pub async fn get_by_model(&self, model: &str) -> Option<BackendState> {
        let backends = self.backends.read().await;
        backends
            .iter()
            .filter(|b| b.healthy && b.models.contains(&model.to_string()))
            .max_by_key(|b| b.config.priority)
            .cloned()
    }

    pub async fn get_least_busy(&self) -> Option<BackendState> {
        let backends = self.backends.read().await;
        backends
            .iter()
            .filter(|b| b.healthy)
            .min_by(|a, b| {
                let a_busy = a.gpu_metrics.as_ref().map(|g| g.utilization).unwrap_or(0.0);
                let b_busy = b.gpu_metrics.as_ref().map(|g| g.utilization).unwrap_or(0.0);
                a_busy.partial_cmp(&b_busy).unwrap_or(std::cmp::Ordering::Equal)
            })
            .cloned()
    }

    pub async fn mark_healthy(&self, name: &str) {
        let mut backends = self.backends.write().await;
        if let Some(backend) = backends.iter_mut().find(|b| b.config.name == name) {
            backend.healthy = true;
            backend.failure_count = 0;
            backend.last_check = Instant::now();
        }
    }

    pub async fn mark_unhealthy(&self, name: &str) {
        let mut backends = self.backends.write().await;
        if let Some(backend) = backends.iter_mut().find(|b| b.config.name == name) {
            backend.failure_count += 1;
            if backend.failure_count >= self.failure_threshold {
                backend.healthy = false;
            }
            backend.last_check = Instant::now();
        }
    }

    pub fn recovery_time(&self) -> Duration {
        self.recovery_time
    }

    pub async fn update_models(&self, name: &str, models: Vec<String>) {
        let mut backends = self.backends.write().await;
        if let Some(backend) = backends.iter_mut().find(|b| b.config.name == name) {
            backend.models = models;
        }
    }

    pub async fn update_current_model(&self, name: &str, model: Option<String>) {
        let mut backends = self.backends.write().await;
        if let Some(backend) = backends.iter_mut().find(|b| b.config.name == name) {
            backend.current_model = model;
        }
    }

    pub async fn update_gpu_metrics(&self, name: &str, metrics: GpuMetrics) {
        let mut backends = self.backends.write().await;
        if let Some(backend) = backends.iter_mut().find(|b| b.config.name == name) {
            backend.gpu_metrics = Some(metrics);
        }
    }

    pub async fn touch_request(&self, name: &str) {
        let mut backends = self.backends.write().await;
        if let Some(backend) = backends.iter_mut().find(|b| b.config.name == name) {
            backend.last_request = Instant::now();
        }
    }

    pub async fn add(&self, backend: Backend) {
        let mut backends = self.backends.write().await;
        backends.push(BackendState {
            config: backend,
            healthy: true,
            models: Vec::new(),
            current_model: None,
            gpu_metrics: None,
            failure_count: 0,
            last_check: Instant::now(),
            last_request: Instant::now(),
        });
    }

    pub async fn update(&self, state: BackendState) {
        let mut backends = self.backends.write().await;
        if let Some(backend) = backends.iter_mut().find(|b| b.config.name == state.config.name) {
            *backend = state;
        }
    }

    pub async fn remove(&self, name: &str) -> bool {
        let mut backends = self.backends.write().await;
        let len_before = backends.len();
        backends.retain(|b| b.config.name != name);
        backends.len() < len_before
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Backend;
    use std::time::Duration;

    fn make_backend(name: &str, priority: u32) -> Backend {
        Backend {
            name: name.into(),
            url: "http://localhost:11434".into(),
            priority,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn add_and_remove_backend() {
        let pool = BackendPool::new(vec![], 3, Duration::from_secs(60));

        // Pool starts empty
        assert!(pool.all().await.is_empty());

        // Add a backend
        pool.add(make_backend("gpu1", 100)).await;
        let all = pool.all().await;
        assert_eq!(all.len(), 1);
        assert_eq!(all[0], "gpu1");

        // Remove it
        let removed = pool.remove("gpu1").await;
        assert!(removed);
        assert!(pool.all().await.is_empty());

        // Removing non-existent returns false
        let removed = pool.remove("gpu1").await;
        assert!(!removed);
    }

    #[tokio::test]
    async fn mark_healthy_unhealthy() {
        let pool = BackendPool::new(
            vec![make_backend("gpu1", 100)],
            3,
            Duration::from_secs(60),
        );

        // Initially healthy with failure_count 0
        let state = pool.get("gpu1").await.unwrap();
        assert!(state.healthy);
        assert_eq!(state.failure_count, 0);

        // One mark_unhealthy increments failure_count but keeps healthy (threshold=3)
        pool.mark_unhealthy("gpu1").await;
        let state = pool.get("gpu1").await.unwrap();
        assert_eq!(state.failure_count, 1);
        assert!(state.healthy);

        // mark_healthy resets failure_count and keeps healthy
        pool.mark_healthy("gpu1").await;
        let state = pool.get("gpu1").await.unwrap();
        assert_eq!(state.failure_count, 0);
        assert!(state.healthy);
    }

    #[tokio::test]
    async fn circuit_breaker_threshold() {
        let pool = BackendPool::new(
            vec![make_backend("gpu1", 100)],
            3,
            Duration::from_secs(60),
        );

        // Below threshold: still healthy
        pool.mark_unhealthy("gpu1").await;
        pool.mark_unhealthy("gpu1").await;
        let state = pool.get("gpu1").await.unwrap();
        assert_eq!(state.failure_count, 2);
        assert!(state.healthy);

        // At threshold: becomes unhealthy
        pool.mark_unhealthy("gpu1").await;
        let state = pool.get("gpu1").await.unwrap();
        assert_eq!(state.failure_count, 3);
        assert!(!state.healthy);
    }

    #[tokio::test]
    async fn all_healthy_filters() {
        let pool = BackendPool::new(
            vec![make_backend("gpu1", 100), make_backend("gpu2", 50)],
            1, // threshold of 1: single failure marks unhealthy
            Duration::from_secs(60),
        );

        // Both healthy initially
        let healthy = pool.all_healthy().await;
        assert_eq!(healthy.len(), 2);

        // Mark gpu1 unhealthy (threshold=1, so one call is enough)
        pool.mark_unhealthy("gpu1").await;

        let healthy = pool.all_healthy().await;
        assert_eq!(healthy.len(), 1);
        assert_eq!(healthy[0], "gpu2");

        // all() still returns both
        assert_eq!(pool.all().await.len(), 2);
    }

    #[tokio::test]
    async fn get_by_model_filters() {
        let pool = BackendPool::new(
            vec![make_backend("gpu1", 100), make_backend("gpu2", 50)],
            3,
            Duration::from_secs(60),
        );

        // No models loaded: get_by_model returns None
        assert!(pool.get_by_model("llama3").await.is_none());

        // Load model on gpu2 only
        pool.update_models("gpu2", vec!["llama3".into()]).await;

        let result = pool.get_by_model("llama3").await;
        assert!(result.is_some());
        assert_eq!(result.unwrap().config.name, "gpu2");

        // Load model on gpu1 as well; should prefer gpu1 (higher priority)
        pool.update_models("gpu1", vec!["llama3".into()]).await;

        let result = pool.get_by_model("llama3").await;
        assert!(result.is_some());
        assert_eq!(result.unwrap().config.name, "gpu1");
    }
}