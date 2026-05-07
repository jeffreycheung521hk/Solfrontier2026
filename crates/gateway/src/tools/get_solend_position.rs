//! Phase 6H — read-only Solend / Save position scanner.
//!
//! Answers the operator question "where is my Solend USDC deposit?"
//! WITHOUT signing, broadcasting, creating an approval, or otherwise
//! mutating chain state. The tool is `required_capabilities: vec![]`
//! and structurally cannot exit through any signing path —
//! `module_has_no_forbidden_runtime_references` enforces that on every
//! `cargo test --lib`.
//!
//! # Discovery strategy
//!
//! Solend obligation accounts are owned by the Solend token-lending
//! program and have a fixed serialized length of [`OBLIGATION_LEN`]
//! (1300 bytes). The owner's `Pubkey` lives at byte offset
//! [`OBL_OWNER_OFF`] (42). A bounded `getProgramAccounts` call with
//! both filters — `dataSize == 1300` AND
//! `memcmp(offset=42, bytes=session_wallet_pubkey)` — discovers all
//! obligations belonging to a wallet without a wide unfiltered scan.
//!
//! Each returned obligation is then decoded by the existing
//! [`decode_obligation`] from `crate::integrations::solend::raw`. The
//! tool surfaces the decoded shape (lending market, deposits, borrows)
//! and flags any deposit whose `deposit_reserve` matches the configured
//! USDC reserve.
//!
//! # USDC value estimate
//!
//! Phase 6H deliberately does NOT compute the cToken → underlying-USDC
//! exchange rate. The deposited cToken amount (`deposited_amount`) is
//! reported exactly; the supplied-USDC estimate is reported as
//! `null` with a clear `estimate_unavailable_reason` string. A future
//! slice may add the reserve-driven exchange-rate decode; this slice
//! never invents a number.
//!
//! # What this tool deliberately does NOT do
//!
//! - Does NOT sign, broadcast, or create an approval.
//! - Does NOT create any signing handoff or rebuild a transaction.
//! - Does NOT mutate parked Solend artifacts or the lifecycle store.
//! - Does NOT register a withdraw tool. Agent B's withdraw substrate
//!   stays dead-source; this tool only READS obligations.

use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use solana_sdk::pubkey::Pubkey;

use claw_tool_system::{errors::ToolError, tool::Tool};
use claw_types::tool::{ToolInput, ToolOutput, ToolSpec};

use crate::integrations::solend::raw::{
    decode_obligation, SolendObligationCollateralRaw, SolendObligationLiquidityRaw,
    SolendObligationRaw, OBLIGATION_LEN, SOLEND_PROGRAM_ID_BS58,
};
use crate::tools::jupiter_swap::SessionBoundWallet;

/// Mainnet USDC mint (6 decimals).
pub const USDC_MINT_BS58: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
/// Solend / Save Main Pool USDC reserve (live target of Phase 6G's 5
/// USDC deposit). Held as a constant so the tool's "is this a USDC
/// deposit?" check is a string comparison against a known mainnet
/// reserve, not an oracle round-trip.
pub const SOLEND_MAIN_POOL_USDC_RESERVE_BS58: &str =
    "BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw";

/// Stable status set surfaced by the tool. Wire-stable strings; the
/// frontend renders on these. Reordering / renaming is a wire change.
const STATUS_OK: &str = "ok";
const STATUS_WALLET_NOT_BOUND: &str = "wallet_not_bound";
const STATUS_NO_POSITION: &str = "no_position";
const STATUS_RPC_ERROR: &str = "rpc_error";
const STATUS_DECODE_ERROR: &str = "decode_error";

/// Tool name. Single source of truth — used by registry + chat
/// allowlist + p5d2 route tests.
pub const TOOL_NAME: &str = "get_solend_position";

// ── Reader seam ─────────────────────────────────────────────────────────────

