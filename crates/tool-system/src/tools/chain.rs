//! Chain state tools.

use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::instrument;

use claw_solana_core::rpc::ClawRpcClient;
use claw_types::tool::{ToolInput, ToolOutput, ToolSpec};

use crate::{errors::ToolError, tool::Tool};

/// `get_account_info` — fetches raw account data for an address.
pub struct GetAccountInfoTool {
    rpc: ClawRpcClient,
}

impl GetAccountInfoTool {
    pub fn new(rpc: ClawRpcClient) -> Self {
        Self { rpc }
    }
}

#[async_trait]
impl Tool for GetAccountInfoTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "get_account_info".to_string(),
            description: "Fetches the account information for a given Solana address, including lamport balance, owner program, and whether the account is executable.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["pubkey"],
                "properties": {
                    "pubkey": { "type": "string" }
                }
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "pubkey":      { "type": "string" },
                    "lamports":    { "type": "integer" },
                    "sol":         { "type": "number" },
                    "owner":       { "type": "string" },
                    "executable":  { "type": "boolean" },
                    "data_len":    { "type": "integer" }
                }
            }),
            required_capabilities: vec!["read_chain".to_string()],
            supports_streaming: false,
            timeout_ms: 10_000,
        }
    }

    #[instrument(skip(self), fields(tool = "get_account_info"))]
    async fn execute(&self, input: ToolInput) -> Result<ToolOutput, ToolError> {
        let pubkey = input.parameters["pubkey"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput { reason: "pubkey required".to_string() })?;

        let account = self.rpc.get_account(pubkey, None).await.map_err(ToolError::Solana)?;

        Ok(ToolOutput {
            tool_name: "get_account_info".to_string(),
            success: true,
            data: Some(json!({
                "pubkey":     pubkey,
                "lamports":   account.lamports,
                "sol":        account.lamports as f64 / 1e9,
                "owner":      account.owner.to_string(),
                "executable": account.executable,
                "data_len":   account.data.len()
            })),
            error: None,
            duration_ms: 0,
        })
    }
}

/// `get_slot` — returns the current confirmed slot.
pub struct GetSlotTool {
    rpc: ClawRpcClient,
}

impl GetSlotTool {
    pub fn new(rpc: ClawRpcClient) -> Self {
        Self { rpc }
    }
}

#[async_trait]
impl Tool for GetSlotTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "get_slot".to_string(),
            description: "Returns the current Solana slot number at the confirmed commitment level.".to_string(),
            input_schema: json!({ "type": "object", "properties": {} }),
            output_schema: json!({
                "type": "object",
                "properties": { "slot": { "type": "integer" } }
            }),
            required_capabilities: vec!["read_chain".to_string()],
            supports_streaming: false,
            timeout_ms: 5_000,
        }
    }

    async fn execute(&self, _input: ToolInput) -> Result<ToolOutput, ToolError> {
        let slot = self.rpc.get_slot(None).await.map_err(ToolError::Solana)?;

        Ok(ToolOutput {
            tool_name: "get_slot".to_string(),
            success: true,
            data: Some(json!({ "slot": slot })),
            error: None,
            duration_ms: 0,
        })
    }
}
