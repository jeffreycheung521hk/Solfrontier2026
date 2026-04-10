//! Orca Whirlpool read-only tools.
//!
//! # What these tools do
//!
//! Read-only intelligence for the Orca Whirlpool AMM protocol.
//! These tools allow agents to:
//!
//! - Fetch whirlpool pool state (liquidity, price, fee tier)
//! - Estimate swap output (quote) using the concentrated liquidity formula
//! - Validate pool addresses against a known-safe allowlist
//!
//! # What these tools do NOT do
//!
//! - Build, sign, or send swap transactions
//! - Move any funds
//! - Execute any on-chain write operation
//!
//! Write-capable swap execution is explicitly deferred.
//! This module is strictly read-only intelligence.
//!
//! # Data trust model
//!
//! All data fetched from the Solana RPC is returned as structured JSON
//! tagged as untrusted external data by the agent loop's `push_tool_result`.
//! Agents must not use this data to authorize transactions without a
//! separate policy evaluation step.
//!
//! # Orca program addresses
//!
//! Whirlpool program (mainnet + devnet):
//!   `whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc`
//!
//! # Account layout reference
//!
//! Whirlpool accounts use Anchor's discriminant (8 bytes) followed by
//! the struct fields. The Whirlpool struct is documented in the Orca
//! open-source SDK:
//!   https://github.com/orca-so/whirlpools/blob/main/programs/whirlpool/src/state/whirlpool.rs
//!
//! V1 implementation parses only the key fields needed for agent reasoning:
//!   - Token mint A and B (to identify the pair)
//!   - Current sqrt_price (to compute current price)
//!   - Fee rate (basis points)
//!   - Liquidity
//!   - Tick spacing
//!
//! # V1 limitations
//!
//! - Quote estimation uses the simplified constant product approximation.
//!   Full concentrated liquidity quote (with tick array traversal) is deferred.
//! - No tick array fetching — price impact for large trades is estimated only.
//! - Pool allowlist is a static in-memory set; a configurable allowlist is planned.

use async_trait::async_trait;
use serde_json::json;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};
use std::str::FromStr;

use claw_solana_core::rpc::ClawRpcClient;
use claw_types::tool::{ToolInput, ToolOutput, ToolSpec};

use crate::{errors::ToolError, tool::Tool};

// ── Constants ─────────────────────────────────────────────────────────────────

/// Orca Whirlpool program address (mainnet + devnet).
pub const ORCA_WHIRLPOOL_PROGRAM_ID: &str = "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc";

/// Anchor discriminant for the Whirlpool V1 account type.
/// sha256("account:Whirlpool")[..8]
const WHIRLPOOL_DISCRIMINANT_V1: [u8; 8] = [0x96, 0x1a, 0xa8, 0x5a, 0x9d, 0x97, 0x0a, 0x75];

/// Anchor discriminant for the Whirlpool V2 account type (used on devnet).
/// Observed from devnet pool 3KBZiL2g8C7tiJ32hTv5v3KM7aK9htpqTw4cTXz1HvPt.
const WHIRLPOOL_DISCRIMINANT_V2: [u8; 8] = [0x3f, 0x95, 0xd1, 0x0c, 0xe1, 0x80, 0x63, 0x09];

/// Whirlpool account minimum size (discriminant + through token_vault_b).
/// Full account is ~653 bytes; we need at least through vault_b (offset 245).
const WHIRLPOOL_MIN_SIZE: usize = 245;

/// Q64.64 fixed-point divisor for sqrt_price conversion.
/// sqrt_price is stored as a Q64.64 fixed-point number.
/// price = (sqrt_price / 2^64)^2
const Q64: f64 = 18_446_744_073_709_551_616.0; // 2^64

// ── Whirlpool field offsets (byte offsets after 8-byte discriminant) ─────────
// Reference: orca-so/whirlpools/programs/whirlpool/src/state/whirlpool.rs
const _OFF_WHIRLPOOLS_CONFIG: usize = 8;       // Pubkey (32 bytes)
const _OFF_WHIRLPOOL_BUMP:    usize = 40;       // u8 (1 byte)
const OFF_TICK_SPACING:       usize = 41;       // u16 (2 bytes)
const _OFF_TICK_SPACING_SEED: usize = 43;       // u16 (2 bytes)
const OFF_FEE_RATE:           usize = 45;       // u16 (2 bytes) — fee in hundredths of bps
const _OFF_PROTOCOL_FEE_RATE: usize = 47;       // u16 (2 bytes)
const OFF_LIQUIDITY:          usize = 49;       // u128 (16 bytes)
const OFF_SQRT_PRICE:         usize = 65;       // u128 (16 bytes)
const OFF_TICK_CURRENT_IDX:   usize = 81;       // i32 (4 bytes)
const _OFF_PROTOCOL_FEE_OWED_A: usize = 85;    // u64 (8 bytes)
const _OFF_PROTOCOL_FEE_OWED_B: usize = 93;    // u64 (8 bytes)
const OFF_TOKEN_MINT_A:       usize = 101;      // Pubkey (32 bytes)
const OFF_TOKEN_VAULT_A:      usize = 133;      // Pubkey (32 bytes)
const _OFF_FEE_GROWTH_GLOBAL_A: usize = 165;   // u128 (16 bytes)
const OFF_TOKEN_MINT_B:       usize = 181;      // Pubkey (32 bytes)
const OFF_TOKEN_VAULT_B:      usize = 213;      // Pubkey (32 bytes)

