//! Jupiter Swap API client — foundation layer for policy-gated swap routing.
//!
//! # API surface
//!
//! - `POST /swap/v1/swap-instructions` — **main execution path**. Returns
//!   decomposed instructions (compute budget, setup, swap, cleanup, other)
//!   plus address lookup table addresses and blockhash. Claw assembles the
//!   transaction itself.
//! - `GET /swap/v1/quote` — **optional preview / informational only**. Provides
//!   route plan and estimated output for operator review. MUST NOT be used
//!   as the execution quote; the build response is authoritative.
//!
//! # Live shape notes (verified against `api.jup.ag` 2026-04-17)
//!
//! - `addressesByLookupTableAddress` is returned as `null`. Jupiter only
//!   returns the ALT *addresses* (in `addressLookupTableAddresses`); the
//!   address-list contents must be resolved client-side via RPC
//!   (`getAccount` on each ALT address, decode as `AddressLookupTable`).
//! - Extra fields (`tokenLedgerInstruction`, `prioritizationType`,
//!   `computeUnitLimit`, `simulationError`, `simulationSlot`, `timeTaken`,
//!   `createAtaTimeTaken`, `dynamicSlippageReport`, `fetchedAt`) are
//!   accepted and ignored — they are informational only and do not affect
//!   V0 assembly.
//!
//! # Not in scope
//!
//! - `/order` + `/execute` (Ultra API)
//! - Trigger / Limit orders
//! - Wiring into signing.rs or approval resume path
//! - ActionOutcome dispatch or multi-backend refactor

use std::collections::HashMap;

use async_trait::async_trait;
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;
use tracing::{info, warn};

/// Custom deserializer: accepts either absent, `null`, or a map for
/// `addresses_by_lookup_table_address`. The live Jupiter API returns
/// explicit `null`, which plain `#[serde(default)]` alone cannot handle.
fn null_or_map<'de, D>(deserializer: D) -> Result<HashMap<String, Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    let opt: Option<HashMap<String, Vec<String>>> = Option::deserialize(deserializer)?;
    Ok(opt.unwrap_or_default())
}

// ── Jupiter API base URL ───────────────────────────────────────────────────

/// Default Jupiter API base URL (hosts both /v6/quote and /swap/v2/build).
pub const JUPITER_API_BASE_URL: &str = "https://api.jup.ag";

// ── Error type ─────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum JupiterError {
    /// HTTP transport failure (DNS, timeout, connection refused).
    #[error("network error: {0}")]
    Network(String),

    /// Jupiter API returned a non-2xx response.
    #[error("API error (HTTP {status}): {message}")]
    Api {
        status: u16,
        message: String,
    },

    /// Response body could not be deserialized.
    #[error("deserialization error: {0}")]
    Deserialize(String),
}

// ── Shared types ───────────────────────────────────────────────────────────

/// Swap mode — determines which side of the swap is fixed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SwapMode {
    ExactIn,
    ExactOut,
}

impl Default for SwapMode {
    fn default() -> Self {
        Self::ExactIn
    }
}

// ── Quote types (preview / informational only) ─────────────────────────────

/// Parameters for `GET /v6/quote`.
///
/// Used for informational preview only. The execution path uses `build()`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SwapQuoteRequest {
    /// Input token mint address (base58).
    pub input_mint: String,
    /// Output token mint address (base58).
    pub output_mint: String,
    /// Amount in smallest unit of the input (ExactIn) or output (ExactOut) token.
    pub amount: u64,
    /// Maximum slippage in basis points (e.g., 50 = 0.5%).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slippage_bps: Option<u16>,
    /// Swap mode. Default: ExactIn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub swap_mode: Option<SwapMode>,
    /// If true, only return direct routes (no multi-hop).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub only_direct_routes: Option<bool>,
    /// Maximum number of accounts the transaction may use.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_accounts: Option<u8>,
}

