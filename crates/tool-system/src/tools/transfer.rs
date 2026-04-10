//! SOL transfer transaction builder tool.

use async_trait::async_trait;
use base64::Engine;
use serde_json::json;
use solana_sdk::{
    message::Message,
    pubkey::Pubkey,
    system_instruction,
    transaction::Transaction,
};
use std::str::FromStr;

use claw_solana_core::rpc::ClawRpcClient;
use claw_types::tool::{ToolInput, ToolOutput, ToolSpec};

use crate::{errors::ToolError, tool::Tool};

/// `build_transfer` — constructs (but does NOT sign or send) a SOL transfer transaction.
///
/// The returned transaction is base64-encoded and must pass through the
/// simulation + policy pipeline before signing.
pub struct BuildTransferTool {
    _rpc: ClawRpcClient,
}

impl BuildTransferTool {
    pub fn new(rpc: ClawRpcClient) -> Self {
        Self { _rpc: rpc }
    }
}

#[async_trait]
impl Tool for BuildTransferTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "build_transfer".to_string(),
            description: "Builds a SOL transfer transaction. Returns a base64-encoded unsigned transaction that can then be simulated and, after policy approval, signed and sent.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["from", "to", "lamports"],
                "properties": {
                    "from": {
                        "type": "string",
                        "description": "Sender public key (base58)"
                    },
                    "to": {
                        "type": "string",
                        "description": "Recipient public key (base58)"
                    },
                    "lamports": {
                        "type": "integer",
                        "description": "Amount to transfer in lamports (1 SOL = 1,000,000,000 lamports)"
                    }
                }
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "transaction_b64": { "type": "string" },
                    "from":            { "type": "string" },
                    "to":              { "type": "string" },
                    "lamports":        { "type": "integer" },
                    "sol":             { "type": "number" }
                }
            }),
            required_capabilities: vec!["build_transaction".to_string()],
            supports_streaming: false,
            timeout_ms: 10_000,
        }
    }

    async fn execute(&self, input: ToolInput) -> Result<ToolOutput, ToolError> {
        let from_str = input.parameters["from"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput { reason: "from required".to_string() })?;
        let to_str = input.parameters["to"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput { reason: "to required".to_string() })?;
        let lamports = input.parameters["lamports"]
            .as_u64()
            .ok_or_else(|| ToolError::InvalidInput { reason: "lamports required".to_string() })?;

        let from = Pubkey::from_str(from_str)
            .map_err(|e| ToolError::InvalidInput { reason: format!("invalid from pubkey: {e}") })?;
        let to = Pubkey::from_str(to_str)
            .map_err(|e| ToolError::InvalidInput { reason: format!("invalid to pubkey: {e}") })?;

        let ix = system_instruction::transfer(&from, &to, lamports);
        // Use a dummy blockhash — will be replaced during the pipeline's
        // blockhash attachment step before simulation or signing.
        let message = Message::new(&[ix], Some(&from));
        let tx = Transaction::new_unsigned(message);

        let tx_bytes = bincode::serialize(&tx)
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
        let tx_b64 = base64::engine::general_purpose::STANDARD.encode(&tx_bytes);

        Ok(ToolOutput {
            tool_name: "build_transfer".to_string(),
            success: true,
            data: Some(json!({
                "transaction_b64": tx_b64,
                "from": from_str,
                "to": to_str,
                "lamports": lamports,
                "sol": lamports as f64 / 1e9,
                "note": "Transaction is unsigned. Simulate and approve before signing."
            })),
            error: None,
            duration_ms: 0,
        })
    }
}
