use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;

/// In-memory Prometheus-compatible metrics.
#[derive(Clone)]
pub struct Metrics {
    /// Total requests by status ("success" / "error")
    pub requests_total: Arc<RwLock<HashMap<String, AtomicU64>>>,
    /// Total requests by backend name
    pub requests_by_backend: Arc<RwLock<HashMap<String, AtomicU64>>>,
    /// Latency histogram buckets (in milliseconds)
    /// Buckets: 10, 50, 100, 250, 500, 1000, 2500, 5000, 10000, +Inf
    pub latency_buckets: Arc<RwLock<LatencyHistogram>>,
    /// Routing selections by "{backend}|{strategy}"
    pub routing_selections: Arc<RwLock<HashMap<String, AtomicU64>>>,
    /// Token counts by "{direction}|{model}" where direction is "in" or "out"
    pub tokens_total: Arc<RwLock<HashMap<String, AtomicU64>>>,
    /// Tokens per second exponential moving average.
    pub tokens_per_second_ema: Arc<RwLock<f32>>,
    /// Labeled latency histogram by "{backend}|{model}|{status}"
    pub labeled_latency: Arc<RwLock<HashMap<String, LatencyHistogram>>>,
    /// Auto classification counts by tier|capability
    pub auto_classifications: Arc<RwLock<HashMap<String, AtomicU64>>>,
    /// Sum of classification durations in ms
    pub auto_classification_duration_sum: Arc<AtomicU64>,
    /// Count of classification calls
    pub auto_classification_duration_count: Arc<AtomicU64>,
    /// Cache hit count
    pub auto_cache_hits: Arc<AtomicU64>,
}

pub struct LatencyHistogram {
    pub bucket_bounds: Vec<u64>,       // upper bounds in ms
    pub bucket_counts: Vec<AtomicU64>, // count of observations <= bound
    pub sum: AtomicU64,                // total sum of observations in ms
    pub count: AtomicU64,              // total count of observations
}

impl Default for LatencyHistogram {
    fn default() -> Self {
        Self::new()
    }
}

