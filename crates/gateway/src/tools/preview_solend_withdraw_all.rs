//! Phase 6I-A — Solend / Save withdraw-all PREVIEW (read-only).
//!
//! Bridges the gap between Phase 6H's read-only obligation scanner
//! ([`crate::tools::get_solend_position`]) and a future Phase 6I-B
//! execution tool. Given an explicit `obligation_pubkey` produced by
//! the scanner, this tool re-fetches that single obligation, decodes
//! it, validates the user-recovery preconditions, and returns a
//! structured PREVIEW. It NEVER signs, NEVER broadcasts, NEVER creates
//! an approval. The withdraw-execution substrate
//! ([`crate::integrations::solend::withdraw`] /
//! [`crate::integrations::solend_withdraw_tx_plan`]) remains
//! `#[cfg(test)]`-gated and unwired.
//!
//! # Why preview-only?
//!
//! The Phase 6H scanner finds obligations but does not validate that a
//! given obligation is SAFE for withdraw-all (no debt, owner ==
//! session wallet, has a USDC-reserve deposit, etc.). The frontend
//! confirmation card needs those facts before asking the user to sign.
//! Phase 6I-A surfaces them explicitly without taking any irreversible
//! action.
//!
//! # Validations performed (in order)
//!
//! 1. Session-bound wallet exists.
//! 2. `obligation_pubkey` parses as a valid pubkey.
//! 3. `getAccountInfo(obligation_pubkey)` returns Some.
//! 4. Account data length matches Solend's
//!    [`crate::integrations::solend::raw::OBLIGATION_LEN`] (1300).
//! 5. Decoded `owner` equals the session wallet.
//! 6. NO borrow entry has a non-zero `borrowed_amount_wads`. A non-zero
//!    borrow blocks the preview as `unsafe_to_withdraw_all` because
//!    Phase 6I-A does not implement the health-factor / risk
//!    calculation Solend requires for partial withdrawal against debt.
//! 7. At least one deposit entry uses the configured USDC reserve and
//!    has a non-zero `deposited_amount`. The first such entry's amount
//!    is reported as `collateral_amount_raw`.
//!
//! # Wire-stable status set
//!
//! The strings below are part of the tool's public output contract.
//! Callers (frontend renderer, p5d2 chat-route observability tests)
//! match on them. Reordering / renaming is a wire change.
//!
//! - `ok`
//! - `wallet_not_bound`
//! - `invalid_obligation_pubkey`
//! - `obligation_not_found`
//! - `owner_mismatch`
//! - `no_usdc_deposit`
//! - `unsafe_to_withdraw_all`
//! - `rpc_error`
//! - `decode_error`
//!
//! # Phase 6I-A scope guarantees
//!
//! - **Scope:** withdraw-all sentinel only. Partial "withdraw X USDC"
//!   is intentionally not supported here — it requires the cToken /
//!   underlying exchange-rate decode that Phase 6I-A defers.
//! - **No mutation:** no approval store touch, no park store touch, no
//!   signing handoff, no transaction submit. The
//!   `module_has_no_forbidden_runtime_references` test below scans
//!   this file's source for any such reference.
//! - **No daemon signature:** the only signer required for the future
//!   execution path is the user wallet (the obligation owner). The
//!   preview output asserts `requires_user_signature: true` and
//!   `required_signers: ["wallet"]`. The future signing slice cannot
//!   change this without updating both this preview and the chat
//!   contract.
//! - **No transient obligation keypair:** withdraw spends the existing
//!   obligation, so `requires_obligation_keypair: false`.
//! - **No dead-source flip:** this slice does not touch the
//!   `#[cfg(test)]` gates on `integrations::solend::withdraw` or
//!   `integrations::solend_withdraw_tx_plan`. The withdraw-execution
//!   substrate stays unwired; this preview only reads obligation
//!   bytes and reports facts.
//!
//! # Chat wiring posture
//!
//! Phase 6I-A registers the tool struct but does NOT add it to
//! [`crate::runtime::chat_wiring::CHAT_TOOL_ALLOWLIST`]. Wiring to
//! chat is deferred to Phase 6I-B / 6I-A2 with full
//! `p5d2_chat_route` coverage proving natural-language preview
//! dispatch cannot create approval / sign / broadcast.

use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use solana_sdk::pubkey::Pubkey;

use claw_tool_system::{errors::ToolError, tool::Tool};
use claw_types::tool::{ToolInput, ToolOutput, ToolSpec};

use crate::integrations::solend::raw::{
    decode_obligation, OBLIGATION_LEN, SOLEND_PROGRAM_ID_BS58,
};
use crate::tools::get_solend_position::{
    SolendPositionReader, SOLEND_MAIN_POOL_USDC_RESERVE_BS58, USDC_MINT_BS58,
};
use crate::tools::jupiter_swap::SessionBoundWallet;