/// Response from `GET /v6/quote`.
///
/// **Informational preview only.** This is NOT the execution quote.
/// Field names match Jupiter V6 JSON (camelCase). Amounts are strings
/// because they can exceed JavaScript's `Number.MAX_SAFE_INTEGER`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwapQuoteResponse {
    pub input_mint: String,
    pub in_amount: String,
    pub output_mint: String,
    pub out_amount: String,
    pub other_amount_threshold: String,
    pub swap_mode: SwapMode,
    pub slippage_bps: u16,
    #[serde(default)]
    pub price_impact_pct: Option<String>,
    #[serde(default)]
    pub route_plan: Vec<RoutePlanStep>,
    #[serde(default)]
    pub context_slot: Option<u64>,
}

impl SwapQuoteResponse {
    pub fn in_amount_u64(&self) -> Result<u64, JupiterError> {
        self.in_amount.parse().map_err(|e| JupiterError::Deserialize(
            format!("invalid in_amount '{}': {}", self.in_amount, e),
        ))
    }

    pub fn out_amount_u64(&self) -> Result<u64, JupiterError> {
        self.out_amount.parse().map_err(|e| JupiterError::Deserialize(
            format!("invalid out_amount '{}': {}", self.out_amount, e),
        ))
    }

    pub fn threshold_u64(&self) -> Result<u64, JupiterError> {
        self.other_amount_threshold.parse().map_err(|e| JupiterError::Deserialize(
            format!("invalid other_amount_threshold '{}': {}", self.other_amount_threshold, e),
        ))
    }

    /// All DEX labels in the route (e.g., ["Orca", "Raydium"]).
    pub fn route_labels(&self) -> Vec<&str> {
        self.route_plan
            .iter()
            .filter_map(|step| step.swap_info.label.as_deref())
            .collect()
    }
}

/// A single hop in the swap route.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutePlanStep {
    pub swap_info: SwapInfo,
    pub percent: u8,
}

/// AMM/DEX details for one hop in the route.
///
/// `fee_amount` and `fee_mint` are optional to match the live API, which
/// omits them for certain AMMs (e.g. HumidiFi, SolFi) while populating
/// them for others. Callers should not rely on their presence.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwapInfo {
    pub amm_key: String,
    #[serde(default)]
    pub label: Option<String>,
    pub input_mint: String,
    pub output_mint: String,
    pub in_amount: String,
    pub out_amount: String,
    #[serde(default)]
    pub fee_amount: Option<String>,
    #[serde(default)]
    pub fee_mint: Option<String>,
}

// ── Build types (main execution path — Swap V2) ───────────────────────────

/// Request body for `POST /swap/v2/build`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SwapBuildRequest {
    /// The quote response from `GET /v6/quote`.
    pub quote_response: SwapQuoteResponse,
    /// The wallet public key that will sign the transaction (base58).
    pub user_public_key: String,
    /// Whether to auto-wrap/unwrap SOL. Default: true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wrap_and_unwrap_sol: Option<bool>,
    /// Let Jupiter set the compute unit limit dynamically.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dynamic_compute_unit_limit: Option<bool>,
    /// Priority fee in lamports. Can be a number or the string `"auto"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prioritization_fee_lamports: Option<serde_json::Value>,
}