impl LatencyHistogram {
    pub fn new() -> Self {
        let bounds = vec![10, 50, 100, 250, 500, 1000, 2500, 5000, 10000];
        let counts: Vec<AtomicU64> = bounds.iter().map(|_| AtomicU64::new(0)).collect();
        Self {
            bucket_bounds: bounds,
            bucket_counts: counts,
            sum: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }

    pub fn observe(&self, value_ms: u64) {
        self.sum.fetch_add(value_ms, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        // Increment only the first matching bucket (render accumulates for Prometheus)
        for (i, bound) in self.bucket_bounds.iter().enumerate() {
            if value_ms <= *bound {
                self.bucket_counts[i].fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
        // value exceeds all bucket bounds — counted in +Inf via self.count
    }

    /// Render histogram buckets in Prometheus format with optional labels.
    pub fn render_with_labels(&self, metric_name: &str, labels: &str) -> String {
        let mut out = String::new();
        let mut cumulative = 0u64;
        for (i, bound) in self.bucket_bounds.iter().enumerate() {
            cumulative += self.bucket_counts[i].load(Ordering::Relaxed);
            if labels.is_empty() {
                out.push_str(&format!(
                    "{}_bucket{{le=\"{}\"}} {}\n",
                    metric_name, bound, cumulative
                ));
            } else {
                out.push_str(&format!(
                    "{}_bucket{{{},le=\"{}\"}} {}\n",
                    metric_name, labels, bound, cumulative
                ));
            }
        }
        let total = self.count.load(Ordering::Relaxed);
        if labels.is_empty() {
            out.push_str(&format!(
                "{}_bucket{{le=\"+Inf\"}} {}\n",
                metric_name, total
            ));
            out.push_str(&format!(
                "{}_sum {}\n",
                metric_name,
                self.sum.load(Ordering::Relaxed)
            ));
            out.push_str(&format!("{}_count {}\n", metric_name, total));
        } else {
            out.push_str(&format!(
                "{}_bucket{{{},le=\"+Inf\"}} {}\n",
                metric_name, labels, total
            ));
            out.push_str(&format!(
                "{}_sum{{{}}} {}\n",
                metric_name,
                labels,
                self.sum.load(Ordering::Relaxed)
            ));
            out.push_str(&format!("{}_count{{{}}} {}\n", metric_name, labels, total));
        }
        out
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(
            "# HELP herd_request_duration_ms Request duration histogram in milliseconds\n",
        );
        out.push_str("# TYPE herd_request_duration_ms histogram\n");
        out.push_str(&self.render_with_labels("herd_request_duration_ms", ""));
        out
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            requests_total: Arc::new(RwLock::new(HashMap::new())),
            requests_by_backend: Arc::new(RwLock::new(HashMap::new())),
            latency_buckets: Arc::new(RwLock::new(LatencyHistogram::new())),
            routing_selections: Arc::new(RwLock::new(HashMap::new())),
            tokens_total: Arc::new(RwLock::new(HashMap::new())),
            tokens_per_second_ema: Arc::new(RwLock::new(0.0)),
            labeled_latency: Arc::new(RwLock::new(HashMap::new())),
            auto_classifications: Arc::new(RwLock::new(HashMap::new())),
            auto_classification_duration_sum: Arc::new(AtomicU64::new(0)),
            auto_classification_duration_count: Arc::new(AtomicU64::new(0)),
            auto_cache_hits: Arc::new(AtomicU64::new(0)),
        }
    }

    pub async fn record_request(&self, backend: &str, status: &str, duration_ms: u64) {
        // Increment by status
        {
            let mut map = self.requests_total.write().await;
            map.entry(status.to_string())
                .or_insert_with(|| AtomicU64::new(0))
                .fetch_add(1, Ordering::Relaxed);
        }
        // Increment by backend
        {
            let mut map = self.requests_by_backend.write().await;
            map.entry(backend.to_string())
                .or_insert_with(|| AtomicU64::new(0))
                .fetch_add(1, Ordering::Relaxed);
        }
        // Record latency
        {
            let hist = self.latency_buckets.read().await;
            hist.observe(duration_ms);
        }
    }

    pub async fn record_routing_selection(&self, backend: &str, strategy: &str) {
        let key = format!("{}|{}", backend, strategy);
        let mut map = self.routing_selections.write().await;
        map.entry(key)
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Record token usage for a model. Increments `herd_tokens_total` counters.
    pub async fn record_tokens(&self, model: &str, tokens_in: u32, tokens_out: u32) {
        let mut map = self.tokens_total.write().await;
        let key_in = format!("in|{}", model);
        map.entry(key_in)
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(tokens_in as u64, Ordering::Relaxed);
        let key_out = format!("out|{}", model);
        map.entry(key_out)
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(tokens_out as u64, Ordering::Relaxed);
    }

    /// Record tokens-per-second using exponential moving average (alpha=0.1).
    pub async fn record_tokens_per_second(&self, tps: f32) {
        let alpha = 0.1_f32;
        let mut ema = self.tokens_per_second_ema.write().await;
        if *ema == 0.0 {
            *ema = tps;
        } else {
            *ema = alpha * tps + (1.0 - alpha) * *ema;
        }
    }

    /// Record a request with backend, model, and status labels for the labeled histogram.
    pub async fn record_request_labeled(
        &self,
        backend: &str,
        model: &str,
        status: &str,
        duration_ms: u64,
    ) {
        let key = format!("{}|{}|{}", backend, model, status);
        let mut map = self.labeled_latency.write().await;
        const MAX_LABEL_COMBOS: usize = 200;
        if !map.contains_key(&key) && map.len() >= MAX_LABEL_COMBOS {
            return; // Cardinality cap reached — skip to avoid memory growth
        }
        map.entry(key)
            .or_insert_with(LatencyHistogram::new)
            .observe(duration_ms);
    }

    pub async fn record_auto_classification(
        &self,
        tier: &str,
        capability: &str,
        duration_ms: u64,
        cache_hit: bool,
    ) {
        let key = format!("{}|{}", tier, capability);
        let mut map = self.auto_classifications.write().await;
        map.entry(key)
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);
        drop(map);
        self.auto_classification_duration_sum
            .fetch_add(duration_ms, Ordering::Relaxed);
        self.auto_classification_duration_count
            .fetch_add(1, Ordering::Relaxed);
        if cache_hit {
            self.auto_cache_hits.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub async fn render(&self) -> String {
        let mut out = String::new();

        // Request totals by status
        out.push_str("# HELP herd_requests_total Total proxied requests by status\n");
        out.push_str("# TYPE herd_requests_total counter\n");
        {
            let map = self.requests_total.read().await;
            for (status, count) in map.iter() {
                out.push_str(&format!(
                    "herd_requests_total{{status=\"{}\"}} {}\n",
                    status,
                    count.load(Ordering::Relaxed)
                ));
            }
        }

        // Request totals by backend
        out.push_str("\n# HELP herd_requests_by_backend Total requests by backend\n");
        out.push_str("# TYPE herd_requests_by_backend counter\n");
        {
            let map = self.requests_by_backend.read().await;
            for (backend, count) in map.iter() {
                out.push_str(&format!(
                    "herd_requests_by_backend{{backend=\"{}\"}} {}\n",
                    backend,
                    count.load(Ordering::Relaxed)
                ));
            }
        }

        // Routing selections
        out.push_str(
            "\n# HELP herd_routing_selections_total Routing selections by backend and strategy\n",
        );
        out.push_str("# TYPE herd_routing_selections_total counter\n");
        {
            let map = self.routing_selections.read().await;
            for (key, count) in map.iter() {
                if let Some((backend, strategy)) = key.split_once('|') {
                    out.push_str(&format!(
                        "herd_routing_selections_total{{backend=\"{}\", strategy=\"{}\"}} {}\n",
                        backend,
                        strategy,
                        count.load(Ordering::Relaxed)
                    ));
                }
            }
        }

        // Latency histogram
        out.push('\n');
        {
            let hist = self.latency_buckets.read().await;
            out.push_str(&hist.render());
        }

        // Token totals
        out.push_str("\n# HELP herd_tokens_total Total tokens processed by direction and model\n");
        out.push_str("# TYPE herd_tokens_total counter\n");
        {
            let map = self.tokens_total.read().await;
            for (key, count) in map.iter() {
                if let Some((direction, model)) = key.split_once('|') {
                    out.push_str(&format!(
                        "herd_tokens_total{{direction=\"{}\", model=\"{}\"}} {}\n",
                        direction,
                        model,
                        count.load(Ordering::Relaxed)
                    ));
                }
            }
        }

        // Tokens per second EMA gauge
        out.push_str(
            "\n# HELP herd_tokens_per_second Tokens per second (exponential moving average)\n",
        );
        out.push_str("# TYPE herd_tokens_per_second gauge\n");
        {
            let ema = self.tokens_per_second_ema.read().await;
            out.push_str(&format!("herd_tokens_per_second {:.2}\n", *ema));
        }

        // Labeled latency histograms
        out.push_str("\n# HELP herd_request_duration_labeled_ms Request duration histogram with backend, model, and status labels\n");
        out.push_str("# TYPE herd_request_duration_labeled_ms histogram\n");
        {
            let map = self.labeled_latency.read().await;
            for (key, hist) in map.iter() {
                let parts: Vec<&str> = key.splitn(3, '|').collect();
                if parts.len() == 3 {
                    let (backend, model, status) = (parts[0], parts[1], parts[2]);
                    let labels = format!(
                        "backend=\"{}\", model=\"{}\", status=\"{}\"",
                        backend, model, status
                    );
                    out.push_str(
                        &hist.render_with_labels("herd_request_duration_labeled_ms", &labels),
                    );
                }
            }
        }

        // Auto classification metrics
        {
            let auto_map = self.auto_classifications.read().await;
            if !auto_map.is_empty() {
                out.push_str("# HELP herd_auto_classifications_total Total auto classifications by tier and capability\n");
                out.push_str("# TYPE herd_auto_classifications_total counter\n");
                for (key, count) in auto_map.iter() {
                    let count_val = count.load(Ordering::Relaxed);
                    if let Some((tier, capability)) = key.split_once('|') {
                        out.push_str(&format!(
                            "herd_auto_classifications_total{{tier=\"{}\",capability=\"{}\"}} {}\n",
                            tier, capability, count_val
                        ));
                    }
                }
            }
        }
        let auto_dur_count = self
            .auto_classification_duration_count
            .load(Ordering::Relaxed);
        if auto_dur_count > 0 {
            let auto_dur_sum = self
                .auto_classification_duration_sum
                .load(Ordering::Relaxed);
            out.push_str("# HELP herd_auto_classification_duration_ms_sum Total classification duration in ms\n");
            out.push_str("# TYPE herd_auto_classification_duration_ms_sum counter\n");
            out.push_str(&format!(
                "herd_auto_classification_duration_ms_sum {}\n",
                auto_dur_sum
            ));
            out.push_str(
                "# HELP herd_auto_classification_duration_ms_count Total classification calls\n",
            );
            out.push_str("# TYPE herd_auto_classification_duration_ms_count counter\n");
            out.push_str(&format!(
                "herd_auto_classification_duration_ms_count {}\n",
                auto_dur_count
            ));
        }
        let cache_hits = self.auto_cache_hits.load(Ordering::Relaxed);
        if cache_hits > 0 {
            out.push_str(
                "# HELP herd_auto_cache_hits_total Total auto classification cache hits\n",
            );
            out.push_str("# TYPE herd_auto_cache_hits_total counter\n");
            out.push_str(&format!("herd_auto_cache_hits_total {}\n", cache_hits));
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn records_and_renders_metrics() {
        let m = Metrics::new();
        m.record_request("backend-a", "success", 150).await;
        m.record_request("backend-a", "success", 50).await;
        m.record_request("backend-b", "error", 5000).await;

        let output = m.render().await;
        assert!(output.contains("herd_requests_total{status=\"success\"} 2"));
        assert!(output.contains("herd_requests_total{status=\"error\"} 1"));
        assert!(output.contains("herd_requests_by_backend{backend=\"backend-a\"} 2"));
        assert!(output.contains("herd_request_duration_ms_count 3"));
    }

    #[tokio::test]
    async fn records_and_renders_routing_selections() {
        let m = Metrics::new();
        m.record_routing_selection("gpu1", "priority").await;
        m.record_routing_selection("gpu1", "priority").await;
        m.record_routing_selection("gpu2", "least_busy").await;

        let output = m.render().await;
        assert!(output
            .contains("herd_routing_selections_total{backend=\"gpu1\", strategy=\"priority\"} 2"));
        assert!(output.contains(
            "herd_routing_selections_total{backend=\"gpu2\", strategy=\"least_busy\"} 1"
        ));
    }

    #[test]
    fn histogram_buckets_cumulative() {
        let h = LatencyHistogram::new();
        h.observe(5); // fits in 10ms bucket
        h.observe(75); // fits in 100ms bucket
        h.observe(300); // fits in 500ms bucket

        let rendered = h.render();
        // 10ms bucket should have 1 (the 5ms observation)
        assert!(rendered.contains("le=\"10\"} 1"));
        // 100ms bucket should have 2 cumulative (5ms + 75ms)
        assert!(rendered.contains("le=\"100\"} 2"));
        // 500ms bucket should have 3 cumulative
        assert!(rendered.contains("le=\"500\"} 3"));
    }

    #[tokio::test]
    async fn record_tokens_renders_correctly() {
        let m = Metrics::new();
        m.record_tokens("llama3:8b", 100, 200).await;
        m.record_tokens("llama3:8b", 50, 75).await;

        let output = m.render().await;
        assert!(output.contains("herd_tokens_total{direction=\"in\", model=\"llama3:8b\"} 150"));
        assert!(output.contains("herd_tokens_total{direction=\"out\", model=\"llama3:8b\"} 275"));
    }

    #[tokio::test]
    async fn tokens_per_second_ema() {
        let m = Metrics::new();
        // First value initializes the EMA
        m.record_tokens_per_second(100.0).await;
        {
            let ema = m.tokens_per_second_ema.read().await;
            assert!((100.0 - *ema).abs() < 0.01);
        }

        // Second value: EMA = 0.1 * 50 + 0.9 * 100 = 5 + 90 = 95
        m.record_tokens_per_second(50.0).await;
        {
            let ema = m.tokens_per_second_ema.read().await;
            assert!((95.0 - *ema).abs() < 0.01);
        }

        let output = m.render().await;
        assert!(output.contains("herd_tokens_per_second 95.00"));
    }

    #[tokio::test]
    async fn labeled_histogram_renders() {
        let m = Metrics::new();
        m.record_request_labeled("gpu1", "llama3:8b", "success", 150)
            .await;
        m.record_request_labeled("gpu1", "llama3:8b", "success", 50)
            .await;

        let output = m.render().await;
        assert!(output.contains("herd_request_duration_labeled_ms"));
        assert!(output.contains("backend=\"gpu1\""));
        assert!(output.contains("model=\"llama3:8b\""));
        assert!(output.contains("status=\"success\""));
        // Count should be 2
        assert!(output.contains(
            "herd_request_duration_labeled_ms_count{backend=\"gpu1\", model=\"llama3:8b\", status=\"success\"} 2"
        ));
    }

    #[tokio::test]
    async fn labeled_histogram_cardinality_cap() {
        let m = Metrics::new();
        // Fill up to the cap (200 unique label combos)
        for i in 0..200 {
            m.record_request_labeled("gpu1", &format!("model-{}", i), "success", 100)
                .await;
        }
        {
            let map = m.labeled_latency.read().await;
            assert_eq!(map.len(), 200);
        }
        // 201st unique combo should be silently dropped
        m.record_request_labeled("gpu1", "model-overflow", "success", 100)
            .await;
        {
            let map = m.labeled_latency.read().await;
            assert_eq!(map.len(), 200);
            assert!(!map.contains_key("gpu1|model-overflow|success"));
        }
        // Existing keys can still be updated
        m.record_request_labeled("gpu1", "model-0", "success", 200)
            .await;
        {
            let map = m.labeled_latency.read().await;
            let hist = map.get("gpu1|model-0|success").unwrap();
            assert_eq!(hist.count.load(Ordering::Relaxed), 2);
        }
    }

    #[tokio::test]
    async fn auto_classification_metrics_render() {
        let metrics = Metrics::new();
        metrics
            .record_auto_classification("standard", "code", 150, false)
            .await;
        metrics
            .record_auto_classification("heavy", "reasoning", 200, true)
            .await;
        let output = metrics.render().await;
        assert!(output
            .contains("herd_auto_classifications_total{tier=\"standard\",capability=\"code\"} 1"));
        assert!(output.contains(
            "herd_auto_classifications_total{tier=\"heavy\",capability=\"reasoning\"} 1"
        ));
        assert!(output.contains("herd_auto_classification_duration_ms_sum 350"));
        assert!(output.contains("herd_auto_classification_duration_ms_count 2"));
        assert!(output.contains("herd_auto_cache_hits_total 1"));
    }

    #[test]
    fn render_with_labels_no_labels() {
        let h = LatencyHistogram::new();
        h.observe(5);
        let rendered = h.render_with_labels("test_metric", "");
        assert!(rendered.contains("test_metric_bucket{le=\"10\"} 1"));
        assert!(rendered.contains("test_metric_bucket{le=\"+Inf\"} 1"));
        assert!(rendered.contains("test_metric_sum 5"));
        assert!(rendered.contains("test_metric_count 1"));
    }

    #[test]
    fn render_with_labels_with_labels() {
        let h = LatencyHistogram::new();
        h.observe(5);
        let rendered = h.render_with_labels("test_metric", "backend=\"gpu1\"");
        assert!(rendered.contains("test_metric_bucket{backend=\"gpu1\",le=\"10\"} 1"));
        assert!(rendered.contains("test_metric_bucket{backend=\"gpu1\",le=\"+Inf\"} 1"));
        assert!(rendered.contains("test_metric_sum{backend=\"gpu1\"} 5"));
        assert!(rendered.contains("test_metric_count{backend=\"gpu1\"} 1"));
    }
}