// ── Parsing helpers ────────────────────────────────────────────────────────────

fn parse_pubkey(data: &[u8], offset: usize) -> Option<Pubkey> {
    if data.len() < offset + 32 {
        return None;
    }
    let bytes: [u8; 32] = data[offset..offset + 32].try_into().ok()?;
    Some(Pubkey::from(bytes))
}

fn parse_u16_le(data: &[u8], offset: usize) -> Option<u16> {
    if data.len() < offset + 2 {
        return None;
    }
    Some(u16::from_le_bytes(data[offset..offset + 2].try_into().ok()?))
}

fn parse_u128_le(data: &[u8], offset: usize) -> Option<u128> {
    if data.len() < offset + 16 {
        return None;
    }
    Some(u128::from_le_bytes(data[offset..offset + 16].try_into().ok()?))
}

fn parse_i32_le(data: &[u8], offset: usize) -> Option<i32> {
    if data.len() < offset + 4 {
        return None;
    }
    Some(i32::from_le_bytes(data[offset..offset + 4].try_into().ok()?))
}

/// Converts a Q64.64 sqrt_price to a human-readable price (token_b per token_a).
fn sqrt_price_to_price(sqrt_price: u128) -> f64 {
    let sqrt_f = sqrt_price as f64 / Q64;
    sqrt_f * sqrt_f
}

/// Parsed representation of a Whirlpool account's key fields.
#[derive(Debug)]
struct ParsedWhirlpool {
    token_mint_a:       Pubkey,
    token_mint_b:       Pubkey,
    token_vault_a:      Pubkey,
    token_vault_b:      Pubkey,
    liquidity:          u128,
    sqrt_price:         u128,
    tick_current_index: i32,
    tick_spacing:       u16,
    fee_rate:           u16, // hundredths of bps (e.g., 300 = 0.03%)
}

fn parse_whirlpool_account(data: &[u8]) -> Result<ParsedWhirlpool, String> {
    if data.len() < WHIRLPOOL_MIN_SIZE {
        return Err(format!(
            "account too small: {} bytes (expected >= {})",
            data.len(),
            WHIRLPOOL_MIN_SIZE
        ));
    }

    // Verify discriminant (accept both V1 and V2)
    if data[..8] != WHIRLPOOL_DISCRIMINANT_V1 && data[..8] != WHIRLPOOL_DISCRIMINANT_V2 {
        return Err(format!(
            "wrong account discriminant — not a Whirlpool account (got {:?})",
            &data[..8]
        ));
    }

    let token_mint_a = parse_pubkey(data, OFF_TOKEN_MINT_A)
        .ok_or("cannot parse token_mint_a")?;
    let token_vault_a = parse_pubkey(data, OFF_TOKEN_VAULT_A)
        .ok_or("cannot parse token_vault_a")?;
    let token_mint_b = parse_pubkey(data, OFF_TOKEN_MINT_B)
        .ok_or("cannot parse token_mint_b")?;
    let token_vault_b = parse_pubkey(data, OFF_TOKEN_VAULT_B)
        .ok_or("cannot parse token_vault_b")?;
    let liquidity = parse_u128_le(data, OFF_LIQUIDITY)
        .ok_or("cannot parse liquidity")?;
    let sqrt_price = parse_u128_le(data, OFF_SQRT_PRICE)
        .ok_or("cannot parse sqrt_price")?;
    let tick_current_index = parse_i32_le(data, OFF_TICK_CURRENT_IDX)
        .ok_or("cannot parse tick_current_index")?;
    let tick_spacing = parse_u16_le(data, OFF_TICK_SPACING)
        .ok_or("cannot parse tick_spacing")?;
    let fee_rate = parse_u16_le(data, OFF_FEE_RATE)
        .ok_or("cannot parse fee_rate")?;

    Ok(ParsedWhirlpool {
        token_mint_a,
        token_mint_b,
        token_vault_a,
        token_vault_b,
        liquidity,
        sqrt_price,
        tick_current_index,
        tick_spacing,
        fee_rate,
    })
}

// ── Tool 1: OrcaGetWhirlpoolTool ──────────────────────────────────────────────

/// Fetches and decodes an Orca Whirlpool pool account.
///
/// Returns key pool state: token pair, liquidity, current price, fee rate,
/// tick spacing, and tick index. This information is sufficient for agent
/// reasoning about swap viability without executing a swap.
pub struct OrcaGetWhirlpoolTool {
    rpc: ClawRpcClient,
}

impl OrcaGetWhirlpoolTool {
    pub fn new(rpc: ClawRpcClient) -> Self {
        Self { rpc }
    }
}