/// Response from `POST /swap/v1/swap-instructions`.
///
/// Returns decomposed instructions so Claw can assemble, inspect, and
/// sign the transaction itself. This is the authoritative execution data.
///
/// # ALT resolution (V0)
///
/// The live API returns `addresses_by_lookup_table_address` as `null`. Only
/// `address_lookup_table_addresses` (the list of ALT *addresses*) is populated.
/// Callers that build V0 transactions must resolve the contents themselves
/// via RPC — see `jupiter_alt::resolve_address_lookup_tables()`.
///
/// Tests and mocks may still supply the pre-resolved map; if present it
/// wins over RPC-side resolution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwapBuildResponse {
    /// Compute budget instructions (set CU limit, set CU price).
    #[serde(default)]
    pub compute_budget_instructions: Vec<JupiterInstruction>,
    /// Setup instructions (create ATAs, wrap SOL, etc.).
    #[serde(default)]
    pub setup_instructions: Vec<JupiterInstruction>,
    /// The core swap instruction.
    pub swap_instruction: JupiterInstruction,
    /// Cleanup instruction (unwrap SOL, close ephemeral accounts).
    #[serde(default)]
    pub cleanup_instruction: Option<JupiterInstruction>,
    /// Any other instructions Jupiter deems necessary.
    #[serde(default)]
    pub other_instructions: Vec<JupiterInstruction>,
    /// Address lookup table addresses (live shape). Each address is the
    /// base58 pubkey of an on-chain ALT whose contents must be resolved
    /// via RPC before V0 assembly.
    #[serde(default)]
    pub address_lookup_table_addresses: Vec<String>,
    /// Pre-resolved ALT contents. The live API currently returns this as
    /// `null`; tests and mocks may still populate it. When present, it
    /// takes precedence over RPC-side resolution.
    ///
    /// Key: lookup table address (base58). Value: list of account addresses.
    ///
    /// Accepts both missing and explicit-null in the wire form (the live
    /// response uses `null`, which plain `#[serde(default)]` would reject).
    #[serde(default, deserialize_with = "null_or_map")]
    pub addresses_by_lookup_table_address: HashMap<String, Vec<String>>,
    /// Blockhash and its validity window.
    pub blockhash_with_metadata: BlockhashWithMetadata,
    /// Optional token-ledger preamble instruction (not used for SOL↔SPL swaps).
    /// Present in the live response; ignored by V0 assembly unless set.
    #[serde(default)]
    pub token_ledger_instruction: Option<JupiterInstruction>,
}

impl SwapBuildResponse {
    /// Total number of instructions across all categories.
    pub fn total_instruction_count(&self) -> usize {
        self.compute_budget_instructions.len()
            + self.setup_instructions.len()
            + 1 // swap_instruction
            + if self.cleanup_instruction.is_some() { 1 } else { 0 }
            + self.other_instructions.len()
    }

    /// Unique program IDs referenced across all instructions.
    /// Useful for policy validation against a program allowlist.
    pub fn all_program_ids(&self) -> Vec<&str> {
        let mut ids: Vec<&str> = Vec::new();
        let all = self.compute_budget_instructions.iter()
            .chain(self.setup_instructions.iter())
            .chain(std::iter::once(&self.swap_instruction))
            .chain(self.cleanup_instruction.iter())
            .chain(self.other_instructions.iter());
        for ix in all {
            if !ids.contains(&ix.program_id.as_str()) {
                ids.push(&ix.program_id);
            }
        }
        ids
    }
}

/// A single Solana instruction in Jupiter's wire format.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JupiterInstruction {
    /// Program ID (base58).
    pub program_id: String,
    /// Ordered list of account inputs.
    pub accounts: Vec<JupiterAccountMeta>,
    /// Instruction data (base64-encoded).
    pub data: String,
}

/// Account metadata for a Jupiter instruction.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JupiterAccountMeta {
    /// Account public key (base58).
    pub pubkey: String,
    /// Whether this account must sign the transaction.
    pub is_signer: bool,
    /// Whether this account is written to.
    pub is_writable: bool,
}

/// Blockhash and its validity metadata.
///
/// The live Jupiter API serializes `blockhash` as a **32-byte array** in JSON
/// (e.g. `[21, 204, 175, ...]`), while older doc + fixture data used base58
/// strings. We accept both on the wire and normalize to base58 on the Rust
/// side so downstream `Hash::from_str` stays unchanged.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockhashWithMetadata {
    /// Recent blockhash (base58 — normalized from either string or byte-array wire form).
    #[serde(deserialize_with = "blockhash_string_or_bytes")]
    pub blockhash: String,
    /// Last block height at which this blockhash is valid.
    pub last_valid_block_height: u64,
    /// Live API includes a `fetchedAt` timestamp; accepted and ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetched_at: Option<serde_json::Value>,
}