/// Narrow read-only RPC seam. Production [`RpcSolendPositionReader`]
/// wraps an `Arc<RpcPool>` and uses the same `read_client()` the rest
/// of the gateway uses; tests inject a stub recording mock.
///
/// Two methods:
/// 1. [`Self::find_obligations_for_owner`] — bounded
///    `getProgramAccounts` with `dataSize=1300` + memcmp on the owner
///    field at offset 42.
/// 2. [`Self::get_account_data`] — single `getAccountInfo`. Used to
///    fetch a reserve account by pubkey if one of the obligation
///    deposits points to a reserve we want to confirm.
#[async_trait]
pub trait SolendPositionReader: Send + Sync {
    /// Return `(pubkey, raw_account_data)` for every obligation owned
    /// by `owner_wallet`. Empty vec means no obligations exist (RPC
    /// returned no rows; not an error).
    async fn find_obligations_for_owner(
        &self,
        owner_wallet: &Pubkey,
    ) -> Result<Vec<(Pubkey, Vec<u8>)>, String>;

    /// Read raw account data for `pubkey` if it exists.
    async fn get_account_data(&self, pubkey: &Pubkey) -> Result<Option<Vec<u8>>, String>;
}

// ── Production reader ──────────────────────────────────────────────────────

/// Production [`SolendPositionReader`] backed by the shared
/// `Arc<RpcPool>`. Uses the same `read_client()` path the Solend
/// confirmation tracker and assembler already use, so circuit-breaker
/// state and endpoint selection stay consistent across the gateway.
///
/// Discovery uses `getProgramAccounts` with both filters applied:
/// `dataSize == 1300` AND `memcmp(offset=42, owner_pubkey)`. This
/// bounds the result set to obligations actually owned by the wallet
/// — no broad unfiltered scan.
pub struct RpcSolendPositionReader {
    rpc_pool: std::sync::Arc<claw_solana_core::rpc::RpcPool>,
    program_id_pk: Pubkey,
}

impl RpcSolendPositionReader {
    pub fn new(rpc_pool: std::sync::Arc<claw_solana_core::rpc::RpcPool>) -> Self {
        Self {
            rpc_pool,
            program_id_pk: Pubkey::from_str(SOLEND_PROGRAM_ID_BS58)
                .expect("hard-coded Solend program id parses"),
        }
    }
}

#[async_trait]
impl SolendPositionReader for RpcSolendPositionReader {
    async fn find_obligations_for_owner(
        &self,
        owner_wallet: &Pubkey,
    ) -> Result<Vec<(Pubkey, Vec<u8>)>, String> {
        use solana_client::rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig};
        use solana_client::rpc_filter::{Memcmp, RpcFilterType};
        use solana_sdk::commitment_config::CommitmentConfig;

        let client = self
            .rpc_pool
            .read_client()
            .map_err(|e| format!("rpc pool: {e}"))?;
        let config = RpcProgramAccountsConfig {
            filters: Some(vec![
                RpcFilterType::DataSize(OBLIGATION_LEN as u64),
                RpcFilterType::Memcmp(Memcmp::new_raw_bytes(
                    /*offset=*/ 42,
                    owner_wallet.to_bytes().to_vec(),
                )),
            ]),
            account_config: RpcAccountInfoConfig {
                encoding: Some(solana_account_decoder::UiAccountEncoding::Base64),
                commitment: Some(CommitmentConfig::confirmed()),
                data_slice: None,
                min_context_slot: None,
            },
            with_context: Some(false),
            sort_results: None,
        };
        let result = client
            .get_program_accounts_with_config(&self.program_id_pk, config)
            .await;
        match result {
            Ok(rows) => {
                self.rpc_pool.record_success(&client.url());
                Ok(rows
                    .into_iter()
                    .map(|(pk, acc)| (pk, acc.data))
                    .collect())
            }
            Err(e) => {
                self.rpc_pool.record_failure(&client.url());
                Err(format!("getProgramAccounts (solend obligations): {e}"))
            }
        }
    }

    async fn get_account_data(&self, pubkey: &Pubkey) -> Result<Option<Vec<u8>>, String> {
        use solana_sdk::commitment_config::CommitmentConfig;

        let client = self
            .rpc_pool
            .read_client()
            .map_err(|e| format!("rpc pool: {e}"))?;
        let resp = client
            .get_account_with_commitment(pubkey, CommitmentConfig::confirmed())
            .await;
        match resp {
            Ok(r) => {
                self.rpc_pool.record_success(&client.url());
                Ok(r.value.map(|a| a.data))
            }
            Err(e) => {
                self.rpc_pool.record_failure(&client.url());
                Err(format!("getAccountInfo: {e}"))
            }
        }
    }
}

