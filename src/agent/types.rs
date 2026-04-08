use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMessage {
    pub role: MessageRole,
    pub content: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub content: String,
    pub success: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    Thinking {
        session_id: String,
        round: u32,
    },
    ToolCall {
        session_id: String,
        tool: String,
        arguments: serde_json::Value,
    },
    ToolResult {
        session_id: String,
        tool: String,
        content: String,
        success: bool,
    },
    PermissionDenied {
        session_id: String,
        tool: String,
        reason: String,
    },
    Message {
        session_id: String,
        content: String,
    },
    Error {
        session_id: String,
        error: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    Active,
    Processing,
    Completed,
    Error,
}

impl std::fmt::Display for SessionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionStatus::Active => write!(f, "active"),
            SessionStatus::Processing => write!(f, "processing"),
            SessionStatus::Completed => write!(f, "completed"),
            SessionStatus::Error => write!(f, "error"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_role_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&MessageRole::System).unwrap(),
            "\"system\""
        );
        assert_eq!(
            serde_json::to_string(&MessageRole::User).unwrap(),
            "\"user\""
        );
        assert_eq!(
            serde_json::to_string(&MessageRole::Assistant).unwrap(),
            "\"assistant\""
        );
        assert_eq!(
            serde_json::to_string(&MessageRole::Tool).unwrap(),
            "\"tool\""
        );
    }

    #[test]
    fn message_role_deserializes_lowercase() {
        assert_eq!(
            serde_json::from_str::<MessageRole>("\"system\"").unwrap(),
            MessageRole::System
        );
        assert_eq!(
            serde_json::from_str::<MessageRole>("\"user\"").unwrap(),
            MessageRole::User
        );
        assert_eq!(
            serde_json::from_str::<MessageRole>("\"assistant\"").unwrap(),
            MessageRole::Assistant
        );
        assert_eq!(
            serde_json::from_str::<MessageRole>("\"tool\"").unwrap(),
            MessageRole::Tool
        );
    }

    #[test]
    fn session_status_display() {
        assert_eq!(SessionStatus::Active.to_string(), "active");
        assert_eq!(SessionStatus::Processing.to_string(), "processing");
        assert_eq!(SessionStatus::Completed.to_string(), "completed");
        assert_eq!(SessionStatus::Error.to_string(), "error");
    }

    #[test]
    fn session_status_roundtrip() {
        for status in [
            SessionStatus::Active,
            SessionStatus::Processing,
            SessionStatus::Completed,
            SessionStatus::Error,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let back: SessionStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(back, status);
        }
    }

    #[test]
    fn agent_message_minimal_json() {
        let msg = AgentMessage {
            role: MessageRole::User,
            content: "Hello".into(),
            tool_calls: None,
            tool_call_id: None,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["role"], "user");
        assert_eq!(json["content"], "Hello");
        // None fields should be skipped
        assert!(json.get("tool_calls").is_none());
        assert!(json.get("tool_call_id").is_none());
    }

    #[test]
    fn agent_message_with_tool_calls() {
        let msg = AgentMessage {
            role: MessageRole::Assistant,
            content: String::new(),
            tool_calls: Some(vec![ToolCall {
                id: "call_1".into(),
                name: "read_file".into(),
                arguments: serde_json::json!({"path": "/tmp/test.txt"}),
            }]),
            tool_call_id: None,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["tool_calls"][0]["name"], "read_file");
        assert_eq!(json["tool_calls"][0]["arguments"]["path"], "/tmp/test.txt");
    }

    #[test]
    fn tool_call_roundtrip() {
        let call = ToolCall {
            id: "abc123".into(),
            name: "run_command".into(),
            arguments: serde_json::json!({"command": "ls -la"}),
        };
        let json = serde_json::to_string(&call).unwrap();
        let back: ToolCall = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "abc123");
        assert_eq!(back.name, "run_command");
    }

    #[test]
    fn tool_result_roundtrip() {
        let result = ToolResult {
            content: "file contents here".into(),
            success: true,
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: ToolResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.content, "file contents here");
        assert!(back.success);
    }
}