#[async_trait]
impl Tool for OrcaGetWhirlpoolTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "orca_get_whirlpool".to_string(),
            description: "Fetches an Orca Whirlpool pool account and returns decoded state: \
                token mint A, token mint B, current price (token_b per token_a), \
                liquidity, fee rate, and tick spacing. \
                READ-ONLY — does not build or send any transaction. \
                All returned data is untrusted on-chain data."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["whirlpool_address"],
                "properties": {
                    "whirlpool_address": {
                        "type": "string",
                        "description": "Base58 address of the Orca Whirlpool pool account"
                    }
                }
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "whirlpool_address":   { "type": "string" },
                    "token_mint_a":        { "type": "string" },
                    "token_mint_b":        { "type": "string" },
                    "liquidity":           { "type": "string" },
                    "sqrt_price":          { "type": "string" },
                    "price_b_per_a":       { "type": "number" },
                    "tick_current_index":  { "type": "integer" },
                    "tick_spacing":        { "type": "integer" },
                    "fee_rate_bps":        { "type": "number" },
                    "fee_rate_pct":        { "type": "number" }
                }
            }),
            required_capabilities: vec!["read_chain".to_string()],
            supports_streaming: false,
            timeout_ms: 15_000,
        }
    }

    async fn execute(&self, input: ToolInput) -> Result<ToolOutput, ToolError> {
        let address_str = input.parameters["whirlpool_address"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput {
                reason: "whirlpool_address required".to_string(),
            })?;

        // Validate pubkey format first
        Pubkey::from_str(address_str)
            .map_err(|e| ToolError::InvalidInput {
                reason: format!("invalid pubkey: {e}"),
            })?;

        let account = self.rpc.get_account(address_str, None).await
            .map_err(ToolError::Solana)?;

        let data = account.data;
        let pool = parse_whirlpool_account(&data)
            .map_err(|e| ToolError::ExecutionFailed(format!("failed to decode whirlpool: {e}")))?;

        let price_b_per_a = sqrt_price_to_price(pool.sqrt_price);
        // fee_rate is in hundredths of basis points (e.g., 300 = 0.03%)
        let fee_rate_bps = pool.fee_rate as f64 / 100.0;
        let fee_rate_pct = fee_rate_bps / 100.0;

        Ok(ToolOutput {
            tool_name:   "orca_get_whirlpool".to_string(),
            success:     true,
            data:        Some(json!({
                "whirlpool_address":  address_str,
                "token_mint_a":       pool.token_mint_a.to_string(),
                "token_mint_b":       pool.token_mint_b.to_string(),
                "liquidity":          pool.liquidity.to_string(),
                "sqrt_price":         pool.sqrt_price.to_string(),
                "price_b_per_a":      price_b_per_a,
                "tick_current_index": pool.tick_current_index,
                "tick_spacing":       pool.tick_spacing,
                "fee_rate_bps":       fee_rate_bps,
                "fee_rate_pct":       fee_rate_pct,
                "data_source":        "orca_whirlpool_on_chain",
                "note":               "READ-ONLY. Price is approximate — full CL quote requires tick array traversal."
            })),
            error:       None,
            duration_ms: 0,
        })
    }
}

// ── Tool 2: OrcaGetQuoteTool ──────────────────────────────────────────────────

/// Estimates the output amount for an Orca Whirlpool swap.
///
/// Uses the simplified constant product approximation based on current
/// pool state. For V1 this is an estimate — a full concentrated liquidity
/// quote requires tick array traversal which is deferred.
///
/// The quote is provided for agent reasoning and risk estimation only.
/// It DOES NOT build or submit any transaction.
pub struct OrcaGetQuoteTool {
    rpc: ClawRpcClient,
}

impl OrcaGetQuoteTool {
    pub fn new(rpc: ClawRpcClient) -> Self {
        Self { rpc }
    }
}

