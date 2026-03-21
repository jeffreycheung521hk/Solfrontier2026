//! OpenAI-compatible API client implementation.
//!
//! Uses the Chat Completions API with function calling support.
//! Compatible with OpenAI GPT-4o and similar models.
//!
//! # Content block mapping
//!
//! ClawSolana's internal `ContentBlock` types are mapped to OpenAI's format:
//! - `Text` → message content string
//! - `ToolUse` → `tool_calls` array on assistant messages
//! - `ToolResult` → `role: "tool"` messages with `tool_call_id`

use async_trait::async_trait;
use reqwest::Client;
use serde_json::{json, Value};
use tracing::{debug, instrument};

use claw_types::tool::ToolSpec;

use super::{ContentBlock, LlmClient, LlmMessage, LlmResponse, LlmToolCall};
use crate::errors::AgentError;

const DEFAULT_MODEL: &str = "gpt-4o";
const OPENAI_API_URL: &str = "https://api.openai.com/v1/chat/completions";
const MAX_TOKENS: u32 = 4096;

/// OpenAI-compatible API client.
#[derive(Clone)]
pub struct OpenAiClient {
    http:    Client,
    api_key: String,
    model:   String,
}

impl OpenAiClient {
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

/// Convert an LlmMessage to OpenAI chat completion message format.
///
/// Key differences from Anthropic:
/// - tool_result blocks become separate messages with `role: "tool"`
/// - tool_use blocks become `tool_calls` on assistant messages
/// - text blocks become `content` string
fn messages_to_openai(messages: &[LlmMessage]) -> Vec<Value> {
    let mut result = Vec::new();

    for msg in messages {
        if msg.has_tool_results() {
            // Each tool_result becomes a separate "tool" role message
            for block in &msg.content {
                match block {
                    ContentBlock::ToolResult { tool_use_id, content } => {
                        result.push(json!({
                            "role": "tool",
                            "tool_call_id": tool_use_id,
                            "content": content,
                        }));
                    }
                    ContentBlock::Text { text } => {
                        // Rare: text alongside tool results; emit as user message
                        result.push(json!({
                            "role": "user",
                            "content": text,
                        }));
                    }
                    _ => {}
                }
            }
        } else if msg.has_tool_use() {
            // Assistant message with tool calls
            let mut text_parts = Vec::new();
            let mut tool_calls = Vec::new();

            for block in &msg.content {
                match block {
                    ContentBlock::Text { text } => text_parts.push(text.clone()),
                    ContentBlock::ToolUse { id, name, input } => {
                        tool_calls.push(json!({
                            "id": id,
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": input.to_string(),
                            }
                        }));
                    }
                    _ => {}
                }
            }

            let content = if text_parts.is_empty() {
                Value::Null
            } else {
                Value::String(text_parts.join("\n"))
            };

            result.push(json!({
                "role": "assistant",
                "content": content,
                "tool_calls": tool_calls,
            }));
        } else {
            // Plain text message
            let text = msg.content_text();
            result.push(json!({
                "role": msg.role,
                "content": text,
            }));
        }
    }

    result
}

/// Convert ToolSpec to OpenAI function tool format.
fn tools_to_openai(tools: &[ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.input_schema,
                }
            })
        })
        .collect()
}

#[async_trait]
impl LlmClient for OpenAiClient {
    #[instrument(skip(self, messages, tools), fields(model = %self.model, messages = messages.len()))]
    async fn complete(
        &self,
        system: &str,
        messages: &[LlmMessage],
        tools: &[ToolSpec],
    ) -> Result<LlmResponse, AgentError> {
        // Build messages array with system message first
        let mut api_messages = vec![json!({
            "role": "system",
            "content": system,
        })];
        api_messages.extend(messages_to_openai(messages));

        let api_tools = tools_to_openai(tools);

        let mut body = json!({
            "model": self.model,
            "max_tokens": MAX_TOKENS,
            "messages": api_messages,
        });

        if !api_tools.is_empty() {
            body["tools"] = json!(api_tools);
        }

        debug!(model = %self.model, "sending request to OpenAI API");

        let response = self
            .http
            .post(OPENAI_API_URL)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| AgentError::Llm(e.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            let err_text = response.text().await.unwrap_or_default();
            return Err(AgentError::Llm(format!("OpenAI API error {status}: {err_text}")));
        }

        let resp_json: Value = response
            .json()
            .await
            .map_err(|e| AgentError::Llm(e.to_string()))?;

        parse_openai_response(resp_json)
    }
}