// ── Tool ────────────────────────────────────────────────────────────────────

/// Read-only chat tool that scans the chain for Solend obligations
/// owned by the session-bound wallet and reports the decoded shape.
pub struct GetSolendPositionTool {
    session_wallet_lookup: Arc<dyn SessionBoundWallet>,
    reader: Arc<dyn SolendPositionReader>,
    /// Pubkey of the Solend / Save Main Pool USDC reserve. Stored as
    /// a `Pubkey` so deposit checks are constant-time `==` compares.
    usdc_reserve_pk: Pubkey,
    /// Pubkey of the Solend program. Used by the production reader to
    /// scope `getProgramAccounts`; held here too for output display
    /// (`program_id` field).
    program_id_pk: Pubkey,
}

impl GetSolendPositionTool {
    pub fn new(
        session_wallet_lookup: Arc<dyn SessionBoundWallet>,
        reader: Arc<dyn SolendPositionReader>,
    ) -> Self {
        Self {
            session_wallet_lookup,
            reader,
            usdc_reserve_pk: Pubkey::from_str(SOLEND_MAIN_POOL_USDC_RESERVE_BS58)
                .expect("hard-coded USDC reserve pubkey parses"),
            program_id_pk: Pubkey::from_str(SOLEND_PROGRAM_ID_BS58)
                .expect("hard-coded Solend program id parses"),
        }
    }
}

#[async_trait]
impl Tool for GetSolendPositionTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: TOOL_NAME.to_string(),
            description: "Read-only scanner for the session wallet's Solend / Save \
                          USDC obligation. No signing, no broadcast, no approval. \
                          Use when the user asks where their Solend deposit lives \
                          on-chain."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false,
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "status": { "type": "string" },
                },
                "required": ["status"],
            }),
            required_capabilities: vec![],
            supports_streaming: false,
            timeout_ms: 8000,
        }
    }

    async fn execute(&self, input: ToolInput) -> Result<ToolOutput, ToolError> {
        // ── Resolve session-bound wallet ──────────────────────────────
        let wallet_bs58 = match self
            .session_wallet_lookup
            .session_wallet_pubkey(&input.session_id)
        {
            Some(b) => b,
            None => return Ok(wallet_not_bound_output()),
        };
        let wallet_pk = match Pubkey::from_str(&wallet_bs58) {
            Ok(pk) => pk,
            Err(e) => {
                return Ok(decode_error_output(&format!(
                    "session wallet binding is not a valid pubkey: {e}"
                )))
            }
        };

        // ── Discover obligations owned by this wallet ─────────────────
        let raw_obligations = match self.reader.find_obligations_for_owner(&wallet_pk).await {
            Ok(v) => v,
            Err(e) => return Ok(rpc_error_output("find_obligations_for_owner", &e)),
        };

        if raw_obligations.is_empty() {
            return Ok(no_position_output(&wallet_bs58, &self.program_id_pk));
        }

        // ── Decode each obligation, collect positions ─────────────────
        let mut positions: Vec<Value> = Vec::with_capacity(raw_obligations.len() * 2);
        let mut decode_errors: Vec<String> = Vec::new();
        let mut lending_market_first: Option<Pubkey> = None;

        for (obligation_pk, data) in raw_obligations.iter() {
            if data.len() != OBLIGATION_LEN {
                decode_errors.push(format!(
                    "{} (got {} bytes, expected {})",
                    obligation_pk,
                    data.len(),
                    OBLIGATION_LEN
                ));
                continue;
            }
            let decoded: SolendObligationRaw = match decode_obligation(data) {
                Ok(o) => o,
                Err(e) => {
                    decode_errors.push(format!("{obligation_pk}: {e}"));
                    continue;
                }
            };
            if lending_market_first.is_none() {
                lending_market_first = Some(decoded.lending_market);
            }
            for collateral in decoded.deposits.iter() {
                positions.push(position_entry(
                    obligation_pk,
                    &decoded,
                    collateral,
                    &self.usdc_reserve_pk,
                ));
            }
            // Borrows are reported in a parallel array so callers can
            // detect `has_borrow == true` even if the deposit list is
            // unrelated to the borrow.
            for liq in decoded.borrows.iter() {
                positions.push(borrow_entry(obligation_pk, &decoded, liq));
            }
        }

        // If every obligation failed to decode, surface decode_error.
        if positions.is_empty() && !decode_errors.is_empty() {
            return Ok(decode_error_output(&format!(
                "failed to decode {} obligation(s): {}",
                decode_errors.len(),
                decode_errors.join("; ")
            )));
        }

        // ── Final ok output ───────────────────────────────────────────
        Ok(ok_output(
            &wallet_bs58,
            &self.program_id_pk,
            lending_market_first.as_ref(),
            &self.usdc_reserve_pk,
            positions,
            &decode_errors,
        ))
    }
}

