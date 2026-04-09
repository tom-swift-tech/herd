use anyhow::Result;

use super::ProviderAdapter;

pub struct OpenAICompatAdapter;

impl ProviderAdapter for OpenAICompatAdapter {
    fn transform_request(&self, body: &serde_json::Value) -> Result<serde_json::Value> {
        Ok(body.clone())
    }

    fn transform_response(&self, body: &serde_json::Value) -> Result<serde_json::Value> {
        Ok(body.clone())
    }

    fn transform_stream_chunk(&self, chunk: &str) -> Result<String> {
        Ok(chunk.to_string())
    }

    fn extract_usage(&self, body: &serde_json::Value) -> Option<(u64, u64)> {
        let usage = body.get("usage")?;
        let prompt = usage.get("prompt_tokens")?.as_u64()?;
        let completion = usage.get("completion_tokens")?.as_u64()?;
        Some((prompt, completion))
    }

    fn auth_header(&self, api_key: &str) -> String {
        format!("Bearer {}", api_key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn passthrough_request() {
        let adapter = OpenAICompatAdapter;
        let body = json!({"model": "gpt-4o", "messages": [{"role": "user", "content": "hi"}]});
        let result = adapter.transform_request(&body).unwrap();
        assert_eq!(result, body);
    }

    #[test]
    fn passthrough_response() {
        let adapter = OpenAICompatAdapter;
        let body = json!({"id": "chatcmpl-123", "choices": [{"message": {"content": "hello"}}]});
        let result = adapter.transform_response(&body).unwrap();
        assert_eq!(result, body);
    }

    #[test]
    fn extract_usage() {
        let adapter = OpenAICompatAdapter;
        let body = json!({"usage": {"prompt_tokens": 10, "completion_tokens": 25}});
        let usage = adapter.extract_usage(&body);
        assert_eq!(usage, Some((10, 25)));

        let missing = adapter.extract_usage(&json!({}));
        assert_eq!(missing, None);
    }

    #[test]
    fn auth_header() {
        let adapter = OpenAICompatAdapter;
        assert_eq!(adapter.auth_header("sk-test-key"), "Bearer sk-test-key");
    }
}
