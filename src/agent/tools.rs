use crate::agent::types::ToolResult;
use serde_json::json;

/// Returns tool definitions in Ollama's expected format for `/api/chat`.
pub fn tool_definitions(allow_shell_commands: bool) -> Vec<serde_json::Value> {
    let mut tools = vec![
        json!({
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read the contents of a file at the given path",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "The file path to read"
                        }
                    },
                    "required": ["path"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "Write content to a file at the given path",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "The file path to write to"
                        },
                        "content": {
                            "type": "string",
                            "description": "The content to write"
                        }
                    },
                    "required": ["path", "content"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "list_files",
                "description": "List files and directories at the given path",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "The directory path to list"
                        }
                    },
                    "required": ["path"]
                }
            }
        }),
    ];

    if allow_shell_commands {
        tools.push(json!({
            "type": "function",
            "function": {
                "name": "run_command",
                "description": "Run a shell command and return its output",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "The shell command to execute"
                        }
                    },
                    "required": ["command"]
                }
            }
        }));
    }

    tools
}

/// Execute a tool call and return the result.
pub async fn execute_tool(name: &str, arguments: &serde_json::Value) -> ToolResult {
    match name {
        "read_file" => execute_read_file(arguments).await,
        "write_file" => execute_write_file(arguments).await,
        "list_files" => execute_list_files(arguments).await,
        "run_command" => execute_run_command(arguments).await,
        _ => ToolResult {
            content: format!("Unknown tool: {}", name),
            success: false,
        },
    }
}

async fn execute_read_file(args: &serde_json::Value) -> ToolResult {
    let path = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => {
            return ToolResult {
                content: "Missing required parameter: path".into(),
                success: false,
            }
        }
    };

    const MAX_READ_BYTES: u64 = 10 * 1024 * 1024; // 10 MB cap

    // Check file size before reading to prevent OOM
    match tokio::fs::metadata(path).await {
        Ok(meta) if meta.len() > MAX_READ_BYTES => {
            return ToolResult {
                content: format!(
                    "File too large ({} bytes, max {})",
                    meta.len(),
                    MAX_READ_BYTES
                ),
                success: false,
            };
        }
        Err(e) => {
            return ToolResult {
                content: format!("Error reading file: {}", e),
                success: false,
            };
        }
        _ => {}
    }

    match tokio::fs::read_to_string(path).await {
        Ok(content) => ToolResult {
            content,
            success: true,
        },
        Err(e) => ToolResult {
            content: format!("Error reading file: {}", e),
            success: false,
        },
    }
}

async fn execute_write_file(args: &serde_json::Value) -> ToolResult {
    let path = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => {
            return ToolResult {
                content: "Missing required parameter: path".into(),
                success: false,
            }
        }
    };
    let content = match args.get("content").and_then(|v| v.as_str()) {
        Some(c) => c,
        None => {
            return ToolResult {
                content: "Missing required parameter: content".into(),
                success: false,
            }
        }
    };

    match tokio::fs::write(path, content).await {
        Ok(()) => ToolResult {
            content: format!("Successfully wrote {} bytes to {}", content.len(), path),
            success: true,
        },
        Err(e) => ToolResult {
            content: format!("Error writing file: {}", e),
            success: false,
        },
    }
}

async fn execute_list_files(args: &serde_json::Value) -> ToolResult {
    let path = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => {
            return ToolResult {
                content: "Missing required parameter: path".into(),
                success: false,
            }
        }
    };

    match tokio::fs::read_dir(path).await {
        Ok(mut entries) => {
            let mut names = Vec::new();
            while let Ok(Some(entry)) = entries.next_entry().await {
                let file_type = entry.file_type().await.ok();
                let suffix = if file_type.is_some_and(|t| t.is_dir()) {
                    "/"
                } else {
                    ""
                };
                names.push(format!("{}{}", entry.file_name().to_string_lossy(), suffix));
            }
            names.sort();
            ToolResult {
                content: names.join("\n"),
                success: true,
            }
        }
        Err(e) => ToolResult {
            content: format!("Error listing directory: {}", e),
            success: false,
        },
    }
}

async fn execute_run_command(args: &serde_json::Value) -> ToolResult {
    let command = match args.get("command").and_then(|v| v.as_str()) {
        Some(c) => c,
        None => {
            return ToolResult {
                content: "Missing required parameter: command".into(),
                success: false,
            }
        }
    };

    let output = if cfg!(target_os = "windows") {
        tokio::process::Command::new("cmd")
            .args(["/C", command])
            .output()
            .await
    } else {
        tokio::process::Command::new("sh")
            .args(["-c", command])
            .output()
            .await
    };

    match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            let content = if stderr.is_empty() {
                stdout.to_string()
            } else if stdout.is_empty() {
                stderr.to_string()
            } else {
                format!("{}\n{}", stdout, stderr)
            };
            ToolResult {
                content,
                success: out.status.success(),
            }
        }
        Err(e) => ToolResult {
            content: format!("Error executing command: {}", e),
            success: false,
        },
    }
}
