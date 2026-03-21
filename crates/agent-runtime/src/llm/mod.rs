//! LLM client abstraction.
//!
//! All LLM calls go through the `LlmClient` trait. This isolates the
//! provider from the agent runtime and enables:
//! - Swapping providers (Anthropic, OpenAI, local) without changing agent code
//! - Mock implementations for testing
//! - Instrumented wrappers for latency/token tracking
//!
//! # Content blocks
//!
//! Messages carry structured content via `ContentBlock`. This enables
//! Anthropic-native `tool_use` and `tool_result` blocks, providing a
//! cleaner semantic separation between user text, tool invocations,
//! and untrusted tool output than flat string prefixes.

pub mod anthropic;
pub mod context;
pub mod openai;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

use claw_types::tool::ToolSpec;

use crate::errors::AgentError;

/// A structured content block within an LLM message.
///
/// Maps directly to Anthropic's content block types. Other providers
/// can be adapted from this representation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    /// Plain text content.
    #[serde(rename = "text")]
    Text { text: String },

    /// A tool invocation requested by the assistant.
    #[serde(rename = "tool_use")]
    ToolUse {
        id:    String,
        name:  String,
        input: Value,
    },

    /// The result of a tool invocation — **untrusted external data**.
    ///
    /// # Security
    ///
    /// This content comes from on-chain data, RPC responses, or external
    /// APIs. The structural separation from `Text` blocks ensures the
    /// provider's API treats it as tool output, not user instructions.
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content:     String,
    },
}

/// A message in the LLM conversation history.
///
/// Content is represented as structured `ContentBlock`s to support
/// native tool_use / tool_result blocks. The `content_text()` helper
/// provides backward-compatible access to plain text content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmMessage {
    pub role:    String,  // "user", "assistant"
    pub content: Vec<ContentBlock>,
}

impl LlmMessage {
    /// Creates a message with a single text block.
    pub fn text(role: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    /// Creates a user message containing one or more tool_result blocks.
    pub fn tool_results(blocks: Vec<ContentBlock>) -> Self {
        Self {
            role: "user".to_string(),
            content: blocks,
        }
    }

    /// Creates an assistant message containing tool_use blocks (and optional text).
    pub fn assistant_with_tool_use(text: Option<String>, tool_calls: &[LlmToolCall]) -> Self {
        let mut blocks = Vec::new();
        if let Some(t) = text {
            blocks.push(ContentBlock::Text { text: t });
        }
        for tc in tool_calls {
            blocks.push(ContentBlock::ToolUse {
                id:    tc.id.clone(),
                name:  tc.tool_name.clone(),
                input: tc.input.clone(),
            });
        }
        Self {
            role: "assistant".to_string(),
            content: blocks,
        }
    }

    /// Extracts concatenated text content (ignoring non-text blocks).
    ///
    /// Used for logging, metrics, and backward-compatible access.
    pub fn content_text(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Returns true if this message contains any tool_result blocks.
    pub fn has_tool_results(&self) -> bool {
        self.content.iter().any(|b| matches!(b, ContentBlock::ToolResult { .. }))
    }

    /// Returns true if this message contains any tool_use blocks.
    pub fn has_tool_use(&self) -> bool {
        self.content.iter().any(|b| matches!(b, ContentBlock::ToolUse { .. }))
    }
}

/// A tool call requested by the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmToolCall {
    pub id:         String,
    pub tool_name:  String,
    pub input:      Value,
}

/// The response from the LLM.
#[derive(Debug, Clone)]
pub struct LlmResponse {
    pub text:       Option<String>,
    pub tool_calls: Vec<LlmToolCall>,
    pub stop_reason: String,
    pub input_tokens: u32,
    pub output_tokens: u32,
}

/// The LLM client trait.
#[async_trait]
pub trait LlmClient: Send + Sync {
    /// Sends a conversation to the LLM and returns its response.
    async fn complete(
        &self,
        system: &str,
        messages: &[LlmMessage],
        tools: &[ToolSpec],
    ) -> Result<LlmResponse, AgentError>;
}

/// A reference-counted LLM client.
pub type LlmClientRef = Arc<dyn LlmClient>;
