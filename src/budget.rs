use crate::config::BudgetConfig;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Result of checking whether a request is within budget.
#[derive(Debug, Clone)]
pub enum BudgetStatus {
    /// Request is within all budget caps.
    Ok { remaining_usd: f32 },
    /// A budget cap has been exceeded.
    Exceeded {
        cap_type: String,
        limit: f32,
        current: f32,
    },
    /// A budget cap is close to being exceeded (action = "warn").
    Warning {
        cap_type: String,
        limit: f32,
        current: f32,
    },
}

/// Summary of current budget state for the API.
#[derive(Debug, Clone, Serialize)]
pub struct BudgetSummary {
    pub global_spend: f32,
    pub global_limit: f32,
    pub client_spend: HashMap<String, f32>,
    pub model_spend: HashMap<String, f32>,
    pub period_start: String,
    pub period_end: String,
}

/// Tracks cumulative spend and enforces budget caps.
pub struct BudgetTracker {
    config: RwLock<BudgetConfig>,
    global_spend: RwLock<f32>,
    client_spend: RwLock<HashMap<String, f32>>,
    model_spend: RwLock<HashMap<String, f32>>,
    period_start: RwLock<chrono::DateTime<chrono::Utc>>,
}

impl BudgetTracker {
    pub fn new(config: BudgetConfig) -> Arc<Self> {
        Arc::new(Self {
            config: RwLock::new(config),
            global_spend: RwLock::new(0.0),
            client_spend: RwLock::new(HashMap::new()),
            model_spend: RwLock::new(HashMap::new()),
            period_start: RwLock::new(chrono::Utc::now()),
        })
    }

    /// Update the budget configuration (e.g., on config reload).
    /// Does NOT reset accumulators — only the caps and action change.
    pub async fn update_config(&self, config: BudgetConfig) {
        *self.config.write().await = config;
    }

    /// Record a cost after a request completes.
    pub async fn record_cost(&self, client: Option<&str>, model: &str, cost_usd: f32) {
        let config = self.config.read().await;
        if !config.enabled {
            return;
        }
        drop(config);

        *self.global_spend.write().await += cost_usd;

        if let Some(client) = client {
            *self
                .client_spend
                .write()
                .await
                .entry(client.to_string())
                .or_insert(0.0) += cost_usd;
        }

        *self
            .model_spend
            .write()
            .await
            .entry(model.to_string())
            .or_insert(0.0) += cost_usd;
    }

    /// Check if a request would exceed any budget cap.
    pub async fn check_budget(&self, client: Option<&str>, model: &str) -> BudgetStatus {
        let config = self.config.read().await;
        if !config.enabled {
            return BudgetStatus::Ok {
                remaining_usd: f32::MAX,
            };
        }

        let is_warn = config.action == "warn";

        // Check global limit
        if config.global_limit_usd > 0.0 {
            let current = *self.global_spend.read().await;
            if current >= config.global_limit_usd {
                if is_warn {
                    return BudgetStatus::Warning {
                        cap_type: "global".to_string(),
                        limit: config.global_limit_usd,
                        current,
                    };
                }
                return BudgetStatus::Exceeded {
                    cap_type: "global".to_string(),
                    limit: config.global_limit_usd,
                    current,
                };
            }
        }

        // Check per-client limit
        if let Some(client) = client {
            if let Some(&limit) = config.clients.get(client) {
                if limit > 0.0 {
                    let spend = self.client_spend.read().await;
                    let current = spend.get(client).copied().unwrap_or(0.0);
                    if current >= limit {
                        if is_warn {
                            return BudgetStatus::Warning {
                                cap_type: format!("client:{}", client),
                                limit,
                                current,
                            };
                        }
                        return BudgetStatus::Exceeded {
                            cap_type: format!("client:{}", client),
                            limit,
                            current,
                        };
                    }
                }
            }
        }

        // Check per-model limit
        if let Some(&limit) = config.models.get(model) {
            if limit > 0.0 {
                let spend = self.model_spend.read().await;
                let current = spend.get(model).copied().unwrap_or(0.0);
                if current >= limit {
                    if is_warn {
                        return BudgetStatus::Warning {
                            cap_type: format!("model:{}", model),
                            limit,
                            current,
                        };
                    }
                    return BudgetStatus::Exceeded {
                        cap_type: format!("model:{}", model),
                        limit,
                        current,
                    };
                }
            }
        }

        // Compute smallest remaining budget
        let remaining = self.remaining_budget(&config).await;

        BudgetStatus::Ok {
            remaining_usd: remaining,
        }
    }

