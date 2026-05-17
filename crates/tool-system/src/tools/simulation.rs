//! Transaction simulation tool.

use async_trait::async_trait;
use base64::Engine;
use serde_json::json;
use solana_sdk::transaction::Transaction;

use claw_solana_core::SimulationClient;
use claw_types::tool::{ToolInput, ToolOutput, ToolSpec};

use crate::{errors::ToolError, tool::Tool};

/// `simulate_transaction` — simulates a base64-encoded transaction.
pub struct SimulateTransactionTool {
    sim: SimulationClient,
}

impl SimulateTransactionTool {
    pub fn new(sim: SimulationClient) -> Self {
        Self { sim }
    }
}

#[async_trait]
impl Tool for SimulateTransactionTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "simulate_transaction".to_string(),
            description: "Simulates a base64-encoded Solana transaction and returns compute units used, logs, and whether it would succeed. Does NOT sign or send.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["transaction_b64"],
                "properties": {
                    "transaction_b64": {
                        "type": "string",
                        "description": "Base64-encoded serialized Solana transaction"
                    }
                }
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "success":            { "type": "boolean" },
                    "error":              { "type": ["string", "null"] },
                    "compute_units_used": { "type": ["integer", "null"] },
                    "logs":               { "type": "array" }
                }
            }),
            required_capabilities: vec!["simulate_transaction".to_string()],
            supports_streaming: false,
            timeout_ms: 20_000,
        }
    }

    async fn execute(&self, input: ToolInput) -> Result<ToolOutput, ToolError> {
        let tx_b64 = input.parameters["transaction_b64"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput {
                reason: "transaction_b64 required".to_string(),
            })?;

        let tx_bytes = base64::engine::general_purpose::STANDARD
            .decode(tx_b64)
            .map_err(|e| ToolError::InvalidInput {
                reason: format!("invalid base64: {e}"),
            })?;

        let tx: Transaction = bincode::deserialize(&tx_bytes)
            .map_err(|e| ToolError::InvalidInput {
                reason: format!("invalid transaction bytes: {e}"),
            })?;

        let output = self
            .sim
            .simulate(&tx)
            .await
            .map_err(ToolError::Solana)?;

        Ok(ToolOutput {
            tool_name: "simulate_transaction".to_string(),
            success: true,
            data: Some(json!({
                "success":            output.success,
                "error":              output.error,
                "compute_units_used": output.compute_units_used,
                "logs":               output.logs,
                "return_data":        output.return_data
            })),
            error: None,
            duration_ms: 0,
        })
    }
}