/// Tool name. Single source of truth — used by registry / future
/// allowlist gating / future p5d2 dispatch tests.
pub const TOOL_NAME: &str = "preview_solend_withdraw_all";

// Wire-stable status strings. Reordering / renaming is a wire change.
const STATUS_OK: &str = "ok";
const STATUS_WALLET_NOT_BOUND: &str = "wallet_not_bound";
const STATUS_INVALID_OBLIGATION_PUBKEY: &str = "invalid_obligation_pubkey";
const STATUS_OBLIGATION_NOT_FOUND: &str = "obligation_not_found";
const STATUS_OWNER_MISMATCH: &str = "owner_mismatch";
const STATUS_NO_USDC_DEPOSIT: &str = "no_usdc_deposit";
const STATUS_UNSAFE_TO_WITHDRAW_ALL: &str = "unsafe_to_withdraw_all";
const STATUS_RPC_ERROR: &str = "rpc_error";
const STATUS_DECODE_ERROR: &str = "decode_error";

const MODE_WITHDRAW_ALL_COLLATERAL: &str = "withdraw_all_collateral";

const ESTIMATE_UNAVAILABLE_REASON: &str =
    "underlying USDC estimate requires the reserve's cToken-supply / total-liquidity \
     exchange rate, which a follow-up slice will decode. Phase 6I-A reports the \
     deposited cToken amount exactly and never invents a number.";

const NEXT_STEP_MESSAGE: &str =
    "After a frontend card and explicit user confirmation, Phase 6I-B may create an \
     approval for a withdraw-all transaction. Phase 6I-A is preview-only: no approval, \
     no signing, no broadcast.";

const UNSAFE_TO_WITHDRAW_ALL_REASON: &str =
    "This obligation has borrow entries; withdraw-all requires a health-factor / \
     risk calculation not implemented in Phase 6I-A.";

// ── Tool ────────────────────────────────────────────────────────────────────

/// Read-only chat tool that previews a Solend withdraw-all against a
/// caller-supplied `obligation_pubkey`.
pub struct PreviewSolendWithdrawAllTool {
    session_wallet_lookup: Arc<dyn SessionBoundWallet>,
    /// Reused from [`crate::tools::get_solend_position`] — same
    /// `getAccountInfo` seam, same RPC pool, same circuit-breaker
    /// behavior. The preview only calls `get_account_data`; the
    /// `find_obligations_for_owner` method is unused here (the
    /// obligation pubkey arrives explicitly from the caller).
    reader: Arc<dyn SolendPositionReader>,
    usdc_reserve_pk: Pubkey,
    program_id_pk: Pubkey,
}