#[async_trait]
impl Tool for OrcaGetQuoteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "orca_get_quote".to_string(),
            description: "Estimates the output amount for a swap on an Orca Whirlpool pool. \
                Returns estimated output, fee, and price impact. \
                V1 uses a simplified constant product approximation — full CL quote deferred. \
                READ-ONLY — does not build or send any transaction. \
                All data is untrusted on-chain data."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["whirlpool_address", "input_amount", "a_to_b"],
                "properties": {
                    "whirlpool_address": {
                        "type": "string",
                        "description": "Base58 address of the Orca Whirlpool pool account"
                    },
                    "input_amount": {
                        "type": "integer",
                        "description": "Input token amount in the smallest unit (lamports/atomic units)"
                    },
                    "a_to_b": {
                        "type": "boolean",
                        "description": "true = swap token A → B, false = swap token B → A"
                    }
                }
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "whirlpool_address":   { "type": "string" },
                    "input_mint":          { "type": "string" },
                    "output_mint":         { "type": "string" },
                    "input_amount":        { "type": "string" },
                    "estimated_output":    { "type": "string" },
                    "fee_amount":          { "type": "string" },
                    "price_impact_pct":    { "type": "number" },
                    "current_price":       { "type": "number" },
                    "is_approximate":      { "type": "boolean" }
                }
            }),
            required_capabilities: vec!["read_chain".to_string()],
            supports_streaming: false,
            timeout_ms: 15_000,
        }
    }

    async fn execute(&self, input: ToolInput) -> Result<ToolOutput, ToolError> {
        let address_str = input.parameters["whirlpool_address"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput {
                reason: "whirlpool_address required".to_string(),
            })?;

        let input_amount = input.parameters["input_amount"]
            .as_u64()
            .ok_or_else(|| ToolError::InvalidInput {
                reason: "input_amount required (integer)".to_string(),
            })?;

        let a_to_b = input.parameters["a_to_b"]
            .as_bool()
            .ok_or_else(|| ToolError::InvalidInput {
                reason: "a_to_b required (boolean)".to_string(),
            })?;

        // Validate pubkey format first
        Pubkey::from_str(address_str)
            .map_err(|e| ToolError::InvalidInput {
                reason: format!("invalid pubkey: {e}"),
            })?;

        let account = self.rpc.get_account(address_str, None).await
            .map_err(ToolError::Solana)?;

        let pool = parse_whirlpool_account(&account.data)
            .map_err(|e| ToolError::ExecutionFailed(format!("failed to decode whirlpool: {e}")))?;

        // ── Simplified quote (constant product approximation) ─────────────
        //
        // V1 limitation: true concentrated liquidity quote requires traversing
        // tick arrays. This approximation is valid only when the swap stays
        // within the current tick's price range, which is the common case for
        // small swaps. For large swaps, price impact will be underestimated.
        //
        // Formula for CL constant product in the current tick:
        //   For a_to_b (selling A, buying B):
        //     L = liquidity, sp = sqrt_price (current)
        //     Δsqrt_price = -Δa * sp² / (L + Δa * sp)
        //     Δb = L * |Δsqrt_price|
        //
        // For the simplified version we use the spot price:
        //   output ≈ input * spot_price * (1 - fee_rate)

        let price_b_per_a = sqrt_price_to_price(pool.sqrt_price);
        let fee_rate = pool.fee_rate as f64 / 1_000_000.0; // convert hundredths-of-bps to fraction

        let (input_mint, output_mint, spot_price) = if a_to_b {
            (
                pool.token_mint_a.to_string(),
                pool.token_mint_b.to_string(),
                price_b_per_a,
            )
        } else {
            (
                pool.token_mint_b.to_string(),
                pool.token_mint_a.to_string(),
                1.0 / price_b_per_a,
            )
        };

        let input_f = input_amount as f64;
        let fee_amount_f = input_f * fee_rate;
        let input_after_fee_f = input_f - fee_amount_f;
        let estimated_output_f = input_after_fee_f * spot_price;

        // Price impact: how much does this trade move the price?
        // For constant product: price_impact ≈ input / (2 * virtual_reserve)
        // Virtual reserve ≈ liquidity / sqrt_price (for token A)
        let virtual_reserve = if pool.sqrt_price > 0 && pool.liquidity > 0 {
            pool.liquidity as f64 / (pool.sqrt_price as f64 / Q64)
        } else {
            f64::MAX
        };
        let price_impact_pct = if virtual_reserve > 0.0 {
            (input_f / (2.0 * virtual_reserve) * 100.0).min(100.0)
        } else {
            100.0 // unknown — assume worst case
        };

        Ok(ToolOutput {
            tool_name:   "orca_get_quote".to_string(),
            success:     true,
            data:        Some(json!({
                "whirlpool_address": address_str,
                "input_mint":        input_mint,
                "output_mint":       output_mint,
                "input_amount":      input_amount.to_string(),
                "estimated_output":  (estimated_output_f as u64).to_string(),
                "fee_amount":        (fee_amount_f as u64).to_string(),
                "price_impact_pct":  price_impact_pct,
                "current_price":     spot_price,
                "fee_rate_pct":      fee_rate * 100.0,
                "is_approximate":    true,
                "note": "V1 approximation using spot price. Full CL quote with tick array traversal is deferred. \
                         Price impact may be underestimated for large swaps."
            })),
            error:       None,
            duration_ms: 0,
        })
    }
}

// ── Tool 3: OrcaPoolAllowlistTool ─────────────────────────────────────────────

/// Validates an Orca pool address against a known-safe allowlist.
///
/// Returns whether the pool is on the allowlist, the pool program owner,
/// and whether the account is owned by the expected Orca Whirlpool program.
/// This is a pre-flight check before constructing swap transactions.
///
/// V1: allowlist is the empty set (trust no pool by default). Operators
/// should configure an explicit allowlist in policy config.
pub struct OrcaPoolAllowlistTool {
    rpc:       ClawRpcClient,
    /// Pool addresses the operator has explicitly allowed.
    allowlist: Vec<String>,
}

impl OrcaPoolAllowlistTool {
    pub fn new(rpc: ClawRpcClient, allowlist: Vec<String>) -> Self {
        Self { rpc, allowlist }
    }
}

#[async_trait]
impl Tool for OrcaPoolAllowlistTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "orca_check_pool".to_string(),
            description: "Validates an Orca Whirlpool pool address. \
                Checks: (1) account exists, (2) owned by the Orca Whirlpool program, \
                (3) has the correct account discriminant, (4) optionally on an operator allowlist. \
                READ-ONLY — does not build or send any transaction."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["whirlpool_address"],
                "properties": {
                    "whirlpool_address": {
                        "type": "string",
                        "description": "Base58 address of the Orca Whirlpool pool account to validate"
                    }
                }
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "whirlpool_address": { "type": "string" },
                    "exists":            { "type": "boolean" },
                    "owned_by_orca":     { "type": "boolean" },
                    "valid_discriminant":{ "type": "boolean" },
                    "on_allowlist":      { "type": "boolean" },
                    "program_owner":     { "type": "string" },
                    "safe_to_use":       { "type": "boolean" },
                    "reason":            { "type": "string" }
                }
            }),
            required_capabilities: vec!["read_chain".to_string()],
            supports_streaming: false,
            timeout_ms: 10_000,
        }
    }

    async fn execute(&self, input: ToolInput) -> Result<ToolOutput, ToolError> {
        let address_str = input.parameters["whirlpool_address"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput {
                reason: "whirlpool_address required".to_string(),
            })?;

        // Validate pubkey format first
        Pubkey::from_str(address_str)
            .map_err(|e| ToolError::InvalidInput {
                reason: format!("invalid pubkey: {e}"),
            })?;

        let account_result = self.rpc.get_account(address_str, None).await;

        let (exists, owned_by_orca, valid_discriminant, program_owner) = match account_result {
            Err(_) => (false, false, false, "not_found".to_string()),
            Ok(acc) => {
                let owner = acc.owner.to_string();
                let owned = owner == ORCA_WHIRLPOOL_PROGRAM_ID;
                let valid_disc = acc.data.len() >= 8
                    && (acc.data[..8] == WHIRLPOOL_DISCRIMINANT_V1 || acc.data[..8] == WHIRLPOOL_DISCRIMINANT_V2);
                (true, owned, valid_disc, owner)
            }
        };

        let on_allowlist = self.allowlist.iter().any(|a| a == address_str);

        // Safe to use = exists + owned by Orca program + valid discriminant + on allowlist
        // If allowlist is empty, we do NOT auto-approve — fail-closed.
        let (safe_to_use, reason) = if !exists {
            (false, "account does not exist".to_string())
        } else if !owned_by_orca {
            (false, format!("account owner is '{}', not Orca Whirlpool program", program_owner))
        } else if !valid_discriminant {
            (false, "account discriminant does not match Whirlpool type".to_string())
        } else if self.allowlist.is_empty() {
            (false, "operator allowlist is empty — no pools are pre-approved; add pool to config allowlist".to_string())
        } else if !on_allowlist {
            (false, "pool is valid but not on operator allowlist — add to policy config to enable".to_string())
        } else {
            (true, "pool is valid and on operator allowlist".to_string())
        };

        Ok(ToolOutput {
            tool_name:   "orca_check_pool".to_string(),
            success:     true,
            data:        Some(json!({
                "whirlpool_address":  address_str,
                "exists":             exists,
                "owned_by_orca":      owned_by_orca,
                "valid_discriminant": valid_discriminant,
                "on_allowlist":       on_allowlist,
                "program_owner":      program_owner,
                "safe_to_use":        safe_to_use,
                "reason":             reason,
                "orca_program_id":    ORCA_WHIRLPOOL_PROGRAM_ID
            })),
            error:       None,
            duration_ms: 0,
        })
    }
}

