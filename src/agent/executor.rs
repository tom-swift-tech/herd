use crate::agent::permissions::{PermissionEngine, PermissionResult};
use crate::agent::session::Session;
use crate::agent::tools;
use crate::agent::types::{AgentEvent, AgentMessage, MessageRole, SessionStatus, ToolCall};
use crate::router::{Router, RouterEnum};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::mpsc;

// -- Ollama wire types (only used at the API boundary) --

#[derive(Debug, Serialize)]
struct OllamaChatRequest {
    model: String,
    messages: Vec<OllamaMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<serde_json::Value>>,
    stream: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OllamaMessage {
    role: String,
    #[serde(default)]
    content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OllamaToolCall>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OllamaToolCall {
    function: OllamaFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OllamaFunction {
    name: String,
    arguments: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct OllamaChatResponse {
    message: OllamaMessage,
    #[allow(dead_code)]
    done: bool,
}

// -- Conversion helpers --

fn to_ollama_message(msg: &AgentMessage) -> OllamaMessage {
    let role = match msg.role {
        MessageRole::System => "system",
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::Tool => "tool",
    };

    let tool_calls = msg.tool_calls.as_ref().map(|calls| {
        calls
            .iter()
            .map(|c| OllamaToolCall {
                function: OllamaFunction {
                    name: c.name.clone(),
                    arguments: c.arguments.clone(),
                },
            })
            .collect()
    });

    OllamaMessage {
        role: role.to_string(),
        content: msg.content.clone(),
        tool_calls,
    }
}

fn from_ollama_tool_calls(calls: &[OllamaToolCall]) -> Vec<ToolCall> {
    calls
        .iter()
        .enumerate()
        .map(|(i, c)| ToolCall {
            id: format!("call_{}", i),
            name: c.function.name.clone(),
            arguments: c.function.arguments.clone(),
        })
        .collect()
}

// -- Executor --

pub struct AgentExecutor {
    client: Arc<reqwest::Client>,
    router: RouterEnum,
    permissions: PermissionEngine,
    max_tool_rounds: u32,
    request_timeout: std::time::Duration,
}

impl AgentExecutor {
    pub fn new(
        client: Arc<reqwest::Client>,
        router: RouterEnum,
        permissions: PermissionEngine,
        max_tool_rounds: u32,
        request_timeout: std::time::Duration,
    ) -> Self {
        Self {
            client,
            router,
            permissions,
            max_tool_rounds,
            request_timeout,
        }
    }

    /// Run the agent loop: send message, handle tool calls, return final response.
    /// Mutates the session in-place (appends messages, updates status).
    pub async fn execute(&self, session: &mut Session, user_message: String) -> Result<String> {
        self.execute_inner(session, user_message, None).await
    }

    /// Run the agent loop with event streaming.
    /// Events are sent to the channel as they happen. The final text content is still returned.
    pub async fn execute_streaming(
        &self,
        session: &mut Session,
        user_message: String,
        events: mpsc::Sender<AgentEvent>,
    ) -> Result<String> {
        self.execute_inner(session, user_message, Some(events))
            .await
    }

    async fn emit(events: &Option<mpsc::Sender<AgentEvent>>, event: AgentEvent) {
        if let Some(ref tx) = events {
            let _ = tx.send(event).await;
        }
    }

    async fn execute_inner(
        &self,
        session: &mut Session,
        user_message: String,
        events: Option<mpsc::Sender<AgentEvent>>,
    ) -> Result<String> {
        // Append user message
        session.messages.push(AgentMessage {
            role: MessageRole::User,
            content: user_message,
            tool_calls: None,
            tool_call_id: None,
        });
        session.status = SessionStatus::Processing;

        let tool_defs = tools::tool_definitions(self.permissions.allows_shell_commands());

        for round in 0..self.max_tool_rounds {
            // Route to a backend that has this model
            let backend = self.router.route(Some(&session.model), None).await?;
            tracing::debug!(
                "Agent session {} routed to {} for model {}",
                session.id,
                backend.name,
                session.model
            );

            Self::emit(
                &events,
                AgentEvent::Thinking {
                    session_id: session.id.clone(),
                    round,
                },
            )
            .await;

            // Build Ollama request
            let ollama_messages: Vec<OllamaMessage> =
                session.messages.iter().map(to_ollama_message).collect();

            let request = OllamaChatRequest {
                model: session.model.clone(),
                messages: ollama_messages,
                tools: Some(tool_defs.clone()),
                stream: false,
            };

            // Call Ollama
            let url = format!("{}/api/chat", backend.url);
            let resp = self
                .client
                .post(&url)
                .timeout(self.request_timeout)
                .json(&request)
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("Backend request failed: {}", e))?;

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                let err_msg = format!("Backend returned {}: {}", status, body);
                Self::emit(
                    &events,
                    AgentEvent::Error {
                        session_id: session.id.clone(),
                        error: err_msg.clone(),
                    },
                )
                .await;
                return Err(anyhow::anyhow!(err_msg));
            }

            let chat_resp: OllamaChatResponse = resp
                .json()
                .await
                .map_err(|e| anyhow::anyhow!("Failed to parse backend response: {}", e))?;

            // Check if model wants to call tools
            if let Some(ref ollama_calls) = chat_resp.message.tool_calls {
                if !ollama_calls.is_empty() {
                    let internal_calls = from_ollama_tool_calls(ollama_calls);

                    // Record assistant message with tool calls
                    session.messages.push(AgentMessage {
                        role: MessageRole::Assistant,
                        content: chat_resp.message.content.clone(),
                        tool_calls: Some(internal_calls.clone()),
                        tool_call_id: None,
                    });

                    // Execute each tool call
                    for call in &internal_calls {
                        let result_content = match self.permissions.check_tool_call(call) {
                            PermissionResult::Allowed => {
                                tracing::info!(
                                    "Session {}: executing tool {}",
                                    session.id,
                                    call.name
                                );

                                Self::emit(
                                    &events,
                                    AgentEvent::ToolCall {
                                        session_id: session.id.clone(),
                                        tool: call.name.clone(),
                                        arguments: call.arguments.clone(),
                                    },
                                )
                                .await;

                                let result = tools::execute_tool(&call.name, &call.arguments).await;

                                Self::emit(
                                    &events,
                                    AgentEvent::ToolResult {
                                        session_id: session.id.clone(),
                                        tool: call.name.clone(),
                                        content: result.content.clone(),
                                        success: result.success,
                                    },
                                )
                                .await;

                                result.content
                            }
                            PermissionResult::Denied(reason) => {
                                tracing::warn!(
                                    "Session {}: permission denied for {}: {}",
                                    session.id,
                                    call.name,
                                    reason
                                );

                                Self::emit(
                                    &events,
                                    AgentEvent::PermissionDenied {
                                        session_id: session.id.clone(),
                                        tool: call.name.clone(),
                                        reason: reason.clone(),
                                    },
                                )
                                .await;

                                format!("Permission denied: {}", reason)
                            }
                        };

                        session.messages.push(AgentMessage {
                            role: MessageRole::Tool,
                            content: result_content,
                            tool_calls: None,
                            tool_call_id: Some(call.id.clone()),
                        });
                    }

                    // Loop — model will see tool results on next iteration
                    continue;
                }
            }

            // No tool calls — this is the final text response
            let content = chat_resp.message.content.clone();
            session.messages.push(AgentMessage {
                role: MessageRole::Assistant,
                content: content.clone(),
                tool_calls: None,
                tool_call_id: None,
            });
            session.status = SessionStatus::Active;
            session.updated_at = chrono::Utc::now().timestamp();

            Self::emit(
                &events,
                AgentEvent::Message {
                    session_id: session.id.clone(),
                    content: content.clone(),
                },
            )
            .await;

            return Ok(content);
        }

        // Exhausted tool rounds
        session.status = SessionStatus::Active;
        session.updated_at = chrono::Utc::now().timestamp();
        let err_msg = format!("Maximum tool rounds ({}) exceeded", self.max_tool_rounds);
        Self::emit(
            &events,
            AgentEvent::Error {
                session_id: session.id.clone(),
                error: err_msg.clone(),
            },
        )
        .await;
        Err(anyhow::anyhow!(err_msg))
    }
}