impl PreviewSolendWithdrawAllTool {
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
impl Tool for PreviewSolendWithdrawAllTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: TOOL_NAME.to_string(),
            description: "Preview-only check that a Solend / Save obligation owned by \
                          the session wallet is safe for withdraw-all. Returns a \
                          structured summary; never signs, never broadcasts, never \
                          creates an approval. Phase 6I-A scope."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["obligation_pubkey"],
                "properties": {
                    "obligation_pubkey": { "type": "string" }
                }
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "status": { "type": "string" }
                },
                "required": ["status"]
            }),
            required_capabilities: vec![],
            supports_streaming: false,
            timeout_ms: 8000,
        }
    }

    async fn execute(&self, input: ToolInput) -> Result<ToolOutput, ToolError> {
        // 1. Session-bound wallet.
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

        // 2. Extract + validate obligation_pubkey.
        let obligation_str = match input
            .parameters
            .get("obligation_pubkey")
            .and_then(|v| v.as_str())
        {
            Some(s) => s,
            None => {
                return Ok(invalid_obligation_pubkey_output(
                    "missing required field `obligation_pubkey`",
                ))
            }
        };
        let obligation_pk = match Pubkey::from_str(obligation_str) {
            Ok(pk) => pk,
            Err(e) => {
                return Ok(invalid_obligation_pubkey_output(&format!(
                    "obligation_pubkey is not a valid pubkey: {e}"
                )))
            }
        };

        // 3. Fetch obligation account.
        let data = match self.reader.get_account_data(&obligation_pk).await {
            Ok(Some(d)) => d,
            Ok(None) => return Ok(obligation_not_found_output(&obligation_pk)),
            Err(e) => return Ok(rpc_error_output("get_account_data", &e)),
        };

        // 4. Length + decode.
        if data.len() != OBLIGATION_LEN {
            return Ok(decode_error_output(&format!(
                "obligation {} has wrong length (got {}, expected {})",
                obligation_pk,
                data.len(),
                OBLIGATION_LEN
            )));
        }
        let decoded = match decode_obligation(&data) {
            Ok(o) => o,
            Err(e) => {
                return Ok(decode_error_output(&format!(
                    "obligation {obligation_pk} failed to decode: {e}"
                )))
            }
        };

        // 5. Owner-match against the session-bound wallet.
        if decoded.owner != wallet_pk {
            return Ok(owner_mismatch_output(
                &wallet_bs58,
                &obligation_pk,
                &decoded.owner.to_string(),
            ));
        }

        // 6. Borrow check. Block before reporting deposit details — an
        //    obligation with debt is structurally not eligible for the
        //    Phase 6I-A withdraw-all path, regardless of its deposit
        //    composition.
        let has_any_borrow = decoded
            .borrows
            .iter()
            .any(|b| b.borrowed_amount_wads > 0);
        if has_any_borrow {
            return Ok(unsafe_to_withdraw_all_output(
                &wallet_bs58,
                &obligation_pk,
                decoded.borrows.len(),
            ));
        }

        // 7. Locate the USDC-reserve deposit. An obligation may carry
        //    multiple deposit entries across different reserves; we
        //    report only the USDC one. If absent, block as
        //    `no_usdc_deposit`.
        let usdc_deposit = decoded.deposits.iter().find(|d| {
            d.deposit_reserve == self.usdc_reserve_pk && d.deposited_amount > 0
        });
        let collateral_amount = match usdc_deposit {
            Some(d) => d.deposited_amount,
            None => {
                return Ok(no_usdc_deposit_output(
                    &wallet_bs58,
                    &obligation_pk,
                    &self.usdc_reserve_pk,
                ))
            }
        };

        // 8. Successful preview.
        Ok(ok_preview_output(
            &wallet_bs58,
            &obligation_pk,
            &decoded.lending_market,
            &self.usdc_reserve_pk,
            &self.program_id_pk,
            collateral_amount,
        ))
    }
}

// ── Output helpers ──────────────────────────────────────────────────────────