fn parse_openai_response(resp: Value) -> Result<LlmResponse, AgentError> {
    let choice = resp["choices"]
        .get(0)
        .ok_or_else(|| AgentError::Llm("no choices in response".to_string()))?;

    let message = &choice["message"];
    let finish_reason = choice["finish_reason"]
        .as_str()
        .unwrap_or("stop")
        .to_string();

    // Map OpenAI finish_reason to Anthropic-compatible stop_reason
    let stop_reason = match finish_reason.as_str() {
        "tool_calls" => "tool_use".to_string(),
        "stop" => "end_turn".to_string(),
        "length" => "max_tokens".to_string(),
        other => other.to_string(),
    };

    let input_tokens = resp["usage"]["prompt_tokens"].as_u64().unwrap_or(0) as u32;
    let output_tokens = resp["usage"]["completion_tokens"].as_u64().unwrap_or(0) as u32;

    let text = message["content"]
        .as_str()
        .map(|s| s.to_string());

    let mut tool_calls = Vec::new();
    if let Some(calls) = message["tool_calls"].as_array() {
        for call in calls {
            let id = call["id"].as_str().unwrap_or("").to_string();
            let name = call["function"]["name"].as_str().unwrap_or("").to_string();
            let args_str = call["function"]["arguments"].as_str().unwrap_or("{}");
            let input: Value = serde_json::from_str(args_str).unwrap_or(json!({}));

            tool_calls.push(LlmToolCall {
                id,
                tool_name: name,
                input,
            });
        }
    }

    Ok(LlmResponse {
        text,
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
    fn text_message_converts_correctly() {
        let msgs = vec![LlmMessage::text("user", "hello")];
        let api = messages_to_openai(&msgs);
        assert_eq!(api.len(), 1);
        assert_eq!(api[0]["role"], "user");
        assert_eq!(api[0]["content"], "hello");
    }

    #[test]
    fn tool_use_message_converts_to_tool_calls() {
        let tool_calls = vec![LlmToolCall {
            id: "tc_1".into(),
            tool_name: "get_balance".into(),
            input: json!({"wallet": "abc"}),
        }];
        let msgs = vec![LlmMessage::assistant_with_tool_use(
            Some("Let me check.".into()),
            &tool_calls,
        )];
        let api = messages_to_openai(&msgs);
        assert_eq!(api.len(), 1);
        assert_eq!(api[0]["role"], "assistant");
        assert_eq!(api[0]["content"], "Let me check.");
        let calls = api[0]["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "get_balance");
    }

    #[test]
    fn tool_result_converts_to_tool_role() {
        let msgs = vec![LlmMessage::tool_results(vec![
            ContentBlock::ToolResult {
                tool_use_id: "tc_1".into(),
                content: "balance: 100".into(),
            },
        ])];
        let api = messages_to_openai(&msgs);
        assert_eq!(api.len(), 1);
        assert_eq!(api[0]["role"], "tool");
        assert_eq!(api[0]["tool_call_id"], "tc_1");
        assert_eq!(api[0]["content"], "balance: 100");
    }

    #[test]
    fn tool_spec_converts_to_function() {
        let specs = vec![ToolSpec {
            name: "get_balance".into(),
            description: "Get wallet balance".into(),
            input_schema: json!({"type": "object", "properties": {"wallet": {"type": "string"}}}),
            output_schema: json!({}),
            required_capabilities: vec![],
            supports_streaming: false,
            timeout_ms: 30000,
        }];
        let api = tools_to_openai(&specs);
        assert_eq!(api.len(), 1);
        assert_eq!(api[0]["type"], "function");
        assert_eq!(api[0]["function"]["name"], "get_balance");
    }

    #[test]
    fn parse_openai_response_with_tool_calls() {
        let resp = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_abc",
                        "type": "function",
                        "function": {
                            "name": "get_balance",
                            "arguments": "{\"wallet\":\"abc\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 100, "completion_tokens": 50}
        });
        let result = parse_openai_response(resp).unwrap();
        assert!(result.text.is_none());
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].tool_name, "get_balance");
        assert_eq!(result.stop_reason, "tool_use"); // mapped
    }

    #[test]
    fn parse_openai_response_text_only() {
        let resp = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "Your balance is 100 SOL."
                },
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 50, "completion_tokens": 20}
        });
        let result = parse_openai_response(resp).unwrap();
        assert_eq!(result.text.unwrap(), "Your balance is 100 SOL.");
        assert!(result.tool_calls.is_empty());
        assert_eq!(result.stop_reason, "end_turn"); // mapped
    }
}
