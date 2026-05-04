//! Read-only `get_wallet_balances` chat tool — Phase 6C.
//!
//! Returns a balance snapshot for the session-bound wallet so the LLM
//! can reason about what the user can afford BEFORE proposing a swap or
//! a deposit. The tool NEVER signs, builds, broadcasts, or creates an
//! approval — `required_capabilities: vec![]` and the implementation
//! contains no signing-related call sites.
//!
//! # Behaviour
//!
//! - Resolve the session's bound external wallet via [`SessionBoundWallet`].
//! - If no wallet is bound, return a structured `wallet_not_bound`
//!   status (the chat handler renders this as a `ToolDispatched` outcome
//!   so the LLM/UI can react).
//! - Read SOL lamports + the wallet's USDC ATA token-account row via
//!   [`WalletBalanceReader`] (production: `ClawRpcClient`; tests stub).
//! - Return SOL + USDC balances with both raw and UI-decimal forms.
//!
//! # Insufficient-balance pattern
//!
//! When the user makes a conditional request such as "deposit 0.1 USDC
//! into Solend if my balance is above it", the recommended chat flow is:
//!
//! 1. LLM calls `get_wallet_balances` (this tool) — one tool call, turn ends.
//! 2. The next LLM turn (with the balance JSON visible) either:
//!    - calls `solend_deposit_usdc` if the balance is sufficient, or
//!    - returns plain text declining the proposal if the balance is not.
//!
//! The chat handler enforces one-tool-per-turn, so a balance read AND a
//! deposit proposal cannot be batched in a single turn.

use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use solana_sdk::pubkey::Pubkey;

use claw_solana_core::errors::SolanaError;
use claw_solana_core::rpc::ClawRpcClient;
use claw_tool_system::{errors::ToolError, tool::Tool};
use claw_types::solana::CommitmentLevel;
use claw_types::tool::{ToolInput, ToolOutput, ToolSpec};

use crate::tools::jupiter_swap::SessionBoundWallet;

/// USDC mainnet mint (6 decimals).
pub const USDC_MINT_BS58: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
/// Classic SPL Token program id.
pub const SPL_TOKEN_PROGRAM_BS58: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
//
// Phase 6C-C — the previous build of this module hard-coded an ATA
// program id constant `"ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJe1bC8"`
// (last six chars `LJe1bC8`) which is NOT the SPL Associated Token
// Account program. The canonical ATA program id ends in `LJA8knL`.
// Calling `find_program_address([owner, token_program, mint], <wrong>)`
// produced an unrelated PDA, so RPC correctly returned AccountNotFound
// and the tool reported `usdc_raw=0`.
//
// The canonical helper `crate::integrations::solend::derive_associated_token_address`
// delegates to `spl_associated_token_account::get_associated_token_address_with_program_id`
// (already a dependency; live-mainnet-validated by the Solend pipeline).
// We re-use that helper here so this module never re-derives the ATA
// program id itself.

/// Minimal token-account snapshot — only the fields the LLM and UI need.
#[derive(Debug, Clone)]
pub struct TokenAccountSnapshot {
    pub mint: String,
    pub owner: String,
    pub raw_amount: u64,
}

/// Narrow read seam. Production wraps `ClawRpcClient`; tests stub.
#[async_trait]
pub trait WalletBalanceReader: Send + Sync {
    /// Return the SOL lamport balance for `pubkey`.
    async fn get_sol_lamports(&self, pubkey: &str) -> Result<u64, String>;

    /// Return a token-account snapshot for `ata` if the account exists,
    /// or `Ok(None)` if it does not exist on chain.
    async fn get_token_account(
        &self,
        ata: &str,
    ) -> Result<Option<TokenAccountSnapshot>, String>;
}

/// Production [`WalletBalanceReader`] backed by [`ClawRpcClient`].
///
/// The daemon owns a single `ClawRpcClient` (cheap to clone — internally
/// shares the connection pool); we wrap a clone here so the read-only
/// tool inherits the same RPC pool, retry, and tracing posture as every
/// other read in the gateway.
pub struct RpcWalletBalanceReader {
    rpc: ClawRpcClient,
}

impl RpcWalletBalanceReader {
    pub fn new(rpc: ClawRpcClient) -> Self {
        Self { rpc }
    }
}

