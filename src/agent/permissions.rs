use crate::agent::types::ToolCall;
use crate::config::PermissionsConfig;
use regex::Regex;

#[derive(Debug, Clone)]
pub enum PermissionResult {
    Allowed,
    Denied(String),
}

#[derive(Clone)]
pub struct PermissionEngine {
    deny_file_patterns: Vec<Regex>,
    deny_bash_patterns: Vec<Regex>,
    allow_shell_commands: bool,
    // Keep raw strings for error messages
    deny_file_raw: Vec<String>,
    deny_bash_raw: Vec<String>,
}

impl PermissionEngine {
    pub fn new(config: &PermissionsConfig) -> Self {
        let deny_file_patterns = config
            .deny_file_patterns
            .iter()
            .filter_map(|p| match Regex::new(p) {
                Ok(r) => Some(r),
                Err(e) => {
                    tracing::warn!("Invalid file deny pattern '{}': {}", p, e);
                    None
                }
            })
            .collect();

        let deny_bash_patterns = config
            .deny_bash_patterns
            .iter()
            .filter_map(|p| match Regex::new(p) {
                Ok(r) => Some(r),
                Err(e) => {
                    tracing::warn!("Invalid bash deny pattern '{}': {}", p, e);
                    None
                }
            })
            .collect();

        Self {
            deny_file_patterns,
            deny_bash_patterns,
            allow_shell_commands: config.allow_shell_commands,
            deny_file_raw: config.deny_file_patterns.clone(),
            deny_bash_raw: config.deny_bash_patterns.clone(),
        }
    }

    pub fn check_file_access(&self, path: &str) -> PermissionResult {
        for (i, pattern) in self.deny_file_patterns.iter().enumerate() {
            if pattern.is_match(path) {
                let raw = &self.deny_file_raw[i];
                return PermissionResult::Denied(format!(
                    "File path '{}' matches deny pattern '{}'",
                    path, raw
                ));
            }
        }
        PermissionResult::Allowed
    }

    pub fn check_bash_command(&self, command: &str) -> PermissionResult {
        for (i, pattern) in self.deny_bash_patterns.iter().enumerate() {
            if pattern.is_match(command) {
                let raw = &self.deny_bash_raw[i];
                return PermissionResult::Denied(format!("Command matches deny pattern '{}'", raw));
            }
        }
        PermissionResult::Allowed
    }

    pub fn check_tool_call(&self, call: &ToolCall) -> PermissionResult {
        match call.name.as_str() {
            "read_file" | "write_file" | "list_files" => {
                if let Some(path) = call.arguments.get("path").and_then(|v| v.as_str()) {
                    self.check_file_access(path)
                } else {
                    PermissionResult::Allowed
                }
            }
            "run_command" => {
                if !self.allow_shell_commands {
                    return PermissionResult::Denied(
                        "Shell commands are disabled by configuration".to_string(),
                    );
                }
                if let Some(cmd) = call.arguments.get("command").and_then(|v| v.as_str()) {
                    self.check_bash_command(cmd)
                } else {
                    PermissionResult::Allowed
                }
            }
            _ => PermissionResult::Allowed,
        }
    }