fn wallet_not_bound_output() -> ToolOutput {
    let reason = "no external wallet is bound to this session; bind one via the \
                  wallet-bind challenge on /chat before previewing Solend withdraws";
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

fn invalid_obligation_pubkey_output(reason: &str) -> ToolOutput {
    ToolOutput {
        tool_name: TOOL_NAME.to_string(),
        success: false,
        data: Some(json!({
            "status": STATUS_INVALID_OBLIGATION_PUBKEY,
            "reason": reason,
        })),
        error: Some(reason.to_string()),
        duration_ms: 0,
    }
}

fn obligation_not_found_output(obligation_pk: &Pubkey) -> ToolOutput {
    ToolOutput {
        tool_name: TOOL_NAME.to_string(),
        success: false,
        data: Some(json!({
            "status":            STATUS_OBLIGATION_NOT_FOUND,
            "obligation_pubkey": obligation_pk.to_string(),
            "reason":            "obligation account does not exist on chain",
        })),
        error: Some(format!("obligation {obligation_pk} not found")),
        duration_ms: 0,
    }
}

fn owner_mismatch_output(
    session_wallet_bs58: &str,
    obligation_pk: &Pubkey,
    decoded_owner_bs58: &str,
) -> ToolOutput {
    let reason = "obligation owner does not match the session-bound wallet; preview \
                  blocked to prevent acting on someone else's position";
    ToolOutput {
        tool_name: TOOL_NAME.to_string(),
        success: false,
        data: Some(json!({
            "status":              STATUS_OWNER_MISMATCH,
            "wallet_pubkey":       session_wallet_bs58,
            "obligation_pubkey":   obligation_pk.to_string(),
            "decoded_owner":       decoded_owner_bs58,
            "reason":              reason,
        })),
        error: Some(reason.to_string()),
        duration_ms: 0,
    }
}

fn no_usdc_deposit_output(
    wallet_bs58: &str,
    obligation_pk: &Pubkey,
    usdc_reserve_pk: &Pubkey,
) -> ToolOutput {
    let reason = "obligation has no non-zero deposit entry for the Solend / Save Main \
                  Pool USDC reserve; nothing to withdraw via this preview";
    ToolOutput {
        tool_name: TOOL_NAME.to_string(),
        success: false,
        data: Some(json!({
            "status":            STATUS_NO_USDC_DEPOSIT,
            "wallet_pubkey":     wallet_bs58,
            "obligation_pubkey": obligation_pk.to_string(),
            "reserve_pubkey":    usdc_reserve_pk.to_string(),
            "reason":            reason,
        })),
        error: Some(reason.to_string()),
        duration_ms: 0,
    }
}

fn unsafe_to_withdraw_all_output(
    wallet_bs58: &str,
    obligation_pk: &Pubkey,
    borrow_entry_count: usize,
) -> ToolOutput {
    ToolOutput {
        tool_name: TOOL_NAME.to_string(),
        success: false,
        data: Some(json!({
            "status":              STATUS_UNSAFE_TO_WITHDRAW_ALL,
            "wallet_pubkey":       wallet_bs58,
            "obligation_pubkey":   obligation_pk.to_string(),
            "borrow_entry_count":  borrow_entry_count,
            "reason":              UNSAFE_TO_WITHDRAW_ALL_REASON,
        })),
        error: Some(UNSAFE_TO_WITHDRAW_ALL_REASON.to_string()),
        duration_ms: 0,
    }
}

fn rpc_error_output(phase: &str, msg: &str) -> ToolOutput {
    ToolOutput {
        tool_name: TOOL_NAME.to_string(),
        success: false,
        data: Some(json!({
            "status": STATUS_RPC_ERROR,
            "phase":  phase,
            "reason": msg,
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

fn ok_preview_output(
    wallet_bs58: &str,
    obligation_pk: &Pubkey,
    lending_market: &Pubkey,
    usdc_reserve: &Pubkey,
    program_id: &Pubkey,
    collateral_amount_raw: u64,
) -> ToolOutput {
    ToolOutput {
        tool_name: TOOL_NAME.to_string(),
        success: true,
        data: Some(json!({
            "status":                       STATUS_OK,
            "mode":                         MODE_WITHDRAW_ALL_COLLATERAL,
            "protocol":                     "Solend/Save",
            "network":                      "mainnet",
            "program_id":                   program_id.to_string(),
            "wallet_pubkey":                wallet_bs58,
            "obligation_pubkey":            obligation_pk.to_string(),
            "lending_market":               lending_market.to_string(),
            "reserve_pubkey":               usdc_reserve.to_string(),
            "reserve_mint":                 USDC_MINT_BS58,
            "collateral_amount_raw":        collateral_amount_raw.to_string(),
            "underlying_usdc_estimate_raw": Value::Null,
            "underlying_usdc_estimate_ui":  Value::Null,
            "estimate_unavailable_reason":  ESTIMATE_UNAVAILABLE_REASON,
            "requires_user_signature":      true,
            "required_signers":             ["wallet"],
            "requires_obligation_keypair":  false,
            "will_create_approval":         false,
            "will_sign":                    false,
            "will_broadcast":               false,
            "next_step":                    NEXT_STEP_MESSAGE,
        })),
        error: None,
        duration_ms: 0,
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::solend::raw::{
        synth_obligation, SolendObligationCollateralRaw, SolendObligationLiquidityRaw,
    };
    use claw_types::session::SessionId;
    use serde_json::Value as JsonValue;
    use std::collections::HashMap;
    use std::sync::Mutex;

    // ── Stubs ───────────────────────────────────────────────────────────

    struct StubSessionWallet(Option<String>);
    #[async_trait]
    impl SessionBoundWallet for StubSessionWallet {
        fn session_wallet_pubkey(&self, _sid: &SessionId) -> Option<String> {
            self.0.clone()
        }
    }

    /// Stub reader. The preview only calls `get_account_data`; the
    /// `find_obligations_for_owner` arm is implemented for trait
    /// completeness but never expected to fire here.
    struct StubReader {
        accounts: Mutex<HashMap<Pubkey, Vec<u8>>>,
        get_err: Mutex<Option<String>>,
        find_calls: Mutex<u32>,
    }
    impl StubReader {
        fn empty() -> Self {
            Self {
                accounts: Mutex::new(HashMap::new()),
                get_err: Mutex::new(None),
                find_calls: Mutex::new(0),
            }
        }
        fn with_account(pk: Pubkey, data: Vec<u8>) -> Self {
            let s = Self::empty();
            s.accounts.lock().unwrap().insert(pk, data);
            s
        }
        fn with_rpc_err(msg: &str) -> Self {
            let s = Self::empty();
            *s.get_err.lock().unwrap() = Some(msg.into());
            s
        }
    }
    #[async_trait]
    impl SolendPositionReader for StubReader {
        async fn find_obligations_for_owner(
            &self,
            _owner: &Pubkey,
        ) -> Result<Vec<(Pubkey, Vec<u8>)>, String> {
            *self.find_calls.lock().unwrap() += 1;
            Ok(Vec::new())
        }
        async fn get_account_data(&self, pk: &Pubkey) -> Result<Option<Vec<u8>>, String> {
            if let Some(e) = self.get_err.lock().unwrap().clone() {
                return Err(e);
            }
            Ok(self.accounts.lock().unwrap().get(pk).cloned())
        }
    }

    // ── Fixture helpers ────────────────────────────────────────────────

    fn usdc_reserve_pk() -> Pubkey {
        Pubkey::from_str(SOLEND_MAIN_POOL_USDC_RESERVE_BS58).unwrap()
    }

    fn build_usdc_obligation(
        owner: Pubkey,
        deposited_amount: u64,
        borrows: &[SolendObligationLiquidityRaw],
    ) -> Vec<u8> {
        let lending_market = Pubkey::new_unique();
        let deposits = [SolendObligationCollateralRaw {
            deposit_reserve: usdc_reserve_pk(),
            deposited_amount,
        }];
        synth_obligation(owner, lending_market, 0, false, &deposits, borrows)
    }

    fn build_non_usdc_obligation(owner: Pubkey, deposited_amount: u64) -> Vec<u8> {
        let lending_market = Pubkey::new_unique();
        let other_reserve = Pubkey::new_unique();
        let deposits = [SolendObligationCollateralRaw {
            deposit_reserve: other_reserve,
            deposited_amount,
        }];
        synth_obligation(owner, lending_market, 0, false, &deposits, &[])
    }

    fn make_tool(
        bound: Option<&str>,
        reader: Arc<dyn SolendPositionReader>,
    ) -> PreviewSolendWithdrawAllTool {
        PreviewSolendWithdrawAllTool::new(
            Arc::new(StubSessionWallet(bound.map(|s| s.to_string()))),
            reader,
        )
    }

    fn input_for(sid: &SessionId, obligation_pubkey: &str) -> ToolInput {
        ToolInput {
            tool_name: TOOL_NAME.to_string(),
            parameters: json!({ "obligation_pubkey": obligation_pubkey }),
            session_id: sid.clone(),
            correlation_id: uuid::Uuid::new_v4(),
        }
    }

    fn input_with_raw_params(sid: &SessionId, params: JsonValue) -> ToolInput {
        ToolInput {
            tool_name: TOOL_NAME.to_string(),
            parameters: params,
            session_id: sid.clone(),
            correlation_id: uuid::Uuid::new_v4(),
        }
    }

    fn data_of(out: &ToolOutput) -> JsonValue {
        out.data.clone().unwrap()
    }

    // ── Required test 1 — wallet not bound ──────────────────────────────

    #[tokio::test]
    async fn wallet_not_bound_returns_status() {
        let tool = make_tool(None, Arc::new(StubReader::empty()));
        let out = tool
            .execute(input_for(&SessionId::new(), &Pubkey::new_unique().to_string()))
            .await
            .unwrap();
        let d = data_of(&out);
        assert_eq!(d["status"], STATUS_WALLET_NOT_BOUND);
        assert!(!out.success);
    }

    // ── Required test 2 — invalid obligation_pubkey ─────────────────────

    #[tokio::test]
    async fn invalid_obligation_pubkey_garbage_string() {
        let owner = Pubkey::new_unique();
        let tool = make_tool(Some(&owner.to_string()), Arc::new(StubReader::empty()));
        let out = tool
            .execute(input_for(&SessionId::new(), "not-a-pubkey-at-all"))
            .await
            .unwrap();
        let d = data_of(&out);
        assert_eq!(d["status"], STATUS_INVALID_OBLIGATION_PUBKEY);
        assert!(!out.success);
    }

    #[tokio::test]
    async fn invalid_obligation_pubkey_missing_field() {
        let owner = Pubkey::new_unique();
        let tool = make_tool(Some(&owner.to_string()), Arc::new(StubReader::empty()));
        let out = tool
            .execute(input_with_raw_params(&SessionId::new(), json!({})))
            .await
            .unwrap();
        let d = data_of(&out);
        assert_eq!(d["status"], STATUS_INVALID_OBLIGATION_PUBKEY);
    }

    // ── Required test 3 — owner mismatch blocks ─────────────────────────

    #[tokio::test]
    async fn owner_mismatch_blocks_preview() {
        let session_wallet = Pubkey::new_unique();
        let obligation_owner = Pubkey::new_unique(); // distinct from session wallet
        let obligation_pk = Pubkey::new_unique();
        let bytes = build_usdc_obligation(obligation_owner, 3_857_506, &[]);
        let reader = Arc::new(StubReader::with_account(obligation_pk, bytes));
        let tool = make_tool(Some(&session_wallet.to_string()), reader);
        let out = tool
            .execute(input_for(&SessionId::new(), &obligation_pk.to_string()))
            .await
            .unwrap();
        let d = data_of(&out);
        assert_eq!(d["status"], STATUS_OWNER_MISMATCH);
        assert_eq!(d["wallet_pubkey"], session_wallet.to_string());
        assert_eq!(d["decoded_owner"], obligation_owner.to_string());
        assert!(!out.success);
    }

    // ── Required test 4 — no_usdc_deposit blocks ────────────────────────

    #[tokio::test]
    async fn no_usdc_deposit_blocks_preview() {
        let owner = Pubkey::new_unique();
        let obligation_pk = Pubkey::new_unique();
        let bytes = build_non_usdc_obligation(owner, 5_000);
        let reader = Arc::new(StubReader::with_account(obligation_pk, bytes));
        let tool = make_tool(Some(&owner.to_string()), reader);
        let out = tool
            .execute(input_for(&SessionId::new(), &obligation_pk.to_string()))
            .await
            .unwrap();
        let d = data_of(&out);
        assert_eq!(d["status"], STATUS_NO_USDC_DEPOSIT);
        assert_eq!(d["reserve_pubkey"], SOLEND_MAIN_POOL_USDC_RESERVE_BS58);
    }

    // ── Required test 5 — borrow present blocks unsafe_to_withdraw_all ──

    #[tokio::test]
    async fn borrow_present_blocks_unsafe_to_withdraw_all() {
        let owner = Pubkey::new_unique();
        let obligation_pk = Pubkey::new_unique();
        let borrow_reserve = Pubkey::new_unique();
        let borrows = [SolendObligationLiquidityRaw {
            borrow_reserve,
            borrowed_amount_wads: 1_000_000_000_000_000_000u128, // 1 token in wads
        }];
        let bytes = build_usdc_obligation(owner, 3_857_506, &borrows);
        let reader = Arc::new(StubReader::with_account(obligation_pk, bytes));
        let tool = make_tool(Some(&owner.to_string()), reader);
        let out = tool
            .execute(input_for(&SessionId::new(), &obligation_pk.to_string()))
            .await
            .unwrap();
        let d = data_of(&out);
        assert_eq!(d["status"], STATUS_UNSAFE_TO_WITHDRAW_ALL);
        assert_eq!(d["borrow_entry_count"], 1);
        assert!(d["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("health-factor"));
    }

    // ── Required test 6 — Phase 6H largest obligation passes preview ────

    /// Concrete-pubkey rehearsal using the actual Phase 6H output for
    /// the live wallet `C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW`,
    /// largest obligation
    /// `HcKrv5Jo5f6qvzSGhJVYTNSqwKudRizn6fxbjPW7M8SV`,
    /// `deposited_collateral_amount_raw = 3_857_506`.
    ///
    /// This satisfies the brief's required test (6) — and asserts the
    /// invariant flags in one place so a future tweak that flips any of
    /// them fails LOUDLY:
    ///   - mode == withdraw_all_collateral
    ///   - collateral_amount_raw == "3857506"
    ///   - requires_user_signature == true
    ///   - requires_obligation_keypair == false
    ///   - will_create_approval == false
    ///   - will_sign == false
    ///   - will_broadcast == false
    #[tokio::test]
    async fn phase6h_largest_obligation_passes_preview_with_correct_flags() {
        let wallet =
            Pubkey::from_str("C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW").unwrap();
        let obligation_pk =
            Pubkey::from_str("HcKrv5Jo5f6qvzSGhJVYTNSqwKudRizn6fxbjPW7M8SV").unwrap();
        let bytes = build_usdc_obligation(wallet, 3_857_506, &[]);
        let reader = Arc::new(StubReader::with_account(obligation_pk, bytes));
        let tool = make_tool(Some(&wallet.to_string()), reader);
        let out = tool
            .execute(input_for(&SessionId::new(), &obligation_pk.to_string()))
            .await
            .unwrap();
        assert!(out.success);
        let d = data_of(&out);
        assert_eq!(d["status"], STATUS_OK);
        assert_eq!(d["mode"], MODE_WITHDRAW_ALL_COLLATERAL);
        assert_eq!(d["protocol"], "Solend/Save");
        assert_eq!(d["network"], "mainnet");
        assert_eq!(d["program_id"], SOLEND_PROGRAM_ID_BS58);
        assert_eq!(d["wallet_pubkey"], wallet.to_string());
        assert_eq!(d["obligation_pubkey"], obligation_pk.to_string());
        assert_eq!(d["reserve_pubkey"], SOLEND_MAIN_POOL_USDC_RESERVE_BS58);
        assert_eq!(d["reserve_mint"], USDC_MINT_BS58);
        assert_eq!(d["collateral_amount_raw"], "3857506");
        assert_eq!(d["underlying_usdc_estimate_raw"], JsonValue::Null);
        assert_eq!(d["underlying_usdc_estimate_ui"], JsonValue::Null);
        assert!(d["estimate_unavailable_reason"]
            .as_str()
            .unwrap_or_default()
            .contains("exchange rate"));

        // Invariants asserted explicitly so a regression flips loudly.
        assert_eq!(d["requires_user_signature"], JsonValue::Bool(true));
        assert_eq!(d["required_signers"], json!(["wallet"]));
        assert_eq!(d["requires_obligation_keypair"], JsonValue::Bool(false));
        assert_eq!(d["will_create_approval"], JsonValue::Bool(false));
        assert_eq!(d["will_sign"], JsonValue::Bool(false));
        assert_eq!(d["will_broadcast"], JsonValue::Bool(false));
    }

    // ── Required test 7 — explicit obligation_pubkey threads through ────

    /// Required test (7): preview must thread the caller-supplied
    /// `obligation_pubkey` verbatim into the output. Composes with
    /// the existing
    /// `solend_withdraw_tx_plan::tests::phase6i_explicit_phase6h_obligation_pubkey_threads_through_plan_and_ix`
    /// (added in the prior turn) which proves that any plan
    /// assembled with this same pubkey carries it into the withdraw
    /// ix at slot 3 with no derivation and no transient keypair.
    /// Together the two tests cover the preview → assembler proof
    /// chain without making the preview tool depend on the
    /// `#[cfg(test)]`-gated assembler at runtime.
    #[tokio::test]
    async fn explicit_obligation_pubkey_threads_into_preview_output() {
        let wallet = Pubkey::new_unique();
        let obligation_pk =
            Pubkey::from_str("HcKrv5Jo5f6qvzSGhJVYTNSqwKudRizn6fxbjPW7M8SV").unwrap();
        let bytes = build_usdc_obligation(wallet, 3_857_506, &[]);
        let reader = Arc::new(StubReader::with_account(obligation_pk, bytes));
        let tool = make_tool(Some(&wallet.to_string()), reader);
        let out = tool
            .execute(input_for(&SessionId::new(), &obligation_pk.to_string()))
            .await
            .unwrap();
        let d = data_of(&out);
        assert_eq!(d["status"], STATUS_OK);
        assert_eq!(
            d["obligation_pubkey"], obligation_pk.to_string(),
            "preview must echo the caller-supplied obligation pubkey verbatim"
        );
    }

    // ── Required test 8 — signer set is only user wallet ────────────────

    #[tokio::test]
    async fn signer_set_is_only_user_wallet_no_daemon_signer() {
        let wallet = Pubkey::new_unique();
        let obligation_pk = Pubkey::new_unique();
        let bytes = build_usdc_obligation(wallet, 3_857_506, &[]);
        let reader = Arc::new(StubReader::with_account(obligation_pk, bytes));
        let tool = make_tool(Some(&wallet.to_string()), reader);
        let out = tool
            .execute(input_for(&SessionId::new(), &obligation_pk.to_string()))
            .await
            .unwrap();
        let d = data_of(&out);
        assert_eq!(d["status"], STATUS_OK);
        assert_eq!(d["required_signers"], json!(["wallet"]));
        assert_eq!(d["requires_user_signature"], JsonValue::Bool(true));
        assert_eq!(d["requires_obligation_keypair"], JsonValue::Bool(false));

        // The output JSON must not contain any daemon / fee-payer /
        // service-key signer identifier. Built via concat so this list
        // does not self-match.
        let s = serde_json::to_string(&out.data).unwrap();
        let forbidden = [
            format!("{}{}", "daemon_", "signer"),
            format!("{}{}", "fee_payer", "_signer"),
            format!("{}{}", "service_", "keypair"),
            format!("{}{}", "claw_", "signer_keypair"),
        ];
        for n in &forbidden {
            assert!(
                !s.contains(n),
                "preview output must not surface signer identifier `{n}`"
            );
        }
    }

    // ── Required test 9 — source guard ──────────────────────────────────

    #[test]
    fn module_has_no_forbidden_runtime_references() {
        const SOURCE: &str = include_str!("preview_solend_withdraw_all.rs");
        // Build needles via concat so this list itself can never
        // self-match the source.
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
            format!("{}{}", "approval_store", "."),
            format!("{}{}", "park_store", "."),
            // Field-name guards: the preview must never emit secret
            // material in its output.
            format!("{}{}", "priv", "ate_key"),
            format!("{}{}", "tx_", "bytes"),
            format!("{}{}", "sign", "ed_bytes"),
        ];
        for n in &needles {
            assert!(
                !SOURCE.contains(n),
                "preview_solend_withdraw_all.rs must not contain `{n}`"
            );
        }
    }

    // ── Required test 10 — withdraw remains dead-source / unwired ───────

    /// Phase 6I-A invariant: this preview tool is NOT the withdraw
    /// execution tool. The execution tool's name (`solend_withdraw_usdc`)
    /// is reserved for a future slice and remains absent from both
    /// the chat allowlist AND any tool-registry constant in this
    /// slice. The preview tool itself is also absent from the chat
    /// allowlist in Phase 6I-A — wiring is deferred per the slice's
    /// preferred path.
    #[test]
    fn withdraw_remains_dead_source_with_preview_wiring_deferred() {
        // 10a. This tool is the preview, not the execution.
        assert_eq!(TOOL_NAME, "preview_solend_withdraw_all");
        assert_ne!(TOOL_NAME, "solend_withdraw_usdc");

        // 10b. solend_withdraw_usdc is NOT in the chat allowlist.
        let allowlist = crate::runtime::chat_wiring::CHAT_TOOL_ALLOWLIST;
        assert!(
            !allowlist.contains(&"solend_withdraw_usdc"),
            "solend_withdraw_usdc must remain absent from CHAT_TOOL_ALLOWLIST"
        );

        // 10c. preview_solend_withdraw_all is also NOT yet in the chat
        //      allowlist (deferred to 6I-B per Phase 6I-A scope).
        assert!(
            !allowlist.contains(&TOOL_NAME),
            "preview tool wiring deferred — should not be in CHAT_TOOL_ALLOWLIST yet"
        );
    }

    // ── Bonus structural tests ──────────────────────────────────────────

    #[tokio::test]
    async fn obligation_not_found_returns_status() {
        let owner = Pubkey::new_unique();
        let missing_pk = Pubkey::new_unique();
        let reader = Arc::new(StubReader::empty()); // no account stored
        let tool = make_tool(Some(&owner.to_string()), reader);
        let out = tool
            .execute(input_for(&SessionId::new(), &missing_pk.to_string()))
            .await
            .unwrap();
        let d = data_of(&out);
        assert_eq!(d["status"], STATUS_OBLIGATION_NOT_FOUND);
        assert_eq!(d["obligation_pubkey"], missing_pk.to_string());
    }

    #[tokio::test]
    async fn rpc_error_during_get_account_returns_rpc_error() {
        let owner = Pubkey::new_unique();
        let pk = Pubkey::new_unique();
        let reader = Arc::new(StubReader::with_rpc_err("503 Service Unavailable"));
        let tool = make_tool(Some(&owner.to_string()), reader);
        let out = tool
            .execute(input_for(&SessionId::new(), &pk.to_string()))
            .await
            .unwrap();
        let d = data_of(&out);
        assert_eq!(d["status"], STATUS_RPC_ERROR);
        assert_eq!(d["phase"], "get_account_data");
        assert!(d["reason"].as_str().unwrap_or_default().contains("503"));
    }

    #[tokio::test]
    async fn malformed_obligation_returns_decode_error() {
        let owner = Pubkey::new_unique();
        let pk = Pubkey::new_unique();
        let bytes = vec![0u8; 100]; // wrong length
        let reader = Arc::new(StubReader::with_account(pk, bytes));
        let tool = make_tool(Some(&owner.to_string()), reader);
        let out = tool
            .execute(input_for(&SessionId::new(), &pk.to_string()))
            .await
            .unwrap();
        let d = data_of(&out);
        assert_eq!(d["status"], STATUS_DECODE_ERROR);
    }

    #[tokio::test]
    async fn spec_required_capabilities_is_empty_and_schema_demands_obligation_pubkey() {
        let tool = make_tool(None, Arc::new(StubReader::empty()));
        let spec = tool.spec();
        assert_eq!(spec.required_capabilities, Vec::<String>::new());
        assert_eq!(spec.name, TOOL_NAME);
        // Input schema requires obligation_pubkey.
        let req = &spec.input_schema["required"];
        assert_eq!(req, &json!(["obligation_pubkey"]));
        assert_eq!(spec.input_schema["additionalProperties"], json!(false));
    }

    #[tokio::test]
    async fn ok_output_payload_has_no_secret_material() {
        let wallet = Pubkey::new_unique();
        let pk = Pubkey::new_unique();
        let bytes = build_usdc_obligation(wallet, 1, &[]);
        let reader = Arc::new(StubReader::with_account(pk, bytes));
        let tool = make_tool(Some(&wallet.to_string()), reader);
        let out = tool
            .execute(input_for(&SessionId::new(), &pk.to_string()))
            .await
            .unwrap();
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
                "preview ok output must not contain `{forbidden}`"
            );
        }
    }
}