#[async_trait]
impl WalletBalanceReader for RpcWalletBalanceReader {
    async fn get_sol_lamports(&self, pubkey: &str) -> Result<u64, String> {
        self.rpc
            .get_balance(pubkey, Some(CommitmentLevel::Confirmed))
            .await
            .map_err(|e| e.to_string())
    }

    async fn get_token_account(
        &self,
        ata: &str,
    ) -> Result<Option<TokenAccountSnapshot>, String> {
        let acct = match self
            .rpc
            .get_account(ata, Some(CommitmentLevel::Confirmed))
            .await
        {
            Ok(a) => a,
            // Treat "ATA does not exist on chain" as a structured `None`
            // rather than an error — the tool contract differentiates
            // "no ATA yet" (usdc_raw=0) from "RPC failed".
            Err(SolanaError::AccountNotFound { .. }) => return Ok(None),
            Err(e) => return Err(e.to_string()),
        };
        // Classic SPL Token account layout: 165 bytes.
        //   [0..32]  mint
        //   [32..64] owner
        //   [64..72] amount (u64 little-endian)
        if acct.data.len() < 72 {
            return Err(format!(
                "token account data too short: {} bytes (need >= 72)",
                acct.data.len()
            ));
        }
        let mint_bytes: [u8; 32] = acct.data[0..32]
            .try_into()
            .map_err(|e| format!("mint slice: {e}"))?;
        let owner_bytes: [u8; 32] = acct.data[32..64]
            .try_into()
            .map_err(|e| format!("owner slice: {e}"))?;
        let mut amount_le: [u8; 8] = [0u8; 8];
        amount_le.copy_from_slice(&acct.data[64..72]);
        Ok(Some(TokenAccountSnapshot {
            mint: Pubkey::new_from_array(mint_bytes).to_string(),
            owner: Pubkey::new_from_array(owner_bytes).to_string(),
            raw_amount: u64::from_le_bytes(amount_le),
        }))
    }
}

/// Read-only wallet-balances tool. Wired into the chat allowlist as
/// `get_wallet_balances`.
pub struct GetWalletBalancesTool {
    wallet_lookup: Arc<dyn SessionBoundWallet>,
    reader: Arc<dyn WalletBalanceReader>,
}

impl GetWalletBalancesTool {
    pub fn new(
        wallet_lookup: Arc<dyn SessionBoundWallet>,
        reader: Arc<dyn WalletBalanceReader>,
    ) -> Self {
        Self {
            wallet_lookup,
            reader,
        }
    }
}

/// Derive the wallet's USDC ATA via the canonical SPL helper.
///
/// Delegates to `crate::integrations::solend::derive_associated_token_address`
/// which itself wraps `spl_associated_token_account::get_associated_token_address_with_program_id`.
/// This avoids re-implementing — and miscopying — the ATA program id.
fn derive_usdc_ata(owner: &Pubkey) -> Pubkey {
    let mint = Pubkey::from_str(USDC_MINT_BS58).expect("USDC mint constant is valid base58");
    let token_program = Pubkey::from_str(SPL_TOKEN_PROGRAM_BS58)
        .expect("SPL Token program constant is valid base58");
    crate::integrations::solend::derive_associated_token_address(owner, &mint, &token_program)
}

fn lamports_to_ui(lamports: u64) -> String {
    // SOL has 9 decimals.
    format!("{}.{:09}", lamports / 1_000_000_000, lamports % 1_000_000_000)
}

fn usdc_raw_to_ui(raw: u64) -> String {
    // USDC has 6 decimals.
    format!("{}.{:06}", raw / 1_000_000, raw % 1_000_000)
}