/// Accept either a base58 string or a 32-byte array on the wire; emit base58.
fn blockhash_string_or_bytes<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::Error;

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Wire {
        Str(String),
        Bytes(Vec<u8>),
    }

    match Wire::deserialize(deserializer)? {
        Wire::Str(s) => Ok(s),
        Wire::Bytes(b) => {
            if b.len() != 32 {
                return Err(D::Error::custom(format!(
                    "blockhash byte array must be 32 bytes, got {}",
                    b.len()
                )));
            }
            Ok(bs58::encode(&b).into_string())
        }
    }
}

// ── Client trait ───────────────────────────────────────────────────────────

/// Async client for Jupiter swap API.
///
/// Trait-based so the control plane can inject a mock for testing.
#[async_trait]
pub trait JupiterClient: Send + Sync {
    /// Informational preview — fetch a quote for operator display.
    ///
    /// NOT part of the execution path. The returned data is suitable for
    /// `SwapPreview` but MUST NOT be treated as the execution quote.
    async fn quote(&self, request: &SwapQuoteRequest) -> Result<SwapQuoteResponse, JupiterError>;

    /// Main execution path — build decomposed swap instructions.
    ///
    /// Calls `POST /swap/v2/build`. Returns individual instructions,
    /// address lookup tables, and blockhash so Claw can assemble the
    /// transaction itself with full inspection and signing control.
    async fn build(&self, request: &SwapBuildRequest) -> Result<SwapBuildResponse, JupiterError>;
}

// ── HTTP implementation ────────────────────────────────────────────────────

/// Production Jupiter client backed by `reqwest`.
pub struct HttpJupiterClient {
    http: reqwest::Client,
    base_url: String,
}

impl HttpJupiterClient {
    /// Create a client pointing at the default Jupiter API.
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: JUPITER_API_BASE_URL.to_string(),
        }
    }

    /// Create a client with a custom base URL (for staging/testing).
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.into(),
        }
    }
}

#[async_trait]
impl JupiterClient for HttpJupiterClient {
    async fn quote(&self, request: &SwapQuoteRequest) -> Result<SwapQuoteResponse, JupiterError> {
        let url = format!("{}/swap/v1/quote", self.base_url);

        info!(
            input_mint = %request.input_mint,
            output_mint = %request.output_mint,
            amount = request.amount,
            "requesting Jupiter quote (preview)"
        );

        let amount_str = request.amount.to_string();
        let mut query: Vec<(&str, &str)> = vec![
            ("inputMint", request.input_mint.as_str()),
            ("outputMint", request.output_mint.as_str()),
            ("amount", amount_str.as_str()),
        ];
        let slippage_str;
        if let Some(bps) = request.slippage_bps {
            slippage_str = bps.to_string();
            query.push(("slippageBps", slippage_str.as_str()));
        }

        let resp = self.http
            .get(&url)
            .query(&query)
            .send()
            .await
            .map_err(|e| JupiterError::Network(e.to_string()))?;

        let status = resp.status().as_u16();
        if status >= 400 {
            let body = resp.text().await.unwrap_or_default();
            warn!(status, body = %body, "Jupiter quote API error");
            return Err(JupiterError::Api { status, message: body });
        }

        resp.json::<SwapQuoteResponse>()
            .await
            .map_err(|e| JupiterError::Deserialize(e.to_string()))
    }

