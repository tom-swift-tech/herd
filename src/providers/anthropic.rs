use anyhow::Result;
use serde_json::{json, Value};

use super::ProviderAdapter;

pub struct AnthropicAdapter;

impl ProviderAdapter for AnthropicAdapter {
    fn transform_request(&self, body: &Value) -> Result<Value> {
        let mut out = json!({});

        // Extract system messages from the messages array into Anthropic's top-level `system` field
        if let Some(messages) = body.get("messages").and_then(|m| m.as_array()) {
            let mut system_parts: Vec<String> = Vec::new();
            let mut non_system: Vec<Value> = Vec::new();

            for msg in messages {
                if msg.get("role").and_then(|r| r.as_str()) == Some("system") {
                    if let Some(content) = msg.get("content").and_then(|c| c.as_str()) {
                        system_parts.push(content.to_string());
                    }
                } else {
                    non_system.push(msg.clone());
                }
            }

            if !system_parts.is_empty() {
                out["system"] = Value::String(system_parts.join("\n"));
            }

            out["messages"] = Value::Array(non_system);
        }

        // model passthrough
        if let Some(model) = body.get("model") {
            out["model"] = model.clone();
        }

        // max_tokens is REQUIRED by Anthropic — default to 4096 if missing
        let max_tokens = body
            .get("max_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(4096);
        out["max_tokens"] = json!(max_tokens);

        // Optional passthrough fields
        if let Some(temp) = body.get("temperature") {
            out["temperature"] = temp.clone();
        }
        if let Some(top_p) = body.get("top_p") {
            out["top_p"] = top_p.clone();
        }
        if let Some(stream) = body.get("stream") {
            out["stream"] = stream.clone();
        }

        Ok(out)
    }

    fn transform_response(&self, body: &Value) -> Result<Value> {
        // Join all text content blocks
        let content = body
            .get("content")
            .and_then(|c| c.as_array())
            .map(|blocks| {
                blocks
                    .iter()
                    .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                    .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default();

        // Map usage fields
        let mut usage = json!({});
        if let Some(u) = body.get("usage") {
            if let Some(input) = u.get("input_tokens") {
                usage["prompt_tokens"] = input.clone();
            }
            if let Some(output) = u.get("output_tokens") {
                usage["completion_tokens"] = output.clone();
            }
        }

        let model = body.get("model").cloned().unwrap_or(json!("unknown"));
        let id = body
            .get("id")
            .cloned()
            .unwrap_or(json!("chatcmpl-anthropic"));

        Ok(json!({
            "id": id,
            "object": "chat.completion",
            "model": model,
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": content,
                },
                "finish_reason": "stop",
            }],
            "usage": usage,
        }))
    }