#[async_trait]
impl Tool for GetWalletBalancesTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "get_wallet_balances".to_string(),
            description:
                "Read-only wallet balance snapshot for the session-bound wallet. \
                 Returns SOL lamports + UI string, USDC raw + UI string, and the \
                 USDC ATA pubkey if found. Never signs, builds, broadcasts, or \
                 creates an approval. Call this BEFORE proposing a Solend deposit \
                 or Jupiter swap when the user makes a conditional request \
                 (\"deposit 0.001 USDC if I have it\"). After the tool returns, \
                 stop the turn and let the user (or a follow-up turn) decide; \
                 do not also call a transaction tool in the same turn."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "required": [],
                "properties": {}
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "status":        { "type": "string", "enum": ["ok", "wallet_not_bound", "rpc_error"] },
                    "wallet_pubkey": { "type": ["string", "null"] },
                    "sol_lamports":  { "type": ["integer", "null"] },
                    "sol_ui":        { "type": ["string", "null"] },
                    "usdc_mint":     { "type": ["string", "null"] },
                    "usdc_ata":      { "type": ["string", "null"] },
                    "usdc_raw":      { "type": ["integer", "null"] },
                    "usdc_ui":       { "type": ["string", "null"] },
                    "error":         { "type": ["string", "null"] }
                }
            }),
            required_capabilities: vec![],
            supports_streaming: false,
            timeout_ms: 8_000,
        }
    }

    async fn execute(&self, input: ToolInput) -> Result<ToolOutput, ToolError> {
        // Resolve the session-bound wallet pubkey.
        let wallet_pubkey = match self.wallet_lookup.session_wallet_pubkey(&input.session_id) {
            Some(pk) => pk,
            None => {
                return Ok(ToolOutput {
                    tool_name: "get_wallet_balances".to_string(),
                    success: false,
                    data: Some(json!({
                        "status": "wallet_not_bound",
                        "wallet_pubkey": null,
                        "sol_lamports": null,
                        "sol_ui": null,
                        "usdc_mint": USDC_MINT_BS58,
                        "usdc_ata": null,
                        "usdc_raw": null,
                        "usdc_ui": null,
                        "error": "no external wallet is bound to this session",
                    })),
                    error: None,
                    duration_ms: 0,
                });
            }
        };

        let owner_pk = Pubkey::from_str(&wallet_pubkey).map_err(|e| ToolError::InvalidInput {
            reason: format!("session-bound wallet pubkey is not base58: {e}"),
        })?;

        // SOL balance.
        let sol_lamports = match self.reader.get_sol_lamports(&wallet_pubkey).await {
            Ok(v) => v,
            Err(msg) => {
                return Ok(rpc_error_output(&wallet_pubkey, &format!("get_balance: {msg}")));
            }
        };

        // USDC ATA + balance.
        let ata = derive_usdc_ata(&owner_pk);
        let ata_bs58 = ata.to_string();
        let token_acct = match self.reader.get_token_account(&ata_bs58).await {
            Ok(v) => v,
            Err(msg) => {
                return Ok(rpc_error_output(
                    &wallet_pubkey,
                    &format!("get_token_account: {msg}"),
                ));
            }
        };

        let (usdc_ata, usdc_raw, usdc_ui) = match token_acct {
            Some(acct)
                if acct.mint == USDC_MINT_BS58 && acct.owner == wallet_pubkey =>
            {
                let ui = usdc_raw_to_ui(acct.raw_amount);
                (Some(ata_bs58), acct.raw_amount, ui)
            }
            _ => (None, 0u64, "0.000000".to_string()),
        };

        Ok(ToolOutput {
            tool_name: "get_wallet_balances".to_string(),
            success: true,
            data: Some(json!({
                "status": "ok",
                "wallet_pubkey": wallet_pubkey,
                "sol_lamports": sol_lamports,
                "sol_ui": lamports_to_ui(sol_lamports),
                "usdc_mint": USDC_MINT_BS58,
                "usdc_ata": usdc_ata,
                "usdc_raw": usdc_raw,
                "usdc_ui": usdc_ui,
            })),
            error: None,
            duration_ms: 0,
        })
    }
}