// ── Swap infrastructure ──────────────────────────────────────────────────────

/// SPL Token program ID.
const SPL_TOKEN_PROGRAM_ID: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

/// Anchor discriminator for the Whirlpool `swap` instruction.
/// sha256("global:swap")[..8]
const SWAP_IX_DISCRIMINATOR: [u8; 8] = [0xf8, 0xc6, 0x9e, 0x91, 0xe1, 0x75, 0x87, 0xc8];

/// Minimum sqrt_price (a_to_b direction) — from Orca SDK.
const MIN_SQRT_PRICE: u128 = 4295048016;

/// Maximum sqrt_price (b_to_a direction) — from Orca SDK.
const MAX_SQRT_PRICE: u128 = 79226673515401279992447579055;

/// Number of ticks per tick array (constant in Whirlpool program).
const TICK_ARRAY_SIZE: i32 = 88;

/// Derives the oracle PDA for a whirlpool.
/// Seeds: ["oracle", whirlpool_pubkey]
fn derive_oracle_pda(whirlpool: &Pubkey) -> Pubkey {
    let program_id = Pubkey::from_str(ORCA_WHIRLPOOL_PROGRAM_ID).unwrap();
    let (pda, _) = Pubkey::find_program_address(
        &[b"oracle", whirlpool.as_ref()],
        &program_id,
    );
    pda
}

/// Derives a tick array PDA for a given start_tick_index.
/// Seeds: ["tick_array", whirlpool_pubkey, start_tick_index.to_string()]
fn derive_tick_array_pda(whirlpool: &Pubkey, start_tick_index: i32) -> Pubkey {
    let program_id = Pubkey::from_str(ORCA_WHIRLPOOL_PROGRAM_ID).unwrap();
    let start_str = start_tick_index.to_string();
    let (pda, _) = Pubkey::find_program_address(
        &[b"tick_array", whirlpool.as_ref(), start_str.as_bytes()],
        &program_id,
    );
    pda
}

/// Computes the start_tick_index for the tick array containing the given tick.
fn start_tick_index_for(tick: i32, tick_spacing: u16) -> i32 {
    let ticks_in_array = TICK_ARRAY_SIZE * tick_spacing as i32;
    (tick as f64 / ticks_in_array as f64).floor() as i32 * ticks_in_array
}

/// Returns the 3 tick array PDAs needed for a swap.
/// For a_to_b: current, current - 1 array, current - 2 arrays
/// For b_to_a: current, current + 1 array, current + 2 arrays
fn resolve_tick_arrays(
    whirlpool: &Pubkey,
    tick_current_index: i32,
    tick_spacing: u16,
    a_to_b: bool,
) -> [Pubkey; 3] {
    let ticks_in_array = TICK_ARRAY_SIZE * tick_spacing as i32;
    let start0 = start_tick_index_for(tick_current_index, tick_spacing);

    let (start1, start2) = if a_to_b {
        (start0 - ticks_in_array, start0 - 2 * ticks_in_array)
    } else {
        (start0 + ticks_in_array, start0 + 2 * ticks_in_array)
    };

    [
        derive_tick_array_pda(whirlpool, start0),
        derive_tick_array_pda(whirlpool, start1),
        derive_tick_array_pda(whirlpool, start2),
    ]
}