// ── Output helpers ──────────────────────────────────────────────────────────

fn dashboard_visibility_note() -> &'static str {
    "Solend / Save dashboards rely on obligation discovery heuristics that may not \
     pick up obligations whose owner is the session wallet but whose obligation \
     account is freshly initialised. The on-chain truth is the obligation account \
     decoded above; if `positions` contains a USDC-reserve entry the deposit is \
     real and recoverable via the obligation pubkey."
}

fn supplied_usdc_estimate_unavailable_reason() -> &'static str {
    "Phase 6H reports the deposited cToken amount exactly; the underlying USDC \
     estimate requires the reserve's cToken-supply / total-liquidity exchange \
     rate, which a follow-up slice will decode. No estimate is invented here."
}

fn position_entry(
    obligation_pk: &Pubkey,
    obligation: &SolendObligationRaw,
    collateral: &SolendObligationCollateralRaw,
    usdc_reserve: &Pubkey,
) -> Value {
    let is_usdc = collateral.deposit_reserve == *usdc_reserve;
    json!({
        "kind":                              "deposit",
        "obligation_pubkey":                 obligation_pk.to_string(),
        "owner_pubkey":                      obligation.owner.to_string(),
        "lending_market":                    obligation.lending_market.to_string(),
        "reserve_pubkey":                    collateral.deposit_reserve.to_string(),
        "is_usdc_main_pool_reserve":         is_usdc,
        "deposited_collateral_amount_raw":   collateral.deposited_amount.to_string(),
        "supplied_usdc_estimate_raw":        Value::Null,
        "supplied_usdc_estimate_ui":         Value::Null,
        "estimate_unavailable_reason":       supplied_usdc_estimate_unavailable_reason(),
        "borrowed_amount_raw":               Value::Null,
        "borrowed_amount_ui":                Value::Null,
        "has_borrow":                        false,
        "source":                            "obligation_scan",
    })
}

fn borrow_entry(
    obligation_pk: &Pubkey,
    obligation: &SolendObligationRaw,
    liq: &SolendObligationLiquidityRaw,
) -> Value {
    json!({
        "kind":                              "borrow",
        "obligation_pubkey":                 obligation_pk.to_string(),
        "owner_pubkey":                      obligation.owner.to_string(),
        "lending_market":                    obligation.lending_market.to_string(),
        "reserve_pubkey":                    liq.borrow_reserve.to_string(),
        "is_usdc_main_pool_reserve":         false,
        "deposited_collateral_amount_raw":   Value::Null,
        "supplied_usdc_estimate_raw":        Value::Null,
        "supplied_usdc_estimate_ui":         Value::Null,
        "estimate_unavailable_reason":       Value::Null,
        // Wad-scaled (10^18); UI conversion is consumer's responsibility.
        "borrowed_amount_raw":               liq.borrowed_amount_wads.to_string(),
        "borrowed_amount_ui":                Value::Null,
        "has_borrow":                        true,
        "source":                            "obligation_scan",
    })
}

