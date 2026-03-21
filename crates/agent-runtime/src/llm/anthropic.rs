//! Anthropic Claude API client implementation.
//!
//! Uses the Messages API with native tool use / tool result support.
//! Model is configurable; defaults to claude-sonnet-4-6.
//!
//! # Content block handling
//!
//! Messages are serialized with structured content blocks:
//! - `Text` → `{"type": "text", "text": "..."}`
//! - `ToolUse` → `{"type": "tool_use", "id": "...", "name": "...", "input": {...}}`
//! - `ToolResult` → `{"type": "tool_result", "tool_use_id": "...", "content": "..."}`
//!
//! This ensures the Anthropic API receives native tool_result blocks
//! rather than plain text disguised as user messages.

use async_trait::async_trait;
use reqwest::Client;
use serde_json::{json, Value};
use tracing::{debug, instrument};

use claw_types::tool::ToolSpec;

use super::{ContentBlock, LlmClient, LlmMessage, LlmResponse, LlmToolCall};
use crate::errors::AgentError;

const DEFAULT_MODEL: &str = "claude-sonnet-4-6";
const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";
const MAX_TOKENS: u32 = 8192;

/// Anthropic Claude API client.
#[derive(Clone)]
pub struct AnthropicClient {
    http:    Client,
    api_key: String,
    model:   String,
}

impl AnthropicClient {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            http:    Client::new(),
            api_key: api_key.into(),
            model:   DEFAULT_MODEL.to_string(),
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }
}

/// Serializes a `ContentBlock` to its Anthropic JSON representation.
fn content_block_to_json(block: &ContentBlock) -> Value {
    match block {
        ContentBlock::Text { text } => json!({
            "type": "text",
            "text": text,
        }),
        ContentBlock::ToolUse { id, name, input } => json!({
            "type": "tool_use",
            "id": id,
            "name": name,
            "input": input,
        }),
        ContentBlock::ToolResult { tool_use_id, content } => json!({
            "type": "tool_result",
            "tool_use_id": tool_use_id,
            "content": content,
        }),
    }
}

/// Converts an `LlmMessage` to the Anthropic API message format.
///
/// Content blocks are serialized as a JSON array, preserving
/// the native tool_use / tool_result structure.
fn message_to_api(msg: &LlmMessage) -> Value {
    let content: Vec<Value> = msg.content.iter().map(content_block_to_json).collect();
    json!({
        "role": msg.role,
        "content": content,
    })
}

#[async_trait]
impl LlmClient for AnthropicClient {
    #[instrument(skip(self, messages, tools), fields(model = %self.model, messages = messages.len()))]
    async fn complete(
        &self,
        system: &str,
        messages: &[LlmMessage],
        tools: &[ToolSpec],
    ) -> Result<LlmResponse, AgentError> {
        // Convert messages with structured content blocks
        let api_messages: Vec<Value> = messages.iter().map(message_to_api).collect();

        // Convert tool specs to Anthropic tool definitions
        let api_tools: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema
                })
            })
            .collect();

        let mut body = json!({
            "model": self.model,
            "max_tokens": MAX_TOKENS,
            "system": system,
            "messages": api_messages
        });

        if !api_tools.is_empty() {
            body["tools"] = json!(api_tools);
        }

        debug!(model = %self.model, "sending request to Anthropic API");

        let response = self
            .http
            .post(ANTHROPIC_API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| AgentError::Llm(e.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            let err_text = response.text().await.unwrap_or_default();
            return Err(AgentError::Llm(format!("API error {status}: {err_text}")));
        }

        let resp_json: Value = response
            .json()
            .await
            .map_err(|e| AgentError::Llm(e.to_string()))?;

        parse_response(resp_json)
    }
}

fn parse_response(resp: Value) -> Result<LlmResponse, AgentError> {
    let stop_reason = resp["stop_reason"]
        .as_str()
        .unwrap_or("end_turn")
        .to_string();

    let input_tokens = resp["usage"]["input_tokens"].as_u64().unwrap_or(0) as u32;
    let output_tokens = resp["usage"]["output_tokens"].as_u64().unwrap_or(0) as u32;

    let content = resp["content"].as_array().cloned().unwrap_or_default();

    let mut text_parts = Vec::new();
    let mut tool_calls = Vec::new();

    for block in &content {
        match block["type"].as_str() {
            Some("text") => {
                if let Some(t) = block["text"].as_str() {
                    text_parts.push(t.to_string());
                }
            }
            Some("tool_use") => {
                tool_calls.push(LlmToolCall {
                    id:        block["id"].as_str().unwrap_or("").to_string(),
                    tool_name: block["name"].as_str().unwrap_or("").to_string(),
                    input:     block["input"].clone(),
                });
            }
            _ => {}
        }
    }

    Ok(LlmResponse {
        text: if text_parts.is_empty() {
            None
        } else {
            Some(text_parts.join("\n"))
        },
        tool_calls,
        stop_reason,
        input_tokens,
        output_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_block_serializes_correctly() {
        let block = ContentBlock::Text { text: "hello".into() };
        let json = content_block_to_json(&block);
        assert_eq!(json["type"], "text");
        assert_eq!(json["text"], "hello");
    }

    #[test]
    fn tool_use_block_serializes_correctly() {
        let block = ContentBlock::ToolUse {
            id: "tc_1".into(),
            name: "get_balance".into(),
            input: json!({"wallet": "abc"}),
        };
        let json = content_block_to_json(&block);
        assert_eq!(json["type"], "tool_use");
        assert_eq!(json["id"], "tc_1");
        assert_eq!(json["name"], "get_balance");
        assert_eq!(json["input"]["wallet"], "abc");
    }

    #[test]
    fn tool_result_block_serializes_correctly() {
        let block = ContentBlock::ToolResult {
            tool_use_id: "tc_1".into(),
            content: "balance: 100 SOL".into(),
        };
        let json = content_block_to_json(&block);
        assert_eq!(json["type"], "tool_result");
        assert_eq!(json["tool_use_id"], "tc_1");
        assert_eq!(json["content"], "balance: 100 SOL");
    }

    #[test]
    fn message_with_tool_results_serializes_as_array() {
        let msg = LlmMessage::tool_results(vec![
            ContentBlock::ToolResult {
                tool_use_id: "tc_1".into(),
                content: "result 1".into(),
            },
            ContentBlock::ToolResult {
                tool_use_id: "tc_2".into(),
                content: "result 2".into(),
            },
        ]);
        let json = message_to_api(&msg);
        assert_eq!(json["role"], "user");
        let content = json["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "tool_result");
        assert_eq!(content[1]["type"], "tool_result");
    }

    #[test]
    fn parse_response_extracts_tool_calls() {
        let resp = json!({
            "content": [
                {"type": "text", "text": "I'll check your balance."},
                {"type": "tool_use", "id": "tc_1", "name": "get_balance", "input": {"wallet": "abc"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 100, "output_tokens": 50}
        });
        let result = parse_response(resp).unwrap();
        assert_eq!(result.text.unwrap(), "I'll check your balance.");
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].id, "tc_1");
        assert_eq!(result.tool_calls[0].tool_name, "get_balance");
        assert_eq!(result.stop_reason, "tool_use");
    }
}