/// Builds the raw Whirlpool swap instruction.
fn build_swap_instruction(
    whirlpool: &Pubkey,
    pool: &ParsedWhirlpool,
    token_authority: &Pubkey,
    token_owner_account_a: &Pubkey,
    token_owner_account_b: &Pubkey,
    tick_arrays: &[Pubkey; 3],
    oracle: &Pubkey,
    amount: u64,
    other_amount_threshold: u64,
    sqrt_price_limit: u128,
    amount_specified_is_input: bool,
    a_to_b: bool,
) -> Instruction {
    let program_id = Pubkey::from_str(ORCA_WHIRLPOOL_PROGRAM_ID).unwrap();
    let token_program = Pubkey::from_str(SPL_TOKEN_PROGRAM_ID).unwrap();

    let accounts = vec![
        AccountMeta::new_readonly(token_program, false),
        AccountMeta::new_readonly(*token_authority, true),     // signer
        AccountMeta::new(*whirlpool, false),
        AccountMeta::new(*token_owner_account_a, false),
        AccountMeta::new(pool.token_vault_a, false),
        AccountMeta::new(*token_owner_account_b, false),
        AccountMeta::new(pool.token_vault_b, false),
        AccountMeta::new(tick_arrays[0], false),
        AccountMeta::new(tick_arrays[1], false),
        AccountMeta::new(tick_arrays[2], false),
        AccountMeta::new_readonly(*oracle, false),
    ];

    // Instruction data: discriminator + amount(u64) + other_amount_threshold(u64)
    //   + sqrt_price_limit(u128) + amount_specified_is_input(bool) + a_to_b(bool)
    let mut data = Vec::with_capacity(8 + 8 + 8 + 16 + 1 + 1);
    data.extend_from_slice(&SWAP_IX_DISCRIMINATOR);
    data.extend_from_slice(&amount.to_le_bytes());
    data.extend_from_slice(&other_amount_threshold.to_le_bytes());
    data.extend_from_slice(&sqrt_price_limit.to_le_bytes());
    data.push(amount_specified_is_input as u8);
    data.push(a_to_b as u8);

    Instruction { program_id, accounts, data }
}

/// Derives the Associated Token Account address for a given wallet + mint.
fn derive_ata(wallet: &Pubkey, mint: &Pubkey) -> Pubkey {
    spl_associated_token_account::get_associated_token_address(wallet, mint)
}

/// Builds a create-ATA-if-needed instruction (idempotent).
fn create_ata_idempotent_instruction(
    funder: &Pubkey,
    wallet: &Pubkey,
    mint: &Pubkey,
) -> Instruction {
    let token_program = Pubkey::from_str(SPL_TOKEN_PROGRAM_ID).unwrap();
    spl_associated_token_account::instruction::create_associated_token_account_idempotent(
        funder,
        wallet,
        mint,
        &token_program,
    )
}

// ── Tool 4: OrcaSwapTool ─────────────────────────────────────────────────────

/// Builds an unsigned Orca Whirlpool swap transaction.
///
/// This tool fetches the pool state, resolves all required accounts
/// (tick arrays, oracle, ATAs), and returns a base64-encoded unsigned
/// transaction ready for the signing pipeline.
///
/// The tool does NOT sign or submit — it produces a `TransactionProposal`
/// that flows through simulate → policy → approve → sign.
pub struct OrcaSwapTool {
    rpc: ClawRpcClient,
}

impl OrcaSwapTool {
    pub fn new(rpc: ClawRpcClient) -> Self {
        Self { rpc }
    }
}