fn ok_output(
    wallet_bs58: &str,
    program_id: &Pubkey,
    lending_market: Option<&Pubkey>,
    usdc_reserve: &Pubkey,
    positions: Vec<Value>,
    decode_errors: &[String],
) -> ToolOutput {
    let usdc_position_count = positions
        .iter()
        .filter(|p| {
            p["is_usdc_main_pool_reserve"].as_bool().unwrap_or(false)
                && p["kind"].as_str() == Some("deposit")
        })
        .count();
    ToolOutput {
        tool_name: TOOL_NAME.to_string(),
        success: true,
        data: Some(json!({
            "status":                       STATUS_OK,
            "wallet_pubkey":                wallet_bs58,
            "network":                      "mainnet",
            "protocol":                     "Solend/Save",
            "program_id":                   program_id.to_string(),
            "lending_market":               lending_market.map(|m| m.to_string()),
            "usdc_main_pool_reserve":       usdc_reserve.to_string(),
            "usdc_main_pool_mint":          USDC_MINT_BS58,
            "obligation_count":             positions
                .iter()
                .filter_map(|p| p["obligation_pubkey"].as_str())
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            "usdc_deposit_position_count":  usdc_position_count,
            "positions":                    positions,
            "decode_warnings":              decode_errors,
            "dashboard_visibility_note":    dashboard_visibility_note(),
        })),
        error: None,
        duration_ms: 0,
    }
}

fn no_position_output(wallet_bs58: &str, program_id: &Pubkey) -> ToolOutput {
    ToolOutput {
        tool_name: TOOL_NAME.to_string(),
        success: true,
        data: Some(json!({
            "status":                    STATUS_NO_POSITION,
            "wallet_pubkey":             wallet_bs58,
            "network":                   "mainnet",
            "protocol":                  "Solend/Save",
            "program_id":                program_id.to_string(),
            "positions":                 Vec::<Value>::new(),
            "dashboard_visibility_note": dashboard_visibility_note(),
        })),
        error: None,
        duration_ms: 0,
    }
}

fn wallet_not_bound_output() -> ToolOutput {
    let reason =
        "no external wallet is bound to this session; bind one via the wallet-bind \
         challenge on /chat before scanning Solend positions";
    ToolOutput {
        tool_name: TOOL_NAME.to_string(),
        success: false,
        data: Some(json!({
            "status": STATUS_WALLET_NOT_BOUND,
            "reason": reason,
        })),
        error: Some(reason.to_string()),
        duration_ms: 0,
    }
}

fn rpc_error_output(phase: &str, msg: &str) -> ToolOutput {
    ToolOutput {
        tool_name: TOOL_NAME.to_string(),
        success: false,
        data: Some(json!({
            "status":  STATUS_RPC_ERROR,
            "phase":   phase,
            "reason":  msg,
        })),
        error: Some(format!("rpc_error in {phase}: {msg}")),
        duration_ms: 0,
    }
}

