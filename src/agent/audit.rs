use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub timestamp: i64,
    pub session_id: String,
    #[serde(rename = "type")]
    pub entry_type: AuditType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub success: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AuditType {
    SessionCreated,
    SessionDeleted,
    ToolCall,
    ToolResult,
    PermissionDenied,
    Message,
    Error,
}

pub struct AgentAudit {
    log_path: PathBuf,
    file_lock: Arc<Mutex<()>>,
}

impl AgentAudit {
    pub fn new() -> Result<Self> {
        let log_dir = dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("Could not find home directory"))?
            .join(".herd");

        std::fs::create_dir_all(&log_dir)?;
        let log_path = log_dir.join("agent_audit.jsonl");

        // Touch the file
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;
        drop(file);

        Ok(Self {
            log_path,
            file_lock: Arc::new(Mutex::new(())),
        })
    }

    pub async fn log(&self, entry: AuditEntry) -> Result<()> {
        let _guard = self.file_lock.lock().await;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)?;
        let json = serde_json::to_string(&entry)?;
        writeln!(file, "{}", json)?;
        file.flush()?;
        Ok(())
    }

    pub async fn log_event(&self, event: &crate::agent::types::AgentEvent) {
        let entry = match event {
            crate::agent::types::AgentEvent::ToolCall {
                session_id: sid,
                tool,
                arguments,
            } => AuditEntry {
                timestamp: chrono::Utc::now().timestamp(),
                session_id: sid.clone(),
                entry_type: AuditType::ToolCall,
                tool: Some(tool.clone()),
                detail: Some(arguments.to_string()),
                success: None,
            },
            crate::agent::types::AgentEvent::ToolResult {
                session_id: sid,
                tool,
                success,
                ..
            } => AuditEntry {
                timestamp: chrono::Utc::now().timestamp(),
                session_id: sid.clone(),
                entry_type: AuditType::ToolResult,
                tool: Some(tool.clone()),
                detail: None,
                success: Some(*success),
            },
            crate::agent::types::AgentEvent::PermissionDenied {
                session_id: sid,
                tool,
                reason,
            } => AuditEntry {
                timestamp: chrono::Utc::now().timestamp(),
                session_id: sid.clone(),
                entry_type: AuditType::PermissionDenied,
                tool: Some(tool.clone()),
                detail: Some(reason.clone()),
                success: Some(false),
            },
            crate::agent::types::AgentEvent::Error {
                session_id: sid,
                error,
            } => AuditEntry {
                timestamp: chrono::Utc::now().timestamp(),
                session_id: sid.clone(),
                entry_type: AuditType::Error,
                tool: None,
                detail: Some(error.clone()),
                success: Some(false),
            },
            _ => return, // Thinking and Message events are not audited
        };

        let _ = self.log(entry).await;
    }

    pub async fn log_session_created(&self, session_id: &str, model: &str) {
        let entry = AuditEntry {
            timestamp: chrono::Utc::now().timestamp(),
            session_id: session_id.to_string(),
            entry_type: AuditType::SessionCreated,
            tool: None,
            detail: Some(model.to_string()),
            success: None,
        };
        let _ = self.log(entry).await;
    }

    pub async fn log_session_deleted(&self, session_id: &str) {
        let entry = AuditEntry {
            timestamp: chrono::Utc::now().timestamp(),
            session_id: session_id.to_string(),
            entry_type: AuditType::SessionDeleted,
            tool: None,
            detail: None,
            success: None,
        };
        let _ = self.log(entry).await;
    }

    /// Remove audit entries older than `max_age_secs`. Rewrites the log file
    /// keeping only recent entries.
    pub async fn cleanup_old(&self, max_age_secs: i64) -> Result<usize> {
        let _guard = self.file_lock.lock().await;
        let cutoff = chrono::Utc::now().timestamp() - max_age_secs;

        let file = match std::fs::File::open(&self.log_path) {
            Ok(f) => f,
            Err(_) => return Ok(0),
        };
        let reader = BufReader::new(file);

        let mut kept_lines = Vec::new();
        let mut removed = 0usize;

        for line in reader.lines() {
            let line = line?;
            if let Ok(entry) = serde_json::from_str::<AuditEntry>(&line) {
                if entry.timestamp >= cutoff {
                    kept_lines.push(line);
                } else {
                    removed += 1;
                }
            }
        }

        if removed > 0 {
            let temp_path = self.log_path.with_extension("jsonl.tmp");
            let mut temp = std::fs::File::create(&temp_path)?;
            for line in &kept_lines {
                writeln!(temp, "{}", line)?;
            }
            temp.flush()?;
            std::fs::rename(&temp_path, &self.log_path)?;
            tracing::info!(
                "Audit log cleanup: removed {} old entries, kept {}",
                removed,
                kept_lines.len()
            );
        }

        Ok(removed)
    }

    pub async fn get_stats(&self, since_seconds: i64) -> Result<AgentAuditStats> {
        let _guard = self.file_lock.lock().await;
        let cutoff = chrono::Utc::now().timestamp() - since_seconds;

        let file = std::fs::File::open(&self.log_path)?;
        let reader = BufReader::new(file);

        let mut total_events = 0u64;
        let mut tool_calls = 0u64;
        let mut permission_denials = 0u64;
        let mut sessions_created = 0u64;
        let mut sessions_deleted = 0u64;
        let mut errors = 0u64;
        let mut tool_counts: std::collections::HashMap<String, u64> =
            std::collections::HashMap::new();

        for line in reader.lines() {
            let line = line?;
            if let Ok(entry) = serde_json::from_str::<AuditEntry>(&line) {
                if entry.timestamp >= cutoff {
                    total_events += 1;
                    match entry.entry_type {
                        AuditType::ToolCall => {
                            tool_calls += 1;
                            if let Some(ref tool) = entry.tool {
                                *tool_counts.entry(tool.clone()).or_insert(0) += 1;
                            }
                        }
                        AuditType::PermissionDenied => permission_denials += 1,
                        AuditType::SessionCreated => sessions_created += 1,
                        AuditType::SessionDeleted => sessions_deleted += 1,
                        AuditType::Error => errors += 1,
                        _ => {}
                    }
                }
            }
        }

        Ok(AgentAuditStats {
            total_events,
            tool_calls,
            permission_denials,
            sessions_created,
            sessions_deleted,
            errors,
            tool_counts,
        })
    }
}

#[derive(Debug, Serialize)]
pub struct AgentAuditStats {
    pub total_events: u64,
    pub tool_calls: u64,
    pub permission_denials: u64,
    pub sessions_created: u64,
    pub sessions_deleted: u64,
    pub errors: u64,
    pub tool_counts: std::collections::HashMap<String, u64>,
}