    /// Check if the budget period has rolled over and reset accumulators if so.
    pub async fn reset_if_needed(&self) {
        let config = self.config.read().await;
        if !config.enabled {
            return;
        }

        let now = chrono::Utc::now();
        let start = *self.period_start.read().await;
        let should_reset = match config.reset_period.as_str() {
            "daily" => now.signed_duration_since(start).num_days() >= 1,
            "weekly" => now.signed_duration_since(start).num_weeks() >= 1,
            _ => {
                // monthly: reset if we've moved to a different calendar month
                now.format("%Y-%m").to_string() != start.format("%Y-%m").to_string()
            }
        };
        drop(config);

        if should_reset {
            *self.global_spend.write().await = 0.0;
            self.client_spend.write().await.clear();
            self.model_spend.write().await.clear();
            *self.period_start.write().await = now;
            tracing::info!("Budget period reset");
        }
    }

    /// Get a summary of the current budget state.
    pub async fn get_summary(&self) -> BudgetSummary {
        let config = self.config.read().await;
        let start = *self.period_start.read().await;
        let period_end = match config.reset_period.as_str() {
            "daily" => start + chrono::Duration::days(1),
            "weekly" => start + chrono::Duration::weeks(1),
            _ => {
                // monthly: advance to the same day next month (approximate with 30 days)
                start + chrono::Duration::days(30)
            }
        };

        BudgetSummary {
            global_spend: *self.global_spend.read().await,
            global_limit: config.global_limit_usd,
            client_spend: self.client_spend.read().await.clone(),
            model_spend: self.model_spend.read().await.clone(),
            period_start: start.to_rfc3339(),
            period_end: period_end.to_rfc3339(),
        }
    }