fn decode_error_output(msg: &str) -> ToolOutput {
    ToolOutput {
        tool_name: TOOL_NAME.to_string(),
        success: false,
        data: Some(json!({
            "status": STATUS_DECODE_ERROR,
            "reason": msg,
        })),
        error: Some(format!("decode_error: {msg}")),
        duration_ms: 0,
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::solend::raw::{
        synth_obligation, SolendObligationCollateralRaw,
    };
    use claw_types::session::SessionId;
    use serde_json::Value as JsonValue;
    use std::sync::Mutex;

    /// Stub session wallet — returns a configured pubkey or None.
    struct StubSessionWallet(Option<String>);
    #[async_trait]
    impl SessionBoundWallet for StubSessionWallet {
        fn session_wallet_pubkey(&self, _sid: &SessionId) -> Option<String> {
            self.0.clone()
        }
    }

    /// Stub reader. Captures calls; returns canned responses.
    struct StubReader {
        obligations: Mutex<Vec<(Pubkey, Vec<u8>)>>,
        find_err: Mutex<Option<String>>,
        find_calls: Mutex<Vec<Pubkey>>,
        accounts: Mutex<std::collections::HashMap<Pubkey, Vec<u8>>>,
    }
    impl StubReader {
        fn empty() -> Self {
            Self {
                obligations: Mutex::new(Vec::new()),
                find_err: Mutex::new(None),
                find_calls: Mutex::new(Vec::new()),
                accounts: Mutex::new(Default::default()),
            }
        }
        fn with_obligation(pk: Pubkey, data: Vec<u8>) -> Self {
            let s = Self::empty();
            s.obligations.lock().unwrap().push((pk, data));
            s
        }
        fn with_err(msg: &str) -> Self {
            let s = Self::empty();
            *s.find_err.lock().unwrap() = Some(msg.into());
            s
        }
    }
    #[async_trait]
    impl SolendPositionReader for StubReader {
        async fn find_obligations_for_owner(
            &self,
            owner: &Pubkey,
        ) -> Result<Vec<(Pubkey, Vec<u8>)>, String> {
            self.find_calls.lock().unwrap().push(*owner);
            if let Some(e) = self.find_err.lock().unwrap().clone() {
                return Err(e);
            }
            Ok(self.obligations.lock().unwrap().clone())
        }
        async fn get_account_data(&self, pk: &Pubkey) -> Result<Option<Vec<u8>>, String> {
            Ok(self.accounts.lock().unwrap().get(pk).cloned())
        }
    }

    fn usdc_reserve_pk() -> Pubkey {
        Pubkey::from_str(SOLEND_MAIN_POOL_USDC_RESERVE_BS58).unwrap()
    }

    /// Build a minimal but well-formed obligation byte-array with one
    /// deposit pointing to `deposit_reserve`. Re-uses the existing
    /// `synth_obligation` helper from `integrations::solend::raw`.
    fn build_obligation_bytes(
        owner: Pubkey,
        lending_market: Pubkey,
        deposit_reserve: Pubkey,
        deposited_amount: u64,
    ) -> Vec<u8> {
        let deposits = [SolendObligationCollateralRaw {
            deposit_reserve,
            deposited_amount,
        }];
        synth_obligation(
            owner,
            lending_market,
            /*last_update_slot=*/ 0,
            /*stale=*/ false,
            &deposits,
            &[],
        )
    }

    fn make_tool(
        bound: Option<&str>,
        reader: Arc<dyn SolendPositionReader>,
    ) -> GetSolendPositionTool {
        GetSolendPositionTool::new(
            Arc::new(StubSessionWallet(bound.map(|s| s.to_string()))),
            reader,
        )
    }

    fn input_for(sid: &SessionId) -> ToolInput {
        ToolInput {
            tool_name: TOOL_NAME.to_string(),
            parameters: json!({}),
            session_id: sid.clone(),
            correlation_id: uuid::Uuid::new_v4(),
        }
    }

    fn data_of(out: &ToolOutput) -> JsonValue {
        out.data.clone().unwrap()
    }

    // ── A. wallet not bound → wallet_not_bound ──────────────────────────

    #[tokio::test]
    async fn wallet_not_bound_returns_status() {
        let tool = make_tool(None, Arc::new(StubReader::empty()));
        let out = tool.execute(input_for(&SessionId::new())).await.unwrap();
        let d = data_of(&out);
        assert_eq!(d["status"], STATUS_WALLET_NOT_BOUND);
        assert!(!out.success);
    }

    // ── B. no obligations → no_position ─────────────────────────────────

    #[tokio::test]
    async fn no_obligations_returns_no_position() {
        let owner = Pubkey::new_unique();
        let tool = make_tool(
            Some(&owner.to_string()),
            Arc::new(StubReader::empty()),
        );
        let out = tool.execute(input_for(&SessionId::new())).await.unwrap();
        let d = data_of(&out);
        assert_eq!(d["status"], STATUS_NO_POSITION);
        assert_eq!(d["wallet_pubkey"], owner.to_string());
        assert_eq!(d["positions"], json!([]));
    }

    // ── C. mocked USDC obligation → ok with USDC-reserve marker ─────────

    #[tokio::test]
    async fn mocked_usdc_obligation_returns_ok_with_usdc_reserve() {
        let owner = Pubkey::new_unique();
        let lending_market = Pubkey::new_unique();
        let obligation_pk = Pubkey::new_unique();
        let bytes = build_obligation_bytes(
            owner,
            lending_market,
            usdc_reserve_pk(),
            5_000_000, // 5 USDC cToken-equivalent — synth uses raw u64
        );
        let reader = Arc::new(StubReader::with_obligation(obligation_pk, bytes));
        let tool = make_tool(Some(&owner.to_string()), reader);
        let out = tool.execute(input_for(&SessionId::new())).await.unwrap();
        let d = data_of(&out);
        assert_eq!(d["status"], STATUS_OK);
        assert_eq!(d["program_id"], SOLEND_PROGRAM_ID_BS58);
        assert_eq!(d["usdc_main_pool_reserve"], SOLEND_MAIN_POOL_USDC_RESERVE_BS58);
        assert_eq!(d["lending_market"], lending_market.to_string());
        assert_eq!(d["usdc_deposit_position_count"], 1);
        assert_eq!(d["obligation_count"], 1);
        let positions = d["positions"].as_array().unwrap();
        let deposit = positions
            .iter()
            .find(|p| p["kind"] == "deposit")
            .unwrap();
        assert_eq!(deposit["is_usdc_main_pool_reserve"], true);
        assert_eq!(deposit["reserve_pubkey"], SOLEND_MAIN_POOL_USDC_RESERVE_BS58);
        assert_eq!(deposit["obligation_pubkey"], obligation_pk.to_string());
        assert_eq!(deposit["owner_pubkey"], owner.to_string());
        assert_eq!(deposit["deposited_collateral_amount_raw"], "5000000");
        assert_eq!(deposit["supplied_usdc_estimate_raw"], JsonValue::Null);
        assert!(deposit["estimate_unavailable_reason"]
            .as_str()
            .unwrap_or_default()
            .contains("follow-up slice"));
        assert_eq!(deposit["source"], "obligation_scan");
    }

    // ── D. non-USDC reserve flagged false ───────────────────────────────

    #[tokio::test]
    async fn non_usdc_reserve_is_flagged_false() {
        let owner = Pubkey::new_unique();
        let lending_market = Pubkey::new_unique();
        let other_reserve = Pubkey::new_unique();
        let obligation_pk = Pubkey::new_unique();
        let bytes = build_obligation_bytes(owner, lending_market, other_reserve, 999);
        let reader = Arc::new(StubReader::with_obligation(obligation_pk, bytes));
        let tool = make_tool(Some(&owner.to_string()), reader);
        let out = tool.execute(input_for(&SessionId::new())).await.unwrap();
        let d = data_of(&out);
        assert_eq!(d["status"], STATUS_OK);
        assert_eq!(d["usdc_deposit_position_count"], 0);
        let positions = d["positions"].as_array().unwrap();
        assert_eq!(positions[0]["is_usdc_main_pool_reserve"], false);
        assert_eq!(positions[0]["reserve_pubkey"], other_reserve.to_string());
    }

    // ── E. RPC error from find → rpc_error ─────────────────────────────

    #[tokio::test]
    async fn rpc_error_during_find_returns_rpc_error() {
        let owner = Pubkey::new_unique();
        let reader = Arc::new(StubReader::with_err("429 Too Many Requests"));
        let tool = make_tool(Some(&owner.to_string()), reader);
        let out = tool.execute(input_for(&SessionId::new())).await.unwrap();
        let d = data_of(&out);
        assert_eq!(d["status"], STATUS_RPC_ERROR);
        assert_eq!(d["phase"], "find_obligations_for_owner");
        assert!(d["reason"].as_str().unwrap().contains("429"));
        assert!(!out.success);
    }

    // ── F. decode error (malformed obligation length) → decode_error ───

    #[tokio::test]
    async fn malformed_obligation_returns_decode_error() {
        let owner = Pubkey::new_unique();
        let obligation_pk = Pubkey::new_unique();
        // Wrong length — should fail size check before decode.
        let bytes = vec![0u8; 100];
        let reader = Arc::new(StubReader::with_obligation(obligation_pk, bytes));
        let tool = make_tool(Some(&owner.to_string()), reader);
        let out = tool.execute(input_for(&SessionId::new())).await.unwrap();
        let d = data_of(&out);
        assert_eq!(d["status"], STATUS_DECODE_ERROR);
    }

    // ── G. spec invariant: required_capabilities is empty ──────────────

    #[tokio::test]
    async fn spec_required_capabilities_is_empty() {
        let tool = make_tool(None, Arc::new(StubReader::empty()));
        let spec = tool.spec();
        assert_eq!(spec.required_capabilities, Vec::<String>::new());
        assert_eq!(spec.name, TOOL_NAME);
    }

    // ── H. forbidden-runtime structural scan ───────────────────────────

    #[test]
    fn module_has_no_forbidden_runtime_references() {
        const SOURCE: &str = include_str!("get_solend_position.rs");
        // Build needles via concat so this list itself can never
        // self-match SOURCE.
        let needles = [
            format!("{}{}", ".send_", "transaction("),
            format!("{}{}", ".send_raw_", "transaction("),
            format!("{}{}", ".send_raw_v0_", "transaction("),
            format!("{}{}", ".confirm_", "transaction("),
            format!("{}{}", "Transaction::", "new_signed_with_payer("),
            format!("{}{}", "Versioned", "Transaction"),
            format!("{}{}", "Message", "V0"),
            format!("{}{}", "AddressLookup", "Table"),
            format!("{}{}", "simulate_", "transaction("),
            format!("{}{}", "Keypair::", "new("),
            format!("{}{}", "create_signing_", "handoff("),
            format!("{}{}", "submit_signed_solend_", "transaction("),
            format!("{}{}", "ApprovalRequest", "::new("),
            // Field-name guards: ensure the tool never emits these
            // names in its output. Built via concat so the literal
            // doesn't appear in this list itself.
            format!("{}{}", "priv", "ate_key"),
            format!("{}{}", "tx_", "bytes"),
            format!("{}{}", "sign", "ed_bytes"),
        ];
        for n in &needles {
            assert!(
                !SOURCE.contains(n),
                "get_solend_position.rs must not contain `{n}`"
            );
        }
    }

    // ── I. ok output never includes secret/tx-bytes fields ─────────────

    #[tokio::test]
    async fn ok_output_payload_has_no_secret_material() {
        let owner = Pubkey::new_unique();
        let bytes = build_obligation_bytes(owner, Pubkey::new_unique(), usdc_reserve_pk(), 1);
        let reader = Arc::new(StubReader::with_obligation(Pubkey::new_unique(), bytes));
        let tool = make_tool(Some(&owner.to_string()), reader);
        let out = tool.execute(input_for(&SessionId::new())).await.unwrap();
        let s = serde_json::to_string(&out.data).unwrap();
        // Concat to avoid the test array itself containing the literal.
        let forbidden_in_payload = [
            format!("{}", "Keypair"),
            format!("{}{}", "priv", "ate_key"),
            format!("{}", "secret_"),
            format!("{}{}", "tx_", "bytes"),
            format!("{}{}", "sign", "ed_bytes"),
            format!("{}", "approval_request_id"),
            format!("{}", "signing_request_id"),
        ];
        for forbidden in &forbidden_in_payload {
            assert!(
                !s.contains(forbidden),
                "ok output payload must not contain `{forbidden}`"
            );
        }
    }
}