fn rpc_error_output(wallet_pubkey: &str, error: &str) -> ToolOutput {
    ToolOutput {
        tool_name: "get_wallet_balances".to_string(),
        success: false,
        data: Some(json!({
            "status": "rpc_error",
            "wallet_pubkey": wallet_pubkey,
            "error": error,
        })),
        error: Some(error.to_string()),
        duration_ms: 0,
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use claw_types::session::SessionId;
    use std::sync::Mutex;
    use uuid::Uuid;

    struct StubWallet {
        pubkey: Option<String>,
    }

    impl SessionBoundWallet for StubWallet {
        fn session_wallet_pubkey(&self, _: &SessionId) -> Option<String> {
            self.pubkey.clone()
        }
    }

    struct StubReader {
        sol: Mutex<Result<u64, String>>,
        token: Mutex<Result<Option<TokenAccountSnapshot>, String>>,
    }

    impl StubReader {
        fn ok(sol: u64, token: Option<TokenAccountSnapshot>) -> Self {
            Self {
                sol: Mutex::new(Ok(sol)),
                token: Mutex::new(Ok(token)),
            }
        }
    }

    #[async_trait]
    impl WalletBalanceReader for StubReader {
        async fn get_sol_lamports(&self, _: &str) -> Result<u64, String> {
            self.sol.lock().unwrap().clone()
        }
        async fn get_token_account(
            &self,
            _: &str,
        ) -> Result<Option<TokenAccountSnapshot>, String> {
            self.token.lock().unwrap().clone()
        }
    }

    fn input() -> ToolInput {
        ToolInput {
            tool_name: "get_wallet_balances".to_string(),
            parameters: json!({}),
            session_id: SessionId::from(Uuid::new_v4()),
            correlation_id: Uuid::new_v4(),
        }
    }

    #[tokio::test]
    async fn no_wallet_bound_returns_structured_status() {
        let tool = GetWalletBalancesTool::new(
            Arc::new(StubWallet { pubkey: None }),
            Arc::new(StubReader::ok(0, None)),
        );
        let out = tool.execute(input()).await.unwrap();
        assert!(!out.success);
        let data = out.data.unwrap();
        assert_eq!(data["status"], "wallet_not_bound");
        assert!(data["wallet_pubkey"].is_null());
        assert!(data["sol_lamports"].is_null());
    }

    #[tokio::test]
    async fn bound_wallet_returns_balances() {
        // Use a known good base58 mainnet pubkey so the ATA derivation
        // succeeds and the stub's owner field can match.
        const WALLET: &str = "3xTfBYx7Y7iC5HgKXTpe9eKJD1FH3v4qDcSFv6oxrt7P";
        let tool = GetWalletBalancesTool::new(
            Arc::new(StubWallet {
                pubkey: Some(WALLET.to_string()),
            }),
            Arc::new(StubReader::ok(
                1_234_567,
                Some(TokenAccountSnapshot {
                    mint: USDC_MINT_BS58.to_string(),
                    owner: WALLET.to_string(),
                    raw_amount: 50_000,
                }),
            )),
        );
        let out = tool.execute(input()).await.unwrap();
        assert!(out.success);
        let data = out.data.unwrap();
        assert_eq!(data["status"], "ok");
        assert_eq!(data["wallet_pubkey"], WALLET);
        assert_eq!(data["sol_lamports"], 1_234_567);
        assert_eq!(data["sol_ui"], "0.001234567");
        assert_eq!(data["usdc_mint"], USDC_MINT_BS58);
        assert_eq!(data["usdc_raw"], 50_000);
        assert_eq!(data["usdc_ui"], "0.050000");
        assert!(data["usdc_ata"].is_string());
    }

    #[tokio::test]
    async fn missing_usdc_ata_returns_zero_balance() {
        const WALLET: &str = "3xTfBYx7Y7iC5HgKXTpe9eKJD1FH3v4qDcSFv6oxrt7P";
        let tool = GetWalletBalancesTool::new(
            Arc::new(StubWallet {
                pubkey: Some(WALLET.to_string()),
            }),
            Arc::new(StubReader::ok(0, None)),
        );
        let out = tool.execute(input()).await.unwrap();
        let data = out.data.unwrap();
        assert_eq!(data["status"], "ok");
        assert_eq!(data["usdc_raw"], 0);
        assert_eq!(data["usdc_ui"], "0.000000");
        assert!(data["usdc_ata"].is_null());
    }

    #[tokio::test]
    async fn rpc_error_propagates_as_structured_status() {
        const WALLET: &str = "3xTfBYx7Y7iC5HgKXTpe9eKJD1FH3v4qDcSFv6oxrt7P";
        struct ErrReader;
        #[async_trait]
        impl WalletBalanceReader for ErrReader {
            async fn get_sol_lamports(&self, _: &str) -> Result<u64, String> {
                Err("rpc unreachable".to_string())
            }
            async fn get_token_account(
                &self,
                _: &str,
            ) -> Result<Option<TokenAccountSnapshot>, String> {
                Ok(None)
            }
        }
        let tool = GetWalletBalancesTool::new(
            Arc::new(StubWallet {
                pubkey: Some(WALLET.to_string()),
            }),
            Arc::new(ErrReader),
        );
        let out = tool.execute(input()).await.unwrap();
        let data = out.data.unwrap();
        assert_eq!(data["status"], "rpc_error");
        assert!(data["error"].as_str().unwrap().contains("rpc unreachable"));
    }

    #[test]
    fn schema_has_no_inputs_and_no_secret_fields() {
        let tool = GetWalletBalancesTool::new(
            Arc::new(StubWallet { pubkey: None }),
            Arc::new(StubReader::ok(0, None)),
        );
        let spec = tool.spec();
        assert_eq!(spec.name, "get_wallet_balances");
        assert!(spec.required_capabilities.is_empty());
        assert_eq!(spec.input_schema["additionalProperties"], json!(false));
        assert!(spec.input_schema["required"].as_array().unwrap().is_empty());
        assert!(spec.input_schema["properties"]
            .as_object()
            .unwrap()
            .is_empty());

        // No secret-shaped tokens in the spec text.
        let raw = serde_json::to_string(&spec.input_schema).unwrap()
            + &serde_json::to_string(&spec.output_schema).unwrap();
        for forbidden in [
            "tx_bytes",
            "transaction_base64",
            "signed_tx",
            "private_key",
            "keypair",
            "Authorization",
        ] {
            assert!(
                !raw.contains(forbidden),
                "get_wallet_balances schema must not mention `{forbidden}`; got {raw}"
            );
        }
    }

    // ── Phase 6C-C — known-vector regression for the canonical ATA fix ─────

    /// Known-vector ATA derivation: this catches the Phase 6C bug where
    /// the wrong ATA program id was hard-coded. The on-chain wallet
    /// `C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW` holds USDC at the
    /// associated token account `4bDArC4UVoBjecHUZaCKqhuhTYPoiKS4qpQQpxuvm1qn`
    /// (verified via mainnet RPC during live smoke). Any future regression
    /// in `derive_usdc_ata` will fail this assertion deterministically,
    /// without contacting the network.
    #[test]
    fn p6c_c_known_vector_usdc_ata_derivation() {
        const OWNER_BS58: &str = "C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW";
        const EXPECTED_USDC_ATA: &str = "4bDArC4UVoBjecHUZaCKqhuhTYPoiKS4qpQQpxuvm1qn";
        let owner = Pubkey::from_str(OWNER_BS58).expect("owner is valid base58");
        let derived = derive_usdc_ata(&owner);
        assert_eq!(
            derived.to_string(),
            EXPECTED_USDC_ATA,
            "USDC ATA derivation must match the canonical SPL helper"
        );
    }

    /// Phase 6C-C smoke-driven fixture: bound wallet C4QQ + StubReader
    /// returning the on-chain USDC balance (397_264 raw = 0.397264 USDC)
    /// must surface in the tool output WITH the correct ATA pubkey.
    /// Locks both the ATA derivation fix and the raw -> UI formatting.
    #[tokio::test]
    async fn p6c_c_known_balance_round_trip_for_c4qq_wallet() {
        const OWNER_BS58: &str = "C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW";
        const EXPECTED_USDC_ATA: &str = "4bDArC4UVoBjecHUZaCKqhuhTYPoiKS4qpQQpxuvm1qn";
        const KNOWN_USDC_RAW: u64 = 397_264;
        let tool = GetWalletBalancesTool::new(
            Arc::new(StubWallet {
                pubkey: Some(OWNER_BS58.to_string()),
            }),
            Arc::new(StubReader::ok(
                500_000_000,
                Some(TokenAccountSnapshot {
                    mint: USDC_MINT_BS58.to_string(),
                    owner: OWNER_BS58.to_string(),
                    raw_amount: KNOWN_USDC_RAW,
                }),
            )),
        );
        let out = tool.execute(input()).await.unwrap();
        assert!(out.success);
        let data = out.data.unwrap();
        assert_eq!(data["status"], "ok");
        assert_eq!(data["wallet_pubkey"], OWNER_BS58);
        assert_eq!(data["usdc_ata"], EXPECTED_USDC_ATA);
        assert_eq!(data["usdc_raw"], KNOWN_USDC_RAW);
        assert_eq!(data["usdc_ui"], "0.397264");
    }
}
