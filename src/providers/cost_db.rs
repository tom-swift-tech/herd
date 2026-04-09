use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;
use std::sync::Mutex;

pub struct CostDb {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderCostSummary {
    pub provider: String,
    pub total_cost_usd: f64,
    pub total_tokens_in: u64,
    pub total_tokens_out: u64,
    pub request_count: u64,
}

impl CostDb {
    pub fn new(conn: Connection) -> Self {
        let db = Self {
            conn: Mutex::new(conn),
        };
        db.migrate()
            .unwrap_or_else(|e| tracing::warn!("Frontier cost migration failed: {}", e));
        db
    }

    pub fn migrate(&self) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {}", e))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS frontier_costs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                provider TEXT NOT NULL,
                model TEXT NOT NULL,
                tokens_in INTEGER NOT NULL,
                tokens_out INTEGER NOT NULL,
                cost_usd REAL NOT NULL,
                request_id TEXT,
                timestamp TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE INDEX IF NOT EXISTS idx_frontier_costs_provider_month
                ON frontier_costs(provider, timestamp);",
        )?;
        Ok(())
    }

    pub fn record_cost(
        &self,
        provider: &str,
        model: &str,
        tokens_in: u64,
        tokens_out: u64,
        cost_usd: f32,
        request_id: Option<&str>,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {}", e))?;
        conn.execute(
            "INSERT INTO frontier_costs (provider, model, tokens_in, tokens_out, cost_usd, request_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![provider, model, tokens_in as i64, tokens_out as i64, cost_usd as f64, request_id],
        )?;
        Ok(())
    }

    pub fn monthly_spend(&self, provider: &str) -> Result<f64> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {}", e))?;
        let now = chrono::Utc::now();
        let month_start = format!("{}-{:02}-01T00:00:00", now.format("%Y"), now.format("%m"));
        let total: f64 = conn.query_row(
            "SELECT COALESCE(SUM(cost_usd), 0.0) FROM frontier_costs WHERE provider = ?1 AND timestamp >= ?2",
            rusqlite::params![provider, month_start],
            |row| row.get(0),
        )?;
        Ok(total)
    }

    pub fn cost_summary(&self) -> Result<Vec<ProviderCostSummary>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {}", e))?;
        let now = chrono::Utc::now();
        let month_start = format!("{}-{:02}-01T00:00:00", now.format("%Y"), now.format("%m"));
        let mut stmt = conn.prepare(
            "SELECT provider, COALESCE(SUM(cost_usd),0), COALESCE(SUM(tokens_in),0), COALESCE(SUM(tokens_out),0), COUNT(*) FROM frontier_costs WHERE timestamp >= ?1 GROUP BY provider"
        )?;
        let rows = stmt
            .query_map(rusqlite::params![month_start], |row| {
                Ok(ProviderCostSummary {
                    provider: row.get(0)?,
                    total_cost_usd: row.get(1)?,
                    total_tokens_in: row.get::<_, i64>(2)? as u64,
                    total_tokens_out: row.get::<_, i64>(3)? as u64,
                    request_count: row.get::<_, i64>(4)? as u64,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn in_memory_db() -> CostDb {
        CostDb::new(Connection::open_in_memory().unwrap())
    }

    #[test]
    fn record_and_query_cost() {
        let db = in_memory_db();
        db.record_cost("openai", "gpt-4o", 1000, 500, 0.015, Some("req-abc"))
            .unwrap();

        let summary = db.cost_summary().unwrap();
        assert_eq!(summary.len(), 1);
        let entry = &summary[0];
        assert_eq!(entry.provider, "openai");
        assert_eq!(entry.total_tokens_in, 1000);
        assert_eq!(entry.total_tokens_out, 500);
        assert_eq!(entry.request_count, 1);
        assert!((entry.total_cost_usd - 0.015_f64).abs() < 1e-6);
    }

    #[test]
    fn monthly_spend_filters_by_provider() {
        let db = in_memory_db();
        db.record_cost("openai", "gpt-4o", 1000, 500, 0.10, None)
            .unwrap();
        db.record_cost("openai", "gpt-4o", 2000, 800, 0.20, None)
            .unwrap();
        db.record_cost("anthropic", "claude-3-5-sonnet", 500, 200, 0.05, None)
            .unwrap();

        let openai_spend = db.monthly_spend("openai").unwrap();
        let anthropic_spend = db.monthly_spend("anthropic").unwrap();
        let unknown_spend = db.monthly_spend("unknown").unwrap();

        assert!((openai_spend - 0.30_f64).abs() < 1e-6);
        assert!((anthropic_spend - 0.05_f64).abs() < 1e-6);
        assert!((unknown_spend - 0.0_f64).abs() < 1e-6);
    }

    #[test]
    fn cost_summary_returns_all_providers() {
        let db = in_memory_db();
        db.record_cost("openai", "gpt-4o", 1000, 500, 0.10, None)
            .unwrap();
        db.record_cost("anthropic", "claude-3-5-sonnet", 800, 300, 0.08, None)
            .unwrap();
        db.record_cost("gemini", "gemini-2.0-flash", 600, 200, 0.04, None)
            .unwrap();
        db.record_cost("openai", "gpt-4o-mini", 200, 100, 0.01, None)
            .unwrap();

        let summary = db.cost_summary().unwrap();
        assert_eq!(summary.len(), 3);

        let mut providers: Vec<&str> = summary.iter().map(|s| s.provider.as_str()).collect();
        providers.sort();
        assert_eq!(providers, ["anthropic", "gemini", "openai"]);

        let openai = summary.iter().find(|s| s.provider == "openai").unwrap();
        assert_eq!(openai.request_count, 2);
        assert!((openai.total_cost_usd - 0.11_f64).abs() < 1e-6);
    }
}
