//! Transaction history tools.

use async_trait::async_trait;
use serde_json::{json, Value};

use claw_solana_core::rpc::ClawRpcClient;
use claw_types::tool::{ToolInput, ToolOutput, ToolSpec};

use crate::{errors::ToolError, tool::Tool};

/// `get_recent_transactions` — returns recent transaction signatures for a wallet.
pub struct GetRecentTransactionsTool {
    rpc: ClawRpcClient,
}

impl GetRecentTransactionsTool {
    pub fn new(rpc: ClawRpcClient) -> Self {
        Self { rpc }
    }
}

#[async_trait]
impl Tool for GetRecentTransactionsTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "get_recent_transactions".to_string(),
            description: "Returns the most recent transaction signatures for a given wallet address. Returns signature, slot, block time, and error status.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["address"],
                "properties": {
                    "address": { "type": "string" },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 50,
                        "default": 10
                    }
                }
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "address": { "type": "string" },
                    "transactions": { "type": "array" }
                }
            }),
            required_capabilities: vec!["read_chain".to_string()],
            supports_streaming: false,
            timeout_ms: 15_000,
        }
    }

    async fn execute(&self, input: ToolInput) -> Result<ToolOutput, ToolError> {
        let address = input.parameters["address"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput { reason: "address required".to_string() })?;

        let limit = input.parameters
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(10) as usize;

        let sigs = self
            .rpc
            .get_signatures_for_address(address, limit, None)
            .await
            .map_err(ToolError::Solana)?;

        let txs: Vec<Value> = sigs
            .iter()
            .map(|s| {
                json!({
                    "signature":  s.signature,
                    "slot":       s.slot,
                    "block_time": s.block_time,
                    "err":        s.err.as_ref().map(|e| format!("{:?}", e)),
                    "memo":       s.memo
                })
            })
            .collect();

        Ok(ToolOutput {
            tool_name: "get_recent_transactions".to_string(),
            success: true,
            data: Some(json!({
                "address": address,
                "count": txs.len(),
                "transactions": txs
            })),
            error: None,
            duration_ms: 0,
        })
    }
}