    async fn build(&self, request: &SwapBuildRequest) -> Result<SwapBuildResponse, JupiterError> {
        let url = format!("{}/swap/v1/swap-instructions", self.base_url);

        info!(
            input_mint = %request.quote_response.input_mint,
            output_mint = %request.quote_response.output_mint,
            user = %request.user_public_key,
            "requesting Jupiter swap-instructions (v1)"
        );

        let resp = self.http
            .post(&url)
            .json(request)
            .send()
            .await
            .map_err(|e| JupiterError::Network(e.to_string()))?;

        let status = resp.status().as_u16();
        if status >= 400 {
            let body = resp.text().await.unwrap_or_default();
            warn!(status, body = %body, "Jupiter swap-instructions API error");
            return Err(JupiterError::Api { status, message: body });
        }

        resp.json::<SwapBuildResponse>()
            .await
            .map_err(|e| JupiterError::Deserialize(e.to_string()))
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Mock client ────────────────────────────────────────────────────

    struct MockJupiterClient {
        quote_response: Result<SwapQuoteResponse, JupiterError>,
        build_response: Result<SwapBuildResponse, JupiterError>,
    }

    impl MockJupiterClient {
        fn success(quote: SwapQuoteResponse, build: SwapBuildResponse) -> Self {
            Self {
                quote_response: Ok(quote),
                build_response: Ok(build),
            }
        }

        fn quote_error(err: JupiterError) -> Self {
            Self {
                quote_response: Err(err),
                build_response: Ok(dummy_build_response()),
            }
        }

        fn build_error(err: JupiterError) -> Self {
            Self {
                quote_response: Ok(dummy_quote_response()),
                build_response: Err(err),
            }
        }
    }

    #[async_trait]
    impl JupiterClient for MockJupiterClient {
        async fn quote(&self, _request: &SwapQuoteRequest) -> Result<SwapQuoteResponse, JupiterError> {
            match &self.quote_response {
                Ok(r) => Ok(r.clone()),
                Err(e) => Err(match e {
                    JupiterError::Network(m) => JupiterError::Network(m.clone()),
                    JupiterError::Api { status, message } => JupiterError::Api {
                        status: *status, message: message.clone(),
                    },
                    JupiterError::Deserialize(m) => JupiterError::Deserialize(m.clone()),
                }),
            }
        }

        async fn build(&self, _request: &SwapBuildRequest) -> Result<SwapBuildResponse, JupiterError> {
            match &self.build_response {
                Ok(r) => Ok(r.clone()),
                Err(e) => Err(match e {
                    JupiterError::Network(m) => JupiterError::Network(m.clone()),
                    JupiterError::Api { status, message } => JupiterError::Api {
                        status: *status, message: message.clone(),
                    },
                    JupiterError::Deserialize(m) => JupiterError::Deserialize(m.clone()),
                }),
            }
        }
    }

    // ── Test fixtures ──────────────────────────────────────────────────

    const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
    const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";
    const COMPUTE_BUDGET_PROGRAM: &str = "ComputeBudget111111111111111111111111111111";
    const SYSTEM_PROGRAM: &str = "11111111111111111111111111111111";
    const JUPITER_PROGRAM: &str = "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4";
    const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

    fn dummy_instruction(program_id: &str) -> JupiterInstruction {
        JupiterInstruction {
            program_id: program_id.to_string(),
            accounts: vec![
                JupiterAccountMeta {
                    pubkey: "wallet_pubkey_placeholder".to_string(),
                    is_signer: true,
                    is_writable: true,
                },
            ],
            data: "AQAAAA==".to_string(), // base64 of [1, 0, 0, 0]
        }
    }

    fn dummy_quote_response() -> SwapQuoteResponse {
        SwapQuoteResponse {
            input_mint: USDC_MINT.to_string(),
            in_amount: "100000000".to_string(),
            output_mint: WSOL_MINT.to_string(),
            out_amount: "670000000".to_string(),
            other_amount_threshold: "666650000".to_string(),
            swap_mode: SwapMode::ExactIn,
            slippage_bps: 50,
            price_impact_pct: Some("0.0012".to_string()),
            route_plan: vec![
                RoutePlanStep {
                    swap_info: SwapInfo {
                        amm_key: "pool_abc123".to_string(),
                        label: Some("Orca".to_string()),
                        input_mint: USDC_MINT.to_string(),
                        output_mint: WSOL_MINT.to_string(),
                        in_amount: "100000000".to_string(),
                        out_amount: "670000000".to_string(),
                        fee_amount: Some("25000".to_string()),
                        fee_mint: Some(USDC_MINT.to_string()),
                    },
                    percent: 100,
                },
            ],
            context_slot: Some(312_000_000),
        }
    }

    fn dummy_build_response() -> SwapBuildResponse {
        SwapBuildResponse {
            compute_budget_instructions: vec![
                dummy_instruction(COMPUTE_BUDGET_PROGRAM),
                dummy_instruction(COMPUTE_BUDGET_PROGRAM),
            ],
            setup_instructions: vec![
                dummy_instruction(SYSTEM_PROGRAM),
                dummy_instruction(TOKEN_PROGRAM),
            ],
            swap_instruction: dummy_instruction(JUPITER_PROGRAM),
            cleanup_instruction: Some(dummy_instruction(TOKEN_PROGRAM)),
            other_instructions: vec![],
            address_lookup_table_addresses: vec!["LookupTable111111111111111111111111111111".to_string()],
            addresses_by_lookup_table_address: {
                let mut m = HashMap::new();
                m.insert(
                    "LookupTable111111111111111111111111111111".to_string(),
                    vec![USDC_MINT.to_string(), WSOL_MINT.to_string()],
                );
                m
            },
            blockhash_with_metadata: BlockhashWithMetadata {
                blockhash: "GHtXQBpokKZYzitZ9bHnDZbUXZwqfsFQ2NUjRmiaV5Hf".to_string(),
                last_valid_block_height: 312_000_200,
                fetched_at: None,
            },
            token_ledger_instruction: None,
        }
    }

    fn dummy_quote_request() -> SwapQuoteRequest {
        SwapQuoteRequest {
            input_mint: USDC_MINT.to_string(),
            output_mint: WSOL_MINT.to_string(),
            amount: 100_000_000,
            slippage_bps: Some(50),
            swap_mode: None,
            only_direct_routes: None,
            max_accounts: None,
        }
    }

    fn dummy_build_request(quote: SwapQuoteResponse) -> SwapBuildRequest {
        SwapBuildRequest {
            quote_response: quote,
            user_public_key: "CL4w7VNnCpGh3oJ8qfPFR4RjV6R3K4fZ1234567890ab".to_string(),
            wrap_and_unwrap_sol: Some(true),
            dynamic_compute_unit_limit: Some(true),
            prioritization_fee_lamports: None,
        }
    }

    // ── Quote tests (preview / informational) ──────────────────────────

    #[tokio::test]
    async fn quote_returns_expected_response() {
        let client = MockJupiterClient::success(
            dummy_quote_response(),
            dummy_build_response(),
        );
        let resp = client.quote(&dummy_quote_request()).await.unwrap();

        assert_eq!(resp.input_mint, USDC_MINT);
        assert_eq!(resp.output_mint, WSOL_MINT);
        assert_eq!(resp.in_amount, "100000000");
        assert_eq!(resp.slippage_bps, 50);
        assert_eq!(resp.swap_mode, SwapMode::ExactIn);
    }

    #[tokio::test]
    async fn quote_amount_parsing() {
        let resp = dummy_quote_response();
        assert_eq!(resp.in_amount_u64().unwrap(), 100_000_000);
        assert_eq!(resp.out_amount_u64().unwrap(), 670_000_000);
        assert_eq!(resp.threshold_u64().unwrap(), 666_650_000);
    }

    #[tokio::test]
    async fn quote_route_labels_extracts_dex_names() {
        let resp = dummy_quote_response();
        assert_eq!(resp.route_labels(), vec!["Orca"]);
    }

    #[tokio::test]
    async fn multi_hop_route_labels() {
        let mut resp = dummy_quote_response();
        resp.route_plan.push(RoutePlanStep {
            swap_info: SwapInfo {
                amm_key: "pool_def456".to_string(),
                label: Some("Raydium".to_string()),
                input_mint: WSOL_MINT.to_string(),
                output_mint: "mSoL_mint".to_string(),
                in_amount: "670000000".to_string(),
                out_amount: "650000000".to_string(),
                fee_amount: Some("10000".to_string()),
                fee_mint: Some(WSOL_MINT.to_string()),
            },
            percent: 100,
        });
        assert_eq!(resp.route_labels(), vec!["Orca", "Raydium"]);
    }

    #[tokio::test]
    async fn invalid_amount_string_returns_deserialize_error() {
        let mut resp = dummy_quote_response();
        resp.in_amount = "not_a_number".to_string();
        match resp.in_amount_u64().unwrap_err() {
            JupiterError::Deserialize(msg) => assert!(msg.contains("not_a_number")),
            other => panic!("expected Deserialize, got: {other}"),
        }
    }

    // ── Build tests (main execution path) ──────────────────────────────

    #[tokio::test]
    async fn build_returns_decomposed_instructions() {
        let quote = dummy_quote_response();
        let client = MockJupiterClient::success(quote.clone(), dummy_build_response());
        let resp = client.build(&dummy_build_request(quote)).await.unwrap();

        assert_eq!(resp.compute_budget_instructions.len(), 2);
        assert_eq!(resp.setup_instructions.len(), 2);
        assert_eq!(resp.swap_instruction.program_id, JUPITER_PROGRAM);
        assert!(resp.cleanup_instruction.is_some());
        assert!(resp.other_instructions.is_empty());
    }

    #[tokio::test]
    async fn build_returns_lookup_tables() {
        let quote = dummy_quote_response();
        let client = MockJupiterClient::success(quote.clone(), dummy_build_response());
        let resp = client.build(&dummy_build_request(quote)).await.unwrap();

        assert_eq!(resp.addresses_by_lookup_table_address.len(), 1);
        let addresses = resp.addresses_by_lookup_table_address
            .get("LookupTable111111111111111111111111111111")
            .unwrap();
        assert_eq!(addresses.len(), 2);
        assert!(addresses.contains(&USDC_MINT.to_string()));
    }

    #[tokio::test]
    async fn build_returns_blockhash() {
        let quote = dummy_quote_response();
        let client = MockJupiterClient::success(quote.clone(), dummy_build_response());
        let resp = client.build(&dummy_build_request(quote)).await.unwrap();

        assert!(!resp.blockhash_with_metadata.blockhash.is_empty());
        assert!(resp.blockhash_with_metadata.last_valid_block_height > 0);
    }

    #[tokio::test]
    async fn build_total_instruction_count() {
        let resp = dummy_build_response();
        // 2 compute_budget + 2 setup + 1 swap + 1 cleanup + 0 other = 6
        assert_eq!(resp.total_instruction_count(), 6);
    }

    #[tokio::test]
    async fn build_total_instruction_count_without_cleanup() {
        let mut resp = dummy_build_response();
        resp.cleanup_instruction = None;
        assert_eq!(resp.total_instruction_count(), 5);
    }

    #[tokio::test]
    async fn build_all_program_ids_deduplicates() {
        let resp = dummy_build_response();
        let ids = resp.all_program_ids();
        // ComputeBudget (x2 deduped), System, Token (setup), Jupiter (swap), Token (cleanup deduped)
        assert_eq!(ids.len(), 4);
        assert!(ids.contains(&COMPUTE_BUDGET_PROGRAM));
        assert!(ids.contains(&SYSTEM_PROGRAM));
        assert!(ids.contains(&JUPITER_PROGRAM));
        assert!(ids.contains(&TOKEN_PROGRAM));
    }

    // ── Error propagation ──────────────────────────────────────────────

    #[tokio::test]
    async fn quote_api_error_propagates() {
        let client = MockJupiterClient::quote_error(JupiterError::Api {
            status: 400,
            message: "Invalid mint address".to_string(),
        });
        match client.quote(&dummy_quote_request()).await.unwrap_err() {
            JupiterError::Api { status, message } => {
                assert_eq!(status, 400);
                assert!(message.contains("Invalid mint"));
            }
            other => panic!("expected Api, got: {other}"),
        }
    }

    #[tokio::test]
    async fn build_network_error_propagates() {
        let quote = dummy_quote_response();
        let client = MockJupiterClient::build_error(JupiterError::Network(
            "connection refused".to_string(),
        ));
        match client.build(&dummy_build_request(quote)).await.unwrap_err() {
            JupiterError::Network(msg) => assert!(msg.contains("connection refused")),
            other => panic!("expected Network, got: {other}"),
        }
    }

    // ── Serde round-trips ──────────────────────────────────────────────

    #[tokio::test]
    async fn quote_response_round_trips_through_json() {
        let original = dummy_quote_response();
        let json = serde_json::to_string(&original).unwrap();
        let parsed: SwapQuoteResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.input_mint, original.input_mint);
        assert_eq!(parsed.in_amount, original.in_amount);
        assert_eq!(parsed.slippage_bps, original.slippage_bps);
        assert_eq!(parsed.route_plan.len(), original.route_plan.len());
    }

