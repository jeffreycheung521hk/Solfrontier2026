//! The `Tool` trait — the interface every tool implements.

use async_trait::async_trait;
use serde_json::Value;

use claw_types::tool::{ToolInput, ToolOutput, ToolSpec};

use crate::errors::ToolError;

/// Every tool in the registry implements this trait.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Returns the static specification for this tool.
    fn spec(&self) -> ToolSpec;

    /// Executes the tool with the given input.
    async fn execute(&self, input: ToolInput) -> Result<ToolOutput, ToolError>;
}