    fn transform_stream_chunk(&self, chunk: &str) -> Result<String> {
        let parsed: Value = serde_json::from_str(chunk)?;
        let event_type = parsed.get("type").and_then(|t| t.as_str()).unwrap_or("");

        match event_type {
            "content_block_delta" => {
                let text = parsed
                    .get("delta")
                    .and_then(|d| d.get("text"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("");

                let openai_chunk = json!({
                    "object": "chat.completion.chunk",
                    "choices": [{
                        "index": 0,
                        "delta": {
                            "content": text,
                        },
                        "finish_reason": null,
                    }],
                });

                Ok(format!(
                    "data: {}\n\n",
                    serde_json::to_string(&openai_chunk)?
                ))
            }
            "message_stop" => {
                let openai_chunk = json!({
                    "object": "chat.completion.chunk",
                    "choices": [{
                        "index": 0,
                        "delta": {},
                        "finish_reason": "stop",
                    }],
                });

                Ok(format!(
                    "data: {}\n\n",
                    serde_json::to_string(&openai_chunk)?
                ))
            }
            _ => Ok(String::new()),
        }
    }

    fn extract_usage(&self, body: &Value) -> Option<(u64, u64)> {
        let usage = body.get("usage")?;
        let input = usage.get("input_tokens")?.as_u64()?;
        let output = usage.get("output_tokens")?.as_u64()?;
        Some((input, output))
    }

    fn auth_header(&self, api_key: &str) -> String {
        api_key.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transform_request_converts_format() {
        let adapter = AnthropicAdapter;
        let openai_req = json!({
            "model": "claude-sonnet-4-20250514",
            "messages": [
                {"role": "system", "content": "You are a helpful assistant."},
                {"role": "system", "content": "Be concise."},
                {"role": "user", "content": "Hello"},
            ],
            "temperature": 0.7,
            "stream": false,
        });

        let result = adapter.transform_request(&openai_req).unwrap();

        // System messages extracted and joined
        assert_eq!(
            result["system"],
            "You are a helpful assistant.\nBe concise."
        );

        // Only non-system messages remain
        let messages = result["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"], "Hello");

        // max_tokens defaulted to 4096
        assert_eq!(result["max_tokens"], 4096);

        // passthrough fields
        assert_eq!(result["temperature"], 0.7);
        assert_eq!(result["stream"], false);
        assert_eq!(result["model"], "claude-sonnet-4-20250514");
    }

    #[test]
    fn transform_request_adds_default_max_tokens() {
        let adapter = AnthropicAdapter;

        // Without max_tokens
        let req_no_max = json!({
            "model": "claude-sonnet-4-20250514",
            "messages": [{"role": "user", "content": "Hi"}],
        });
        let result = adapter.transform_request(&req_no_max).unwrap();
        assert_eq!(result["max_tokens"], 4096);

        // With explicit max_tokens
        let req_with_max = json!({
            "model": "claude-sonnet-4-20250514",
            "messages": [{"role": "user", "content": "Hi"}],
            "max_tokens": 1024,
        });
        let result = adapter.transform_request(&req_with_max).unwrap();
        assert_eq!(result["max_tokens"], 1024);
    }

    #[test]
    fn transform_response_to_openai_format() {
        let adapter = AnthropicAdapter;
        let anthropic_resp = json!({
            "id": "msg_123",
            "type": "message",
            "model": "claude-sonnet-4-20250514",
            "content": [
                {"type": "text", "text": "Hello, "},
                {"type": "text", "text": "world!"},
            ],
            "usage": {
                "input_tokens": 100,
                "output_tokens": 50,
            },
        });

        let result = adapter.transform_response(&anthropic_resp).unwrap();

        assert_eq!(result["object"], "chat.completion");
        assert_eq!(result["id"], "msg_123");
        assert_eq!(result["model"], "claude-sonnet-4-20250514");
        assert_eq!(result["choices"][0]["message"]["content"], "Hello, world!");
        assert_eq!(result["choices"][0]["message"]["role"], "assistant");
        assert_eq!(result["choices"][0]["finish_reason"], "stop");
        assert_eq!(result["usage"]["prompt_tokens"], 100);
        assert_eq!(result["usage"]["completion_tokens"], 50);
    }

    #[test]
    fn extract_usage_from_anthropic_response() {
        let adapter = AnthropicAdapter;
        let body = json!({
            "usage": {
                "input_tokens": 250,
                "output_tokens": 120,
            }
        });

        let (input, output) = adapter.extract_usage(&body).unwrap();
        assert_eq!(input, 250);
        assert_eq!(output, 120);
    }

    #[test]
    fn auth_header_returns_raw_key() {
        let adapter = AnthropicAdapter;
        assert_eq!(adapter.auth_header("sk-ant-123456"), "sk-ant-123456");
    }

    #[test]
    fn transform_stream_chunk_content_delta() {
        let adapter = AnthropicAdapter;

        // content_block_delta → OpenAI delta with content
        let delta_chunk = json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": "Hello"},
        });
        let result = adapter
            .transform_stream_chunk(&serde_json::to_string(&delta_chunk).unwrap())
            .unwrap();
        let parsed: Value =
            serde_json::from_str(result.trim_start_matches("data: ").trim()).unwrap();
        assert_eq!(parsed["choices"][0]["delta"]["content"], "Hello");
        assert!(parsed["choices"][0]["finish_reason"].is_null());

        // message_stop → OpenAI finish_reason: "stop"
        let stop_chunk = json!({"type": "message_stop"});
        let result = adapter
            .transform_stream_chunk(&serde_json::to_string(&stop_chunk).unwrap())
            .unwrap();
        let parsed: Value =
            serde_json::from_str(result.trim_start_matches("data: ").trim()).unwrap();
        assert_eq!(parsed["choices"][0]["finish_reason"], "stop");

        // Other event types → empty string
        let ping_chunk = json!({"type": "ping"});
        let result = adapter
            .transform_stream_chunk(&serde_json::to_string(&ping_chunk).unwrap())
            .unwrap();
        assert_eq!(result, "");
    }
}