    #[tokio::test]
    async fn build_response_round_trips_through_json() {
        let original = dummy_build_response();
        let json = serde_json::to_string(&original).unwrap();
        let parsed: SwapBuildResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.compute_budget_instructions.len(), 2);
        assert_eq!(parsed.swap_instruction.program_id, JUPITER_PROGRAM);
        assert_eq!(parsed.blockhash_with_metadata.blockhash, original.blockhash_with_metadata.blockhash);
        assert_eq!(parsed.addresses_by_lookup_table_address.len(), 1);
    }

    #[tokio::test]
    async fn quote_request_serializes_to_camel_case() {
        let req = dummy_quote_request();
        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("inputMint").is_some());
        assert!(json.get("outputMint").is_some());
        assert!(json.get("slippageBps").is_some());
    }

    #[tokio::test]
    async fn build_response_deserializes_camel_case_wire_format() {
        // Simulate the exact JSON shape Jupiter returns.
        let wire_json = serde_json::json!({
            "computeBudgetInstructions": [{
                "programId": COMPUTE_BUDGET_PROGRAM,
                "accounts": [],
                "data": "AQAAAA=="
            }],
            "setupInstructions": [],
            "swapInstruction": {
                "programId": JUPITER_PROGRAM,
                "accounts": [{ "pubkey": "abc", "isSigner": false, "isWritable": true }],
                "data": "AgAAAA=="
            },
            "cleanupInstruction": null,
            "otherInstructions": [],
            "addressesByLookupTableAddress": {
                "LUT123": ["addr1", "addr2"]
            },
            "blockhashWithMetadata": {
                "blockhash": "GHtXQBpokKZYzitZ9bHnDZbUXZwqfsFQ2NUjRmiaV5Hf",
                "lastValidBlockHeight": 999
            }
        });
        let resp: SwapBuildResponse = serde_json::from_value(wire_json).unwrap();
        assert_eq!(resp.compute_budget_instructions.len(), 1);
        assert_eq!(resp.swap_instruction.program_id, JUPITER_PROGRAM);
        assert_eq!(resp.swap_instruction.accounts[0].pubkey, "abc");
        assert!(resp.cleanup_instruction.is_none());
        assert_eq!(resp.blockhash_with_metadata.last_valid_block_height, 999);
        assert!(resp.addresses_by_lookup_table_address.contains_key("LUT123"));
    }

    #[tokio::test]
    async fn swap_mode_serializes_correctly() {
        assert_eq!(serde_json::to_string(&SwapMode::ExactIn).unwrap(), "\"ExactIn\"");
        assert_eq!(serde_json::to_string(&SwapMode::ExactOut).unwrap(), "\"ExactOut\"");
        let parsed: SwapMode = serde_json::from_str("\"ExactIn\"").unwrap();
        assert_eq!(parsed, SwapMode::ExactIn);
    }

    #[test]
    fn trait_is_object_safe() {
        fn _assert_object_safe(_: &dyn JupiterClient) {}
    }
}