    pub fn allows_shell_commands(&self) -> bool {
        self.allow_shell_commands
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PermissionsConfig;

    fn permissive() -> PermissionEngine {
        PermissionEngine::new(&PermissionsConfig {
            deny_file_patterns: vec![],
            deny_bash_patterns: vec![],
            allow_shell_commands: true,
        })
    }

    fn restrictive() -> PermissionEngine {
        PermissionEngine::new(&PermissionsConfig {
            deny_file_patterns: vec![r"\.env$".into(), r"\.ssh".into(), r"/etc/shadow".into()],
            deny_bash_patterns: vec![r"rm\s+-rf\s+/".into(), r"\bsudo\b".into()],
            allow_shell_commands: true,
        })
    }

    #[test]
    fn permissive_allows_everything() {
        let engine = permissive();
        assert!(matches!(
            engine.check_file_access("/tmp/test.txt"),
            PermissionResult::Allowed
        ));
        assert!(matches!(
            engine.check_bash_command("ls -la"),
            PermissionResult::Allowed
        ));
    }

    #[test]
    fn denies_file_patterns() {
        let engine = restrictive();
        assert!(matches!(
            engine.check_file_access("/app/.env"),
            PermissionResult::Denied(_)
        ));
        assert!(matches!(
            engine.check_file_access("/home/user/.ssh/id_rsa"),
            PermissionResult::Denied(_)
        ));
        assert!(matches!(
            engine.check_file_access("/etc/shadow"),
            PermissionResult::Denied(_)
        ));
    }

    #[test]
    fn allows_non_matching_files() {
        let engine = restrictive();
        assert!(matches!(
            engine.check_file_access("/tmp/test.txt"),
            PermissionResult::Allowed
        ));
        assert!(matches!(
            engine.check_file_access("/home/user/code.rs"),
            PermissionResult::Allowed
        ));
    }

    #[test]
    fn denies_bash_patterns() {
        let engine = restrictive();
        assert!(matches!(
            engine.check_bash_command("rm -rf /"),
            PermissionResult::Denied(_)
        ));
        // Regex catches extra whitespace too
        assert!(matches!(
            engine.check_bash_command("rm  -rf  /home"),
            PermissionResult::Denied(_)
        ));
        assert!(matches!(
            engine.check_bash_command("sudo apt install foo"),
            PermissionResult::Denied(_)
        ));
    }

    #[test]
    fn allows_non_matching_commands() {
        let engine = restrictive();
        assert!(matches!(
            engine.check_bash_command("ls -la"),
            PermissionResult::Allowed
        ));
        assert!(matches!(
            engine.check_bash_command("cat /tmp/test.txt"),
            PermissionResult::Allowed
        ));
    }

    #[test]
    fn regex_word_boundary_prevents_false_positives() {
        let engine = restrictive();
        // "sudo" with word boundary should NOT match "pseudocode"
        assert!(matches!(
            engine.check_bash_command("echo pseudocode"),
            PermissionResult::Allowed
        ));
    }

    #[test]
    fn check_tool_call_read_file() {
        let engine = restrictive();
        let call = ToolCall {
            id: "1".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({"path": "/app/.env"}),
        };
        assert!(matches!(
            engine.check_tool_call(&call),
            PermissionResult::Denied(_)
        ));
    }

    #[test]
    fn check_tool_call_run_command() {
        let engine = restrictive();
        let call = ToolCall {
            id: "1".into(),
            name: "run_command".into(),
            arguments: serde_json::json!({"command": "sudo rm -rf /"}),
        };
        assert!(matches!(
            engine.check_tool_call(&call),
            PermissionResult::Denied(_)
        ));
    }

    #[test]
    fn check_tool_call_allowed() {
        let engine = restrictive();
        let call = ToolCall {
            id: "1".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({"path": "/tmp/test.txt"}),
        };
        assert!(matches!(
            engine.check_tool_call(&call),
            PermissionResult::Allowed
        ));
    }

    #[test]
    fn unknown_tool_allowed() {
        let engine = restrictive();
        let call = ToolCall {
            id: "1".into(),
            name: "unknown_tool".into(),
            arguments: serde_json::json!({}),
        };
        assert!(matches!(
            engine.check_tool_call(&call),
            PermissionResult::Allowed
        ));
    }

    #[test]
    fn shell_commands_disabled_by_default() {
        let engine = PermissionEngine::new(&PermissionsConfig::default());
        let call = ToolCall {
            id: "1".into(),
            name: "run_command".into(),
            arguments: serde_json::json!({"command": "ls -la"}),
        };
        assert!(matches!(
            engine.check_tool_call(&call),
            PermissionResult::Denied(_)
        ));
    }
}