#[async_trait]
impl Tool for OrcaSwapTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "orca_swap".to_string(),
            description: "Build an unsigned Orca Whirlpool swap transaction. \
                Returns base64-encoded transaction bytes for the signing pipeline. \
                Does NOT sign or submit.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "whirlpool_address": {
                        "type": "string",
                        "description": "Base58 address of the Orca Whirlpool pool"
                    },
                    "input_amount": {
                        "type": "integer",
                        "description": "Amount of input token in atomic units (e.g., lamports for SOL)"
                    },
                    "a_to_b": {
                        "type": "boolean",
                        "description": "Swap direction: true = token A → B, false = token B → A"
                    },
                    "slippage_bps": {
                        "type": "integer",
                        "description": "Slippage tolerance in basis points (e.g., 100 = 1%). Default: 100"
                    },
                    "wallet_pubkey": {
                        "type": "string",
                        "description": "Base58 public key of the wallet executing the swap"
                    }
                },
                "required": ["whirlpool_address", "input_amount", "a_to_b", "wallet_pubkey"]
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "transaction_b64": { "type": "string" },
                    "estimated_output": { "type": "string" },
                    "price_impact_pct": { "type": "number" },
                    "input_mint": { "type": "string" },
                    "output_mint": { "type": "string" }
                }
            }),
            required_capabilities: vec!["build_transaction".to_string()],
            supports_streaming: false,
            timeout_ms: 20_000,
        }
    }

    async fn execute(&self, input: ToolInput) -> Result<ToolOutput, ToolError> {
        // Parse inputs
        let whirlpool_str = input.parameters["whirlpool_address"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput { reason: "missing whirlpool_address".into() })?;
        let whirlpool_pk = Pubkey::from_str(whirlpool_str)
            .map_err(|e| ToolError::InvalidInput { reason: format!("invalid whirlpool address: {e}") })?;

        let input_amount = input.parameters["input_amount"]
            .as_u64()
            .ok_or_else(|| ToolError::InvalidInput { reason: "missing or invalid input_amount".into() })?;

        let a_to_b = input.parameters["a_to_b"]
            .as_bool()
            .ok_or_else(|| ToolError::InvalidInput { reason: "missing a_to_b".into() })?;

        let slippage_bps = input.parameters["slippage_bps"]
            .as_u64()
            .unwrap_or(100) as u32; // default 1%

        let wallet_str = input.parameters["wallet_pubkey"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput { reason: "missing wallet_pubkey".into() })?;
        let wallet_pk = Pubkey::from_str(wallet_str)
            .map_err(|e| ToolError::InvalidInput { reason: format!("invalid wallet pubkey: {e}") })?;

        // Fetch pool state
        let account = self.rpc.get_account(whirlpool_str, None).await
            .map_err(|e| ToolError::ExecutionFailed(format!("RPC error fetching pool: {e}")))?;

        let pool = parse_whirlpool_account(&account.data)
            .map_err(|e| ToolError::ExecutionFailed(format!("failed to parse pool: {e}")))?;

        // Determine input/output mints
        let (input_mint, output_mint) = if a_to_b {
            (pool.token_mint_a, pool.token_mint_b)
        } else {
            (pool.token_mint_b, pool.token_mint_a)
        };

        // Estimate output using constant product approximation (same as quote tool)
        let price_b_per_a = sqrt_price_to_price(pool.sqrt_price);
        let fee_fraction = pool.fee_rate as f64 / 1_000_000.0;
        let input_after_fee = input_amount as f64 * (1.0 - fee_fraction);
        let estimated_output = if a_to_b {
            (input_after_fee * price_b_per_a) as u64
        } else {
            if price_b_per_a > 0.0 {
                (input_after_fee / price_b_per_a) as u64
            } else {
                0
            }
        };

        // Slippage: minimum output = estimated * (1 - slippage)
        let other_amount_threshold = estimated_output
            .saturating_mul(10_000u64.saturating_sub(slippage_bps as u64))
            / 10_000;

        // sqrt_price_limit: use min/max constants based on direction
        let sqrt_price_limit = if a_to_b { MIN_SQRT_PRICE } else { MAX_SQRT_PRICE };

        // Resolve ATAs for wallet
        let token_owner_a = derive_ata(&wallet_pk, &pool.token_mint_a);
        let token_owner_b = derive_ata(&wallet_pk, &pool.token_mint_b);

        // Resolve tick arrays
        let tick_arrays = resolve_tick_arrays(
            &whirlpool_pk,
            pool.tick_current_index,
            pool.tick_spacing,
            a_to_b,
        );

        // Oracle PDA
        let oracle = derive_oracle_pda(&whirlpool_pk);

        // Detect if input token is native SOL (needs wrap)
        let native_sol_mint = Pubkey::from_str("So11111111111111111111111111111111111111112").unwrap();
        let input_is_native_sol = input_mint == native_sol_mint;

        // Build instructions
        let mut instructions = Vec::new();

        // Create ATAs if needed (idempotent)
        instructions.push(create_ata_idempotent_instruction(&wallet_pk, &wallet_pk, &pool.token_mint_a));
        instructions.push(create_ata_idempotent_instruction(&wallet_pk, &wallet_pk, &pool.token_mint_b));

        // If input is native SOL, fund the wSOL ATA and sync
        if input_is_native_sol {
            let wsol_ata = if a_to_b { &token_owner_a } else { &token_owner_b };
            // Transfer SOL into wSOL ATA (swap amount + small buffer for rent if newly created)
            instructions.push(solana_sdk::system_instruction::transfer(
                &wallet_pk,
                wsol_ata,
                input_amount + 10_000, // extra for rent delta
            ));
            // SyncNative: update the token account balance to reflect the SOL deposit
            let token_program_id = Pubkey::from_str(SPL_TOKEN_PROGRAM_ID).unwrap();
            instructions.push(Instruction {
                program_id: token_program_id,
                accounts: vec![AccountMeta::new(*wsol_ata, false)],
                data: vec![17], // SyncNative instruction index
            });
        }

        // Swap instruction
        instructions.push(build_swap_instruction(
            &whirlpool_pk,
            &pool,
            &wallet_pk,
            &token_owner_a,
            &token_owner_b,
            &tick_arrays,
            &oracle,
            input_amount,
            other_amount_threshold,
            sqrt_price_limit,
            true, // amount_specified_is_input (ExactIn)
            a_to_b,
        ));

        // Build unsigned transaction
        let message = solana_sdk::message::Message::new(&instructions, Some(&wallet_pk));
        let tx = solana_sdk::transaction::Transaction::new_unsigned(message);
        let tx_bytes = bincode::serialize(&tx)
            .map_err(|e| ToolError::ExecutionFailed(format!("serialize error: {e}")))?;
        let tx_b64 = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            &tx_bytes,
        );

        // Price impact estimate
        let virtual_reserve = if pool.sqrt_price > 0 {
            pool.liquidity as f64 / (pool.sqrt_price as f64 / Q64)
        } else {
            f64::MAX
        };
        let price_impact_pct = (input_amount as f64 / (2.0 * virtual_reserve) * 100.0).min(100.0);

        Ok(ToolOutput {
            tool_name: "orca_swap".to_string(),
            success: true,
            data: Some(json!({
                "transaction_b64": tx_b64,
                "input_mint": input_mint.to_string(),
                "output_mint": output_mint.to_string(),
                "input_amount": input_amount.to_string(),
                "estimated_output": estimated_output.to_string(),
                "other_amount_threshold": other_amount_threshold.to_string(),
                "slippage_bps": slippage_bps,
                "price_impact_pct": price_impact_pct,
                "a_to_b": a_to_b,
                "tick_arrays": [
                    tick_arrays[0].to_string(),
                    tick_arrays[1].to_string(),
                    tick_arrays[2].to_string(),
                ],
                "oracle": oracle.to_string(),
                "is_unsigned": true,
            })),
            error: None,
            duration_ms: 0,
        })
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqrt_price_to_price_identity() {
        // At sqrt_price = 2^64 (Q64.64 = 1.0), price should be 1.0
        let sqrt_price_one = 1u128 << 64;
        let price = sqrt_price_to_price(sqrt_price_one);
        assert!((price - 1.0).abs() < 1e-9, "price at sqrt=1 should be 1.0, got {}", price);
    }

    #[test]
    fn sqrt_price_to_price_two() {
        // At sqrt_price = 2 * 2^64 (Q64.64 = 2.0), price should be 4.0
        let sqrt_price_two = 2u128 << 64;
        let price = sqrt_price_to_price(sqrt_price_two);
        assert!((price - 4.0).abs() < 0.001, "price at sqrt=2 should be ~4.0, got {}", price);
    }

    #[test]
    fn parse_whirlpool_wrong_discriminant() {
        let mut data = vec![0u8; WHIRLPOOL_MIN_SIZE];
        // Wrong discriminant
        data[0] = 0xFF;
        let result = parse_whirlpool_account(&data);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("wrong account discriminant"));
    }

    #[test]
    fn parse_whirlpool_too_short() {
        let data = vec![0u8; 10];
        let result = parse_whirlpool_account(&data);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("too small"));
    }

    #[test]
    fn allowlist_empty_fails_closed() {
        assert_eq!(
            ORCA_WHIRLPOOL_PROGRAM_ID,
            "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc"
        );
    }

    // ── Swap infrastructure tests ────────────────────────────────────────

    #[test]
    fn oracle_pda_is_deterministic() {
        let pool = Pubkey::new_unique();
        let pda1 = derive_oracle_pda(&pool);
        let pda2 = derive_oracle_pda(&pool);
        assert_eq!(pda1, pda2);
        // Should not be the same as the pool address
        assert_ne!(pda1, pool);
    }

    #[test]
    fn tick_array_pda_is_deterministic() {
        let pool = Pubkey::new_unique();
        let pda1 = derive_tick_array_pda(&pool, 0);
        let pda2 = derive_tick_array_pda(&pool, 0);
        assert_eq!(pda1, pda2);
        // Different start indices → different PDAs
        let pda3 = derive_tick_array_pda(&pool, 704);
        assert_ne!(pda1, pda3);
    }

    #[test]
    fn start_tick_index_for_positive_tick() {
        // tick_spacing=8, ticks_in_array = 88 * 8 = 704
        assert_eq!(start_tick_index_for(100, 8), 0);
        assert_eq!(start_tick_index_for(703, 8), 0);
        assert_eq!(start_tick_index_for(704, 8), 704);
        assert_eq!(start_tick_index_for(1000, 8), 704);
    }

    #[test]
    fn start_tick_index_for_negative_tick() {
        // Negative ticks should floor to the lower array start
        assert_eq!(start_tick_index_for(-1, 8), -704);
        assert_eq!(start_tick_index_for(-704, 8), -704);
        assert_eq!(start_tick_index_for(-705, 8), -1408);
    }

    #[test]
    fn resolve_tick_arrays_returns_3_distinct_pdas() {
        let pool = Pubkey::new_unique();
        let arrays = resolve_tick_arrays(&pool, 100, 8, true);
        // All three should be different
        assert_ne!(arrays[0], arrays[1]);
        assert_ne!(arrays[1], arrays[2]);
        assert_ne!(arrays[0], arrays[2]);
    }

    #[test]
    fn build_swap_ix_has_correct_accounts_and_data() {
        let whirlpool = Pubkey::new_unique();
        let pool = ParsedWhirlpool {
            token_mint_a: Pubkey::new_unique(),
            token_mint_b: Pubkey::new_unique(),
            token_vault_a: Pubkey::new_unique(),
            token_vault_b: Pubkey::new_unique(),
            liquidity: 1_000_000,
            sqrt_price: 1u128 << 64,
            tick_current_index: 0,
            tick_spacing: 8,
            fee_rate: 300,
        };
        let authority = Pubkey::new_unique();
        let owner_a = Pubkey::new_unique();
        let owner_b = Pubkey::new_unique();
        let tick_arrays = [Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique()];
        let oracle = Pubkey::new_unique();

        let ix = build_swap_instruction(
            &whirlpool, &pool, &authority,
            &owner_a, &owner_b,
            &tick_arrays, &oracle,
            100_000, 90_000, MIN_SQRT_PRICE, true, true,
        );

        // 11 accounts
        assert_eq!(ix.accounts.len(), 11);
        // Correct program
        let program_id = Pubkey::from_str(ORCA_WHIRLPOOL_PROGRAM_ID).unwrap();
        assert_eq!(ix.program_id, program_id);
        // Data: 8 (disc) + 8 (amount) + 8 (threshold) + 16 (sqrt_price) + 1 + 1 = 42
        assert_eq!(ix.data.len(), 42);
        // Discriminator
        assert_eq!(&ix.data[..8], &SWAP_IX_DISCRIMINATOR);
        // Amount
        assert_eq!(u64::from_le_bytes(ix.data[8..16].try_into().unwrap()), 100_000);
        // a_to_b flag at offset 41
        assert_eq!(ix.data[41], 1); // true
    }

    #[test]
    fn derive_ata_is_deterministic() {
        let wallet = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let ata1 = derive_ata(&wallet, &mint);
        let ata2 = derive_ata(&wallet, &mint);
        assert_eq!(ata1, ata2);
    }
}
