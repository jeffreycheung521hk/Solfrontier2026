//! Wallet inspection tools.

use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::instrument;

use claw_solana_core::rpc::ClawRpcClient;
use claw_types::{
    solana::CommitmentLevel,
    tool::{ToolInput, ToolOutput, ToolSpec},
};

use crate::{errors::ToolError, tool::Tool};

/// `get_wallet_balance` — returns the SOL and token balances for a wallet.
pub struct GetWalletBalanceTool {
    rpc: ClawRpcClient,
}

impl GetWalletBalanceTool {
    pub fn new(rpc: ClawRpcClient) -> Self {
        Self { rpc }
    }
}

#[async_trait]
impl Tool for GetWalletBalanceTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "get_wallet_balance".to_string(),
            description: "Returns the SOL balance (in lamports and SOL) for the given wallet address.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["pubkey"],
                "properties": {
                    "pubkey": {
                        "type": "string",
                        "description": "Base58-encoded public key of the wallet"
                    },
                    "commitment": {
                        "type": "string",
                        "enum": ["processed", "confirmed", "finalized"],
                        "description": "Commitment level (default: confirmed)"
                    }
                }
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "pubkey":    { "type": "string" },
                    "lamports":  { "type": "integer" },
                    "sol":       { "type": "number" }
                }
            }),
            required_capabilities: vec!["read_wallet".to_string()],
            supports_streaming: false,
            timeout_ms: 10_000,
        }
    }

    #[instrument(skip(self), fields(tool = "get_wallet_balance"))]
    async fn execute(&self, input: ToolInput) -> Result<ToolOutput, ToolError> {
        let pubkey = input.parameters["pubkey"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput {
                reason: "pubkey is required".to_string(),
            })?;

        let commitment = parse_commitment(input.parameters.get("commitment"));

        let lamports = self
            .rpc
            .get_balance(pubkey, commitment)
            .await
            .map_err(ToolError::Solana)?;

        let sol = lamports as f64 / 1_000_000_000.0;

        Ok(ToolOutput {
            tool_name: "get_wallet_balance".to_string(),
            success: true,
            data: Some(json!({
                "pubkey":   pubkey,
                "lamports": lamports,
                "sol":      sol
            })),
            error: None,
            duration_ms: 0,
        })
    }
}

/// `get_token_accounts` — returns all SPL token accounts for a wallet.
pub struct GetTokenAccountsTool {
    rpc: ClawRpcClient,
}

impl GetTokenAccountsTool {
    pub fn new(rpc: ClawRpcClient) -> Self {
        Self { rpc }
    }
}

#[async_trait]
impl Tool for GetTokenAccountsTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "get_token_accounts".to_string(),
            description: "Returns all SPL token accounts owned by the given wallet address, including balances and mint addresses.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["owner"],
                "properties": {
                    "owner": {
                        "type": "string",
                        "description": "Base58-encoded public key of the wallet owner"
                    }
                }
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "owner": { "type": "string" },
                    "accounts": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "address":   { "type": "string" },
                                "mint":      { "type": "string" },
                                "amount":    { "type": "string" },
                                "decimals":  { "type": "integer" },
                                "ui_amount": { "type": "number" }
                            }
                        }
                    }
                }
            }),
            required_capabilities: vec!["read_wallet".to_string()],
            supports_streaming: false,
            timeout_ms: 15_000,
        }
    }

    #[instrument(skip(self), fields(tool = "get_token_accounts"))]
    async fn execute(&self, input: ToolInput) -> Result<ToolOutput, ToolError> {
        let owner = input.parameters["owner"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput {
                reason: "owner is required".to_string(),
            })?;

        // Use getProgramAccounts filtered by the Token program and owner.
        // For V1, we delegate to the RPC client's getTokenAccountsByOwner.
        let rpc_client = self.rpc.pool().read_client().map_err(ToolError::Solana)?;

        use std::str::FromStr;
        use solana_sdk::pubkey::Pubkey;
        use solana_client::rpc_request::TokenAccountsFilter;

        let owner_pk = Pubkey::from_str(owner)
            .map_err(|e| ToolError::InvalidInput { reason: e.to_string() })?;

        let token_program = Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA")
            .unwrap();

        let accounts = rpc_client
            .get_token_accounts_by_owner(
                &owner_pk,
                TokenAccountsFilter::ProgramId(token_program),
            )
            .await
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

        let account_list: Vec<Value> = accounts
            .iter()
            .map(|a| {
                json!({
                    "address": a.pubkey,
                    "account": a.account
                })
            })
            .collect();

        Ok(ToolOutput {
            tool_name: "get_token_accounts".to_string(),
            success: true,
            data: Some(json!({
                "owner": owner,
                "count": account_list.len(),
                "accounts": account_list
            })),
            error: None,
            duration_ms: 0,
        })
    }
}

fn parse_commitment(val: Option<&Value>) -> Option<CommitmentLevel> {
    val.and_then(|v| v.as_str()).and_then(|s| match s {
        "processed" => Some(CommitmentLevel::Processed),
        "confirmed"  => Some(CommitmentLevel::Confirmed),
        "finalized"  => Some(CommitmentLevel::Finalized),
        _            => None,
    })
}