    /// Compute the smallest remaining budget across all applicable caps.
    async fn remaining_budget(&self, config: &BudgetConfig) -> f32 {
        let mut remaining = f32::MAX;

        if config.global_limit_usd > 0.0 {
            let spent = *self.global_spend.read().await;
            remaining = remaining.min(config.global_limit_usd - spent);
        }

        remaining
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BudgetConfig;

    fn make_config(enabled: bool, global_limit: f32, action: &str) -> BudgetConfig {
        BudgetConfig {
            enabled,
            global_limit_usd: global_limit,
            clients: HashMap::new(),
            models: HashMap::new(),
            reset_period: "monthly".to_string(),
            action: action.to_string(),
        }
    }

    #[tokio::test]
    async fn record_cost_accumulates() {
        let tracker = BudgetTracker::new(make_config(true, 100.0, "reject"));
        tracker.record_cost(Some("alice"), "llama3:8b", 1.5).await;
        tracker.record_cost(Some("alice"), "llama3:8b", 2.5).await;
        tracker
            .record_cost(Some("bob"), "qwen2:14b", 3.0)
            .await;

        let summary = tracker.get_summary().await;
        assert!((summary.global_spend - 7.0).abs() < 0.001);
        assert!((*summary.client_spend.get("alice").unwrap() - 4.0).abs() < 0.001);
        assert!((*summary.client_spend.get("bob").unwrap() - 3.0).abs() < 0.001);
        assert!((*summary.model_spend.get("llama3:8b").unwrap() - 4.0).abs() < 0.001);
        assert!((*summary.model_spend.get("qwen2:14b").unwrap() - 3.0).abs() < 0.001);
    }

    #[tokio::test]
    async fn global_cap_exceeded_rejects() {
        let tracker = BudgetTracker::new(make_config(true, 10.0, "reject"));
        tracker.record_cost(None, "llama3:8b", 10.5).await;

        match tracker.check_budget(None, "llama3:8b").await {
            BudgetStatus::Exceeded {
                cap_type,
                limit,
                current,
            } => {
                assert_eq!(cap_type, "global");
                assert!((limit - 10.0).abs() < 0.001);
                assert!((current - 10.5).abs() < 0.001);
            }
            other => panic!("Expected Exceeded, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn per_client_cap_exceeded_rejects() {
        let mut config = make_config(true, 0.0, "reject");
        config.clients.insert("alice".to_string(), 5.0);

        let tracker = BudgetTracker::new(config);
        tracker.record_cost(Some("alice"), "llama3:8b", 5.5).await;
        tracker.record_cost(Some("bob"), "llama3:8b", 100.0).await;

        // Alice is over limit
        match tracker.check_budget(Some("alice"), "llama3:8b").await {
            BudgetStatus::Exceeded {
                cap_type, limit, ..
            } => {
                assert_eq!(cap_type, "client:alice");
                assert!((limit - 5.0).abs() < 0.001);
            }
            other => panic!("Expected Exceeded for alice, got {:?}", other),
        }

        // Bob has no per-client cap, should be Ok
        match tracker.check_budget(Some("bob"), "llama3:8b").await {
            BudgetStatus::Ok { .. } => {}
            other => panic!("Expected Ok for bob, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn per_model_cap_exceeded_rejects() {
        let mut config = make_config(true, 0.0, "reject");
        config.models.insert("llama3:70b".to_string(), 20.0);

        let tracker = BudgetTracker::new(config);
        tracker.record_cost(None, "llama3:70b", 21.0).await;
        tracker.record_cost(None, "llama3:8b", 100.0).await;

        // 70b model is over limit
        match tracker.check_budget(None, "llama3:70b").await {
            BudgetStatus::Exceeded {
                cap_type, limit, ..
            } => {
                assert_eq!(cap_type, "model:llama3:70b");
                assert!((limit - 20.0).abs() < 0.001);
            }
            other => panic!("Expected Exceeded for 70b, got {:?}", other),
        }

        // 8b model has no cap
        match tracker.check_budget(None, "llama3:8b").await {
            BudgetStatus::Ok { .. } => {}
            other => panic!("Expected Ok for 8b, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn action_warn_returns_warning_instead_of_exceeded() {
        let tracker = BudgetTracker::new(make_config(true, 10.0, "warn"));
        tracker.record_cost(None, "llama3:8b", 10.5).await;

        match tracker.check_budget(None, "llama3:8b").await {
            BudgetStatus::Warning {
                cap_type,
                limit,
                current,
            } => {
                assert_eq!(cap_type, "global");
                assert!((limit - 10.0).abs() < 0.001);
                assert!((current - 10.5).abs() < 0.001);
            }
            other => panic!("Expected Warning, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn period_reset_clears_accumulators() {
        let tracker = BudgetTracker::new(make_config(true, 100.0, "reject"));
        tracker.record_cost(Some("alice"), "llama3:8b", 50.0).await;

        // Force the period start to the past
        {
            *tracker.period_start.write().await =
                chrono::Utc::now() - chrono::Duration::days(32);
        }

        tracker.reset_if_needed().await;

        let summary = tracker.get_summary().await;
        assert!((summary.global_spend).abs() < 0.001);
        assert!(summary.client_spend.is_empty());
        assert!(summary.model_spend.is_empty());
    }

    #[tokio::test]
    async fn zero_limit_means_unlimited() {
        let tracker = BudgetTracker::new(make_config(true, 0.0, "reject"));
        tracker.record_cost(None, "llama3:8b", 999999.0).await;

        match tracker.check_budget(None, "llama3:8b").await {
            BudgetStatus::Ok { .. } => {}
            other => panic!("Expected Ok (unlimited), got {:?}", other),
        }
    }

    #[tokio::test]
    async fn budget_disabled_always_ok() {
        let tracker = BudgetTracker::new(make_config(false, 1.0, "reject"));
        // Even though global limit is 1.0, budget is disabled
        tracker.record_cost(None, "llama3:8b", 999.0).await;

        match tracker.check_budget(None, "llama3:8b").await {
            BudgetStatus::Ok { .. } => {}
            other => panic!("Expected Ok (disabled), got {:?}", other),
        }
    }

    #[tokio::test]
    async fn disabled_budget_does_not_record_cost() {
        let tracker = BudgetTracker::new(make_config(false, 100.0, "reject"));
        tracker.record_cost(Some("alice"), "llama3:8b", 50.0).await;

        let summary = tracker.get_summary().await;
        assert!((summary.global_spend).abs() < 0.001);
    }
}
