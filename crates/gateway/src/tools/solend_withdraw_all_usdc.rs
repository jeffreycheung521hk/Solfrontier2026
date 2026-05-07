//! Phase 6I-D — Solend / Save withdraw-all USDC EXECUTION proposal tool.
//!
//! First execution-side surface for Solend withdraw. Strictly bounded:
//! one obligation, one reserve (Solend / Save Main Pool USDC), one
//! mode (withdraw-all via the `u64::MAX` collateral sentinel). No
//! partial amounts, no multi-obligation batch, no borrow / repay.
//!
//! # Flow
//!
//! 1. Strict input parsing (`deny_unknown_fields`) — only
//!    `obligation_pubkey` is accepted. Any extra field (`amount`,
//!    `mode`, `wallet_pubkey`, `slippage`, signed-transaction blobs,
//!    ...) fails the call before any side effect.
//! 2. Session-bound wallet resolution.
//! 3. Pending-action guard (mirrors deposit's Phase 5A guard, scoped
//!    to the withdraw park store).
//! 4. Obligation fetch via the read-only [`SolendPositionReader`] seam
//!    Phase 6H established. Single `getAccountInfo` against the
//!    caller-supplied pubkey.
//! 5. Decode + structural validation: owner ==  session wallet, lending
//!    market matches the configured Solend / Save Main Pool, at least
//!    one non-zero USDC-reserve deposit entry, no non-zero borrow
//!    entry. Each failure surfaces a stable wire status.
//! 6. Construct [`ProposedAction::Withdraw`] with the `u64::MAX`
//!    sentinel.
//! 7. Build a [`ParkedSolendWithdrawAllIntent`] and park it under a
//!    freshly generated `approval_request_id` in the
//!    [`SolendWithdrawAllParkStore`]. The explicit obligation pubkey
//!    flows verbatim into the parked entry — no derivation.
//! 8. Return `awaiting_approval` with the `approval_request_id`.
//!
//! # What this slice does NOT do
//!
//! - Does NOT register the approval in the daemon-wide
//!   [`crate::approval_store::ApprovalStore`]. Phase 6I-D returns a
//!   real, parked-intent-keyed `approval_request_id`, but the
//!   approval-routing / lifecycle / decision pipeline is NOT yet
//!   withdraw-aware. A follow-up slice (Phase 6I-E) will register
//!   the approval, add the withdraw arm in `route_approval_outcome`,
//!   and spawn the resume task.
//! - Does NOT spawn a resume task. No fresh-snapshot reassembly, no
//!   JIT signing handoff is created at chat-tool call time.
//! - Does NOT build a transaction. The withdraw substrate
//!   ([`crate::integrations::solend::withdraw`] +
//!   [`crate::integrations::solend_withdraw_tx_plan`]) remains
//!   `#[cfg(test)]`-gated. A follow-up slice will flip those gates
//!   alongside the JIT signing handoff.
//! - Does NOT sign. No `Keypair`, no signing path, no
//!   `create_signing_handoff` call.
//! - Does NOT broadcast. No `send_transaction` / `submit_signed_solend_*`
//!   call site is reachable from this module.
//!
//! # Output statuses (wire-stable)
//!
//! - `awaiting_approval`        — happy path
//! - `wallet_not_bound`         — session has no bound external wallet
//! - `invalid_obligation_pubkey` — input missing / unparseable
//! - `obligation_not_found`     — `getAccountInfo` returned None
//! - `decode_error`             — wrong size or `decode_obligation` Err
//! - `rpc_error`                — RPC seam returned Err
//! - `policy_blocked`           — owner mismatch / no USDC deposit /
//!                                borrow present / lending-market
//!                                mismatch / zero collateral
//! - `pending_action_exists`    — duplicate proposal while a prior one
//!                                for the same (session, wallet) is in
//!                                flight (mirrors deposit guard)
//!
//! Status strings are part of the chat / frontend / audit contract.
//! Reordering or renaming is a wire change.

use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};
use solana_sdk::pubkey::Pubkey;
use tracing::{info, warn};
use uuid::Uuid;

use claw_tool_system::{errors::ToolError, tool::Tool};
use claw_types::tool::{ToolInput, ToolOutput, ToolSpec};

use crate::integrations::solend::raw::{decode_obligation, OBLIGATION_LEN};
use crate::integrations::solend_withdraw_park::{
    ParkedSolendWithdrawAllIntent, SolendWithdrawAllParkStore,
};
use crate::lending::{CollateralTokenAmount, ProposedAction, ProtocolTag};
use crate::tools::get_solend_position::{
    SolendPositionReader, SOLEND_MAIN_POOL_USDC_RESERVE_BS58, USDC_MINT_BS58,
};
use crate::tools::jupiter_swap::SessionBoundWallet;

// ── Constants ───────────────────────────────────────────────────────────────

/// Tool name. Single source of truth — used by the chat allowlist,
/// p5d2 chat-route tests, and the daemon registry.
pub const TOOL_NAME: &str = "solend_withdraw_all_usdc";

/// Solend / Save mainnet lending market that the Phase 6H scanner is
/// pinned to. The withdraw-all execution surface is scoped to this
/// market only; an obligation under any other market is rejected.
const SOLEND_SAVE_MAIN_POOL_LENDING_MARKET_BS58: &str =
    "4UpD2fh7xH3VP9QQaXtsS1YY3bxzWhtfpks7FatyKvdY";

// ── Status strings (wire-stable) ────────────────────────────────────────────

const STATUS_AWAITING_APPROVAL: &str = "awaiting_approval";
const STATUS_WALLET_NOT_BOUND: &str = "wallet_not_bound";
const STATUS_INVALID_OBLIGATION_PUBKEY: &str = "invalid_obligation_pubkey";
const STATUS_OBLIGATION_NOT_FOUND: &str = "obligation_not_found";
const STATUS_DECODE_ERROR: &str = "decode_error";
const STATUS_RPC_ERROR: &str = "rpc_error";
const STATUS_POLICY_BLOCKED: &str = "policy_blocked";
const STATUS_PENDING_ACTION_EXISTS: &str = "pending_action_exists";

const MODE_WITHDRAW_ALL_COLLATERAL: &str = "withdraw_all_collateral";

const ESTIMATE_UNAVAILABLE_REASON: &str =
    "underlying USDC estimate requires the reserve's cToken-supply / total-liquidity \
     exchange rate, which a follow-up slice will decode. Phase 6I-D reports the \
     observed cToken amount exactly and never invents a number.";

const POLICY_RULE_NAME: &str = "solend-withdraw-all-explicit-obligation";

// Stable policy_rule reasons — kept short so the chat surface and the
// audit row see the same string.
const REASON_OWNER_MISMATCH: &str =
    "obligation owner does not match the session-bound wallet";
const REASON_NO_USDC_DEPOSIT: &str =
    "obligation has no non-zero deposit on the Solend / Save Main Pool USDC reserve";
const REASON_BORROW_PRESENT: &str =
    "obligation has at least one non-zero borrow entry; withdraw-all blocked";
const REASON_LENDING_MARKET_MISMATCH: &str =
    "obligation belongs to a Solend / Save lending market other than the one \
     this slice supports";

const APPROVAL_LEASE_SECONDS_DEFAULT: u64 = 300;

// ── Input schema ────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WithdrawAllInput {
    obligation_pubkey: String,
}

// ── Tool ────────────────────────────────────────────────────────────────────

/// Phase 6I-D execution-proposal tool. Returns `awaiting_approval` for
/// a parked withdraw-all intent against an explicit obligation pubkey.
pub struct SubmitSolendWithdrawAllUsdcTool {
    session_wallet_lookup: Arc<dyn SessionBoundWallet>,
    /// Reuses Phase 6H's reader seam — single `getAccountInfo` against
    /// the caller-supplied obligation pubkey. The
    /// `find_obligations_for_owner` arm is unused here; the user
    /// supplies the obligation explicitly.
    reader: Arc<dyn SolendPositionReader>,
    park_store: SolendWithdrawAllParkStore,
    usdc_reserve_pk: Pubkey,
    usdc_mint_pk: Pubkey,
    expected_lending_market_pk: Pubkey,
    /// TTL applied to the parked intent's `expires_at` field. Mirrors
    /// deposit's `approval_lease_seconds` — used for symmetry and so
    /// a future approval-routing slice can reuse the same value.
    approval_lease_seconds: u64,
}

impl SubmitSolendWithdrawAllUsdcTool {
    pub fn new(
        session_wallet_lookup: Arc<dyn SessionBoundWallet>,
        reader: Arc<dyn SolendPositionReader>,
        park_store: SolendWithdrawAllParkStore,
    ) -> Self {
        Self {
            session_wallet_lookup,
            reader,
            park_store,
            usdc_reserve_pk: Pubkey::from_str(SOLEND_MAIN_POOL_USDC_RESERVE_BS58)
                .expect("hard-coded USDC reserve pubkey parses"),
            usdc_mint_pk: Pubkey::from_str(USDC_MINT_BS58)
                .expect("hard-coded USDC mint parses"),
            expected_lending_market_pk: Pubkey::from_str(
                SOLEND_SAVE_MAIN_POOL_LENDING_MARKET_BS58,
            )
            .expect("hard-coded lending market pubkey parses"),
            approval_lease_seconds: APPROVAL_LEASE_SECONDS_DEFAULT,
        }
    }

    /// Test / config override of the approval lease.
    pub fn with_approval_lease_seconds(mut self, secs: u64) -> Self {
        self.approval_lease_seconds = secs;
        self
    }
}

#[async_trait]
impl Tool for SubmitSolendWithdrawAllUsdcTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: TOOL_NAME.to_string(),
            description: "Propose a Solend / Save withdraw-all of USDC from a SPECIFIC \
                          existing obligation owned by the session-bound wallet. \
                          Withdraw-all only — no partial amount field. The user must \
                          supply `obligation_pubkey` explicitly (typically copied from \
                          a prior `get_solend_position` or `preview_solend_withdraw_all` \
                          result). The tool validates ownership, USDC deposit presence, \
                          and zero borrow; on success it parks an awaiting-approval \
                          intent. It does NOT sign, broadcast, or build a transaction."
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
            // The chat path's `propose_signing` capability (deposit /
            // jupiter swap) is what gates execution-tier tools. Withdraw
            // belongs in that tier — same wire-shape commitments.
            required_capabilities: vec!["propose_signing".to_string()],
            supports_streaming: false,
            timeout_ms: 15_000,
        }
    }

    async fn execute(&self, input: ToolInput) -> Result<ToolOutput, ToolError> {
        // 1. Strict input parse (deny_unknown_fields).
        let parsed: WithdrawAllInput =
            serde_json::from_value(input.parameters.clone()).map_err(|e| {
                warn!(
                    session = %input.session_id,
                    err = %e,
                    "solend_withdraw_all_usdc: rejected unknown / malformed field"
                );
                ToolError::InvalidInput {
                    reason: format!(
                        "solend_withdraw_all_usdc accepts only `obligation_pubkey`. \
                         Hallucinated or malformed field rejected: {e}"
                    ),
                }
            })?;

        // 2. Resolve session-bound wallet.
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
                warn!(
                    session = %input.session_id,
                    bound_wallet = %wallet_bs58,
                    err = %e,
                    "solend_withdraw_all_usdc: bound wallet is not a valid Pubkey"
                );
                return Ok(decode_error_output(&format!(
                    "session wallet binding is not a valid pubkey: {e}"
                )));
            }
        };

        // 3. Validate obligation_pubkey.
        let obligation_pk = match Pubkey::from_str(&parsed.obligation_pubkey) {
            Ok(pk) => pk,
            Err(e) => {
                return Ok(invalid_obligation_pubkey_output(&format!(
                    "obligation_pubkey is not a valid pubkey: {e}"
                )))
            }
        };

        // 3a. Pending-action guard. Mirrors deposit's Phase 5A
        // concurrency guard: refuse a duplicate proposal while a prior
        // one for the same (session, wallet) is still parked.
        if self
            .park_store
            .has_active_for_session_wallet(&input.session_id, &wallet_pk)
        {
            return Ok(pending_action_exists_output(&wallet_pk));
        }

        // 4. Fetch obligation account.
        let data = match self.reader.get_account_data(&obligation_pk).await {
            Ok(Some(d)) => d,
            Ok(None) => return Ok(obligation_not_found_output(&obligation_pk)),
            Err(e) => return Ok(rpc_error_output("get_account_data", &e)),
        };

        // 5. Length + decode.
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

        // 6. Owner match (policy: explicit-obligation owner-check).
        if decoded.owner != wallet_pk {
            return Ok(policy_blocked_output(
                &wallet_bs58,
                &obligation_pk,
                Some(&decoded.lending_market),
                None,
                None,
                REASON_OWNER_MISMATCH,
            ));
        }

        // 7. Lending-market match. The withdraw-all execution surface
        // is scoped to Solend / Save Main Pool only.
        if decoded.lending_market != self.expected_lending_market_pk {
            return Ok(policy_blocked_output(
                &wallet_bs58,
                &obligation_pk,
                Some(&decoded.lending_market),
                None,
                None,
                REASON_LENDING_MARKET_MISMATCH,
            ));
        }

        // 8. Borrow check — block before reporting deposit details.
        let has_any_borrow = decoded
            .borrows
            .iter()
            .any(|b| b.borrowed_amount_wads > 0);
        if has_any_borrow {
            return Ok(policy_blocked_output(
                &wallet_bs58,
                &obligation_pk,
                Some(&decoded.lending_market),
                None,
                Some(decoded.borrows.len()),
                REASON_BORROW_PRESENT,
            ));
        }

        // 9. USDC-reserve deposit lookup.
        let usdc_deposit = decoded.deposits.iter().find(|d| {
            d.deposit_reserve == self.usdc_reserve_pk && d.deposited_amount > 0
        });
        let collateral_amount = match usdc_deposit {
            Some(d) => d.deposited_amount,
            None => {
                return Ok(policy_blocked_output(
                    &wallet_bs58,
                    &obligation_pk,
                    Some(&decoded.lending_market),
                    None,
                    None,
                    REASON_NO_USDC_DEPOSIT,
                ))
            }
        };

        // 10. Construct the typed withdraw action with the `u64::MAX`
        //     sentinel. The on-chain Solend program clamps to the
        //     obligation's actual deposit; we never invent a numeric
        //     amount on this surface.
        let action = ProposedAction::Withdraw {
            protocol: ProtocolTag::Solend,
            reserve_mint: self.usdc_mint_pk,
            collateral_amount: CollateralTokenAmount::new(u64::MAX),
        };

        // 11. Park the intent. The ID generated here is the
        //     `approval_request_id` returned to the caller AND the
        //     park-store key — a future slice reuses this id when it
        //     wires the intent into the daemon's approval pipeline.
        let intent_id = Uuid::new_v4();
        let approval_request_id = Uuid::new_v4();
        let proposed_at = Utc::now();
        let expires_at = proposed_at + chrono::Duration::seconds(self.approval_lease_seconds as i64);
        let parked = ParkedSolendWithdrawAllIntent {
            intent_id,
            session_id: input.session_id.clone(),
            session_wallet: wallet_pk,
            // Explicit pubkey from the user — verbatim, no derivation.
            obligation_pubkey: obligation_pk,
            lending_market: decoded.lending_market,
            reserve_pubkey: self.usdc_reserve_pk,
            reserve_mint: self.usdc_mint_pk,
            propose_time_deposited_collateral_raw: collateral_amount,
            action,
            proposed_at,
            expires_at,
        };
        if self.park_store.park(approval_request_id, parked).is_err() {
            // UUID collision is essentially impossible; surface as a
            // typed internal error so the LLM sees a stable, non-PII
            // failure rather than a panic.
            return Ok(decode_error_output(
                "internal: failed to park withdraw intent (id collision)",
            ));
        }

        info!(
            session = %input.session_id,
            intent_id = %intent_id,
            approval_request_id = %approval_request_id,
            wallet = %wallet_pk,
            obligation = %obligation_pk,
            collateral_raw = collateral_amount,
            "solend_withdraw_all_usdc: parked withdraw-all intent, returning awaiting_approval"
        );

        Ok(awaiting_approval_output(
            &wallet_bs58,
            &obligation_pk,
            &decoded.lending_market,
            &self.usdc_reserve_pk,
            &self.usdc_mint_pk,
            collateral_amount,
            approval_request_id,
            intent_id,
        ))
    }
}

// ── Output helpers ──────────────────────────────────────────────────────────

fn wallet_not_bound_output() -> ToolOutput {
    let reason = "no external wallet is bound to this session; bind one via the \
                  wallet-bind challenge on /chat before proposing a Solend withdraw";
    ToolOutput {
        tool_name: TOOL_NAME.to_string(),
        success: false,
        data: Some(json!({
            "status":              STATUS_WALLET_NOT_BOUND,
            "approval_request_id": Value::Null,
            "reason":              reason,
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
            "status":              STATUS_INVALID_OBLIGATION_PUBKEY,
            "approval_request_id": Value::Null,
            "reason":              reason,
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
            "status":              STATUS_OBLIGATION_NOT_FOUND,
            "obligation_pubkey":   obligation_pk.to_string(),
            "approval_request_id": Value::Null,
            "reason":              "obligation account does not exist on chain",
        })),
        error: Some(format!("obligation {obligation_pk} not found")),
        duration_ms: 0,
    }
}

fn rpc_error_output(phase: &str, msg: &str) -> ToolOutput {
    ToolOutput {
        tool_name: TOOL_NAME.to_string(),
        success: false,
        data: Some(json!({
            "status":              STATUS_RPC_ERROR,
            "phase":               phase,
            "approval_request_id": Value::Null,
            "reason":              msg,
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
            "status":              STATUS_DECODE_ERROR,
            "approval_request_id": Value::Null,
            "reason":              msg,
        })),
        error: Some(format!("decode_error: {msg}")),
        duration_ms: 0,
    }
}

fn pending_action_exists_output(wallet_pk: &Pubkey) -> ToolOutput {
    let reason = "a prior solend_withdraw_all_usdc proposal for this session is still \
                  awaiting approval, rejection, or expiry; wait for that to resolve \
                  before submitting a new proposal";
    ToolOutput {
        tool_name: TOOL_NAME.to_string(),
        success: false,
        data: Some(json!({
            "status":                  STATUS_PENDING_ACTION_EXISTS,
            "session_wallet":          wallet_pk.to_string(),
            "approval_request_id":     Value::Null,
            "human_readable_next_step":
                "Wait for the existing pending Solend withdraw to be approved, rejected, or to expire.",
            "reason":                  reason,
        })),
        error: Some(reason.to_string()),
        duration_ms: 0,
    }
}

#[allow(clippy::too_many_arguments)]
fn policy_blocked_output(
    wallet_bs58: &str,
    obligation_pk: &Pubkey,
    lending_market: Option<&Pubkey>,
    collateral_amount_raw: Option<u64>,
    borrow_entry_count: Option<usize>,
    reason: &str,
) -> ToolOutput {
    ToolOutput {
        tool_name: TOOL_NAME.to_string(),
        success: false,
        data: Some(json!({
            "status":                STATUS_POLICY_BLOCKED,
            "protocol":              "Solend/Save",
            "network":               "mainnet",
            "mode":                  MODE_WITHDRAW_ALL_COLLATERAL,
            "asset":                 "USDC",
            "wallet_pubkey":         wallet_bs58,
            "obligation_pubkey":     obligation_pk.to_string(),
            "lending_market":        lending_market.map(|m| m.to_string()),
            "reserve_pubkey":        SOLEND_MAIN_POOL_USDC_RESERVE_BS58,
            "reserve_mint":          USDC_MINT_BS58,
            "collateral_amount_raw": collateral_amount_raw.map(|n| n.to_string()),
            "borrow_entry_count":    borrow_entry_count,
            "approval_request_id":   Value::Null,
            "policy_verdict":        "HardBlock",
            "policy_rule_name":      POLICY_RULE_NAME,
            "human_readable_reason": reason,
            "human_readable_next_step": "Request blocked by policy.",
        })),
        error: Some(format!("policy_blocked: {reason}")),
        duration_ms: 0,
    }
}

#[allow(clippy::too_many_arguments)]
fn awaiting_approval_output(
    wallet_bs58: &str,
    obligation_pk: &Pubkey,
    lending_market: &Pubkey,
    usdc_reserve: &Pubkey,
    usdc_mint: &Pubkey,
    collateral_amount_raw: u64,
    approval_request_id: Uuid,
    intent_id: Uuid,
) -> ToolOutput {
    ToolOutput {
        tool_name: TOOL_NAME.to_string(),
        success: true,
        data: Some(json!({
            "status":                          STATUS_AWAITING_APPROVAL,
            "protocol":                        "Solend/Save",
            "network":                         "mainnet",
            "mode":                            MODE_WITHDRAW_ALL_COLLATERAL,
            "asset":                           "USDC",
            "wallet_pubkey":                   wallet_bs58,
            "obligation_pubkey":               obligation_pk.to_string(),
            "lending_market":                  lending_market.to_string(),
            "reserve_pubkey":                  usdc_reserve.to_string(),
            "reserve_mint":                    usdc_mint.to_string(),
            "collateral_amount_raw":           collateral_amount_raw.to_string(),
            "underlying_usdc_estimate_raw":    Value::Null,
            "underlying_usdc_estimate_ui":     Value::Null,
            "estimate_unavailable_reason":     ESTIMATE_UNAVAILABLE_REASON,
            "requires_user_signature":         true,
            "required_signers":                ["wallet"],
            "requires_obligation_keypair":     false,
            "intent_id":                       intent_id.to_string(),
            "approval_request_id":             approval_request_id.to_string(),
            "approval_required":               true,
            "will_build_transaction_on_sign_click": true,
            "will_sign":                       false,
            "will_broadcast":                  false,
            "policy_verdict":                  "Pass",
            "policy_rule_name":                POLICY_RULE_NAME,
            "human_readable_reason":
                "All Phase 6I-D preconditions satisfied (owner match, USDC deposit \
                 present, no borrow, lending market match). Awaiting operator approval.",
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
        fn session_wallet_pubkey(&self, _: &SessionId) -> Option<String> {
            self.0.clone()
        }
    }

    struct StubReader {
        accounts: Mutex<HashMap<Pubkey, Vec<u8>>>,
        get_err: Mutex<Option<String>>,
    }
    impl StubReader {
        fn empty() -> Self {
            Self {
                accounts: Mutex::new(HashMap::new()),
                get_err: Mutex::new(None),
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
            _: &Pubkey,
        ) -> Result<Vec<(Pubkey, Vec<u8>)>, String> {
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

    fn save_main_pool_lending_market_pk() -> Pubkey {
        Pubkey::from_str(SOLEND_SAVE_MAIN_POOL_LENDING_MARKET_BS58).unwrap()
    }

    /// Build a 1300-byte obligation with `owner`, `lending_market`, one
    /// USDC-reserve deposit at `deposited_amount`, and the supplied
    /// borrow array.
    fn build_obligation(
        owner: Pubkey,
        lending_market: Pubkey,
        deposit_reserve: Pubkey,
        deposited_amount: u64,
        borrows: &[SolendObligationLiquidityRaw],
    ) -> Vec<u8> {
        let deposits = if deposited_amount > 0 {
            vec![SolendObligationCollateralRaw {
                deposit_reserve,
                deposited_amount,
            }]
        } else {
            vec![]
        };
        synth_obligation(owner, lending_market, 0, false, &deposits, borrows)
    }

    fn make_tool(
        bound: Option<&str>,
        reader: Arc<dyn SolendPositionReader>,
    ) -> (SubmitSolendWithdrawAllUsdcTool, SolendWithdrawAllParkStore) {
        let park_store = SolendWithdrawAllParkStore::new();
        let tool = SubmitSolendWithdrawAllUsdcTool::new(
            Arc::new(StubSessionWallet(bound.map(|s| s.to_string()))),
            reader,
            park_store.clone(),
        );
        (tool, park_store)
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

    // ── Required test 1 — wallet_not_bound ─────────────────────────────

    #[tokio::test]
    async fn wallet_not_bound_returns_status() {
        let (tool, park) = make_tool(None, Arc::new(StubReader::empty()));
        let out = tool
            .execute(input_for(
                &SessionId::new(),
                &Pubkey::new_unique().to_string(),
            ))
            .await
            .unwrap();
        assert_eq!(data_of(&out)["status"], STATUS_WALLET_NOT_BOUND);
        assert!(!out.success);
        assert_eq!(park.parked_count(), 0);
    }

    // ── Required test 2 — invalid_obligation_pubkey ────────────────────

    #[tokio::test]
    async fn invalid_obligation_pubkey_garbage_string() {
        let owner = Pubkey::new_unique();
        let (tool, park) = make_tool(Some(&owner.to_string()), Arc::new(StubReader::empty()));
        let out = tool
            .execute(input_for(&SessionId::new(), "not-a-pubkey"))
            .await
            .unwrap();
        assert_eq!(data_of(&out)["status"], STATUS_INVALID_OBLIGATION_PUBKEY);
        assert_eq!(park.parked_count(), 0);
    }

    // ── Required test 3 — obligation_not_found ─────────────────────────

    #[tokio::test]
    async fn obligation_not_found_returns_status() {
        let owner = Pubkey::new_unique();
        let missing_pk = Pubkey::new_unique();
        let (tool, park) = make_tool(Some(&owner.to_string()), Arc::new(StubReader::empty()));
        let out = tool
            .execute(input_for(&SessionId::new(), &missing_pk.to_string()))
            .await
            .unwrap();
        let d = data_of(&out);
        assert_eq!(d["status"], STATUS_OBLIGATION_NOT_FOUND);
        assert_eq!(d["obligation_pubkey"], missing_pk.to_string());
        assert_eq!(park.parked_count(), 0);
    }

    // ── Required test 4 — owner_mismatch -> policy_blocked ─────────────

    #[tokio::test]
    async fn owner_mismatch_returns_policy_blocked() {
        let session_wallet = Pubkey::new_unique();
        let foreign_owner = Pubkey::new_unique();
        let obl_pk = Pubkey::new_unique();
        let bytes = build_obligation(
            foreign_owner,
            save_main_pool_lending_market_pk(),
            usdc_reserve_pk(),
            5_000,
            &[],
        );
        let reader = Arc::new(StubReader::with_account(obl_pk, bytes));
        let (tool, park) = make_tool(Some(&session_wallet.to_string()), reader);
        let out = tool
            .execute(input_for(&SessionId::new(), &obl_pk.to_string()))
            .await
            .unwrap();
        let d = data_of(&out);
        assert_eq!(d["status"], STATUS_POLICY_BLOCKED);
        assert_eq!(d["policy_rule_name"], POLICY_RULE_NAME);
        assert!(d["human_readable_reason"]
            .as_str()
            .unwrap_or_default()
            .contains("owner does not match"));
        assert_eq!(park.parked_count(), 0);
    }

    // ── Required test 5 — no_usdc_deposit -> policy_blocked ────────────

    #[tokio::test]
    async fn no_usdc_deposit_returns_policy_blocked() {
        let owner = Pubkey::new_unique();
        let obl_pk = Pubkey::new_unique();
        let other_reserve = Pubkey::new_unique();
        let bytes = build_obligation(
            owner,
            save_main_pool_lending_market_pk(),
            other_reserve,
            5_000,
            &[],
        );
        let reader = Arc::new(StubReader::with_account(obl_pk, bytes));
        let (tool, park) = make_tool(Some(&owner.to_string()), reader);
        let out = tool
            .execute(input_for(&SessionId::new(), &obl_pk.to_string()))
            .await
            .unwrap();
        let d = data_of(&out);
        assert_eq!(d["status"], STATUS_POLICY_BLOCKED);
        assert!(d["human_readable_reason"]
            .as_str()
            .unwrap_or_default()
            .contains("no non-zero deposit on the Solend / Save Main Pool USDC reserve"));
        assert_eq!(park.parked_count(), 0);
    }

    // ── Required test 6 — borrow present -> policy_blocked ─────────────

    #[tokio::test]
    async fn borrow_present_returns_policy_blocked() {
        let owner = Pubkey::new_unique();
        let obl_pk = Pubkey::new_unique();
        let borrow_reserve = Pubkey::new_unique();
        let borrows = [SolendObligationLiquidityRaw {
            borrow_reserve,
            borrowed_amount_wads: 1_000_000_000_000_000_000u128,
        }];
        let bytes = build_obligation(
            owner,
            save_main_pool_lending_market_pk(),
            usdc_reserve_pk(),
            3_857_506,
            &borrows,
        );
        let reader = Arc::new(StubReader::with_account(obl_pk, bytes));
        let (tool, park) = make_tool(Some(&owner.to_string()), reader);
        let out = tool
            .execute(input_for(&SessionId::new(), &obl_pk.to_string()))
            .await
            .unwrap();
        let d = data_of(&out);
        assert_eq!(d["status"], STATUS_POLICY_BLOCKED);
        assert!(d["human_readable_reason"]
            .as_str()
            .unwrap_or_default()
            .contains("non-zero borrow"));
        assert_eq!(d["borrow_entry_count"], 1);
        assert_eq!(park.parked_count(), 0);
    }

    // ── Required tests 7 + 8 — happy path ──────────────────────────────

    /// Required test (7) and (8): the largest Phase 6H obligation
    /// (`HcKrv5Jo…`) for the live wallet `C4QQjz…` produces an
    /// awaiting_approval output with every Phase 6I-D invariant flag
    /// set correctly AND a real `approval_request_id`.
    #[tokio::test]
    async fn phase6h_largest_obligation_returns_awaiting_approval_with_correct_flags() {
        let wallet =
            Pubkey::from_str("C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW").unwrap();
        let obligation_pk =
            Pubkey::from_str("HcKrv5Jo5f6qvzSGhJVYTNSqwKudRizn6fxbjPW7M8SV").unwrap();
        let bytes = build_obligation(
            wallet,
            save_main_pool_lending_market_pk(),
            usdc_reserve_pk(),
            3_857_506,
            &[],
        );
        let reader = Arc::new(StubReader::with_account(obligation_pk, bytes));
        let (tool, park) = make_tool(Some(&wallet.to_string()), reader);
        let out = tool
            .execute(input_for(&SessionId::new(), &obligation_pk.to_string()))
            .await
            .unwrap();
        assert!(out.success);
        let d = data_of(&out);
        assert_eq!(d["status"], STATUS_AWAITING_APPROVAL);
        assert_eq!(d["protocol"], "Solend/Save");
        assert_eq!(d["network"], "mainnet");
        assert_eq!(d["mode"], MODE_WITHDRAW_ALL_COLLATERAL);
        assert_eq!(d["asset"], "USDC");
        assert_eq!(d["wallet_pubkey"], wallet.to_string());
        assert_eq!(d["obligation_pubkey"], obligation_pk.to_string());
        assert_eq!(
            d["lending_market"],
            SOLEND_SAVE_MAIN_POOL_LENDING_MARKET_BS58
        );
        assert_eq!(d["reserve_pubkey"], SOLEND_MAIN_POOL_USDC_RESERVE_BS58);
        assert_eq!(d["reserve_mint"], USDC_MINT_BS58);
        assert_eq!(d["collateral_amount_raw"], "3857506");
        assert_eq!(d["underlying_usdc_estimate_raw"], JsonValue::Null);
        assert_eq!(d["requires_user_signature"], JsonValue::Bool(true));
        assert_eq!(d["required_signers"], json!(["wallet"]));
        assert_eq!(d["requires_obligation_keypair"], JsonValue::Bool(false));
        assert_eq!(d["will_build_transaction_on_sign_click"], JsonValue::Bool(true));
        assert_eq!(d["will_sign"], JsonValue::Bool(false));
        assert_eq!(d["will_broadcast"], JsonValue::Bool(false));
        assert_eq!(d["policy_rule_name"], POLICY_RULE_NAME);
        // approval_request_id is a parseable UUID v4.
        let arid = d["approval_request_id"].as_str().unwrap();
        let arid_uuid = Uuid::from_str(arid).expect("approval_request_id must be a UUID");
        assert_eq!(d["approval_required"], JsonValue::Bool(true));

        // Park store has exactly one entry under that id.
        assert_eq!(park.parked_count(), 1);
        let parked = park.get(&arid_uuid).expect("parked intent present");
        assert_eq!(parked.session_wallet, wallet);
        assert_eq!(parked.obligation_pubkey, obligation_pk);
        assert_eq!(parked.propose_time_deposited_collateral_raw, 3_857_506);
    }

    // ── Required test 9 — policy-block paths do NOT park ───────────────

    #[tokio::test]
    async fn policy_block_paths_do_not_park_an_intent() {
        for (label, build) in [
            (
                "owner_mismatch",
                build_obligation(
                    Pubkey::new_unique(), // foreign owner
                    save_main_pool_lending_market_pk(),
                    usdc_reserve_pk(),
                    5_000,
                    &[],
                ),
            ),
            (
                "no_usdc_deposit",
                build_obligation(
                    Pubkey::new_unique(),
                    save_main_pool_lending_market_pk(),
                    Pubkey::new_unique(), // non-USDC reserve
                    5_000,
                    &[],
                ),
            ),
            (
                "borrow_present",
                build_obligation(
                    Pubkey::new_unique(),
                    save_main_pool_lending_market_pk(),
                    usdc_reserve_pk(),
                    3_857_506,
                    &[SolendObligationLiquidityRaw {
                        borrow_reserve: Pubkey::new_unique(),
                        borrowed_amount_wads: 1_000_000_000_000_000_000u128,
                    }],
                ),
            ),
            (
                "lending_market_mismatch",
                build_obligation(
                    Pubkey::new_unique(),
                    Pubkey::new_unique(), // foreign market
                    usdc_reserve_pk(),
                    5_000,
                    &[],
                ),
            ),
        ] {
            // For owner_mismatch we need the SESSION wallet to differ
            // from the obligation's owner; for the others it's enough
            // that the lender/reserve checks fire BEFORE or AFTER the
            // owner check — set session wallet to the obligation owner
            // so owner-check passes for those cases.
            let session_wallet = if label == "owner_mismatch" {
                Pubkey::new_unique()
            } else {
                // Pull the synthesized owner out of the bytes we built.
                // Owner is at offset 42 per the obligation layout.
                Pubkey::try_from(&build[42..74]).unwrap()
            };
            let obl_pk = Pubkey::new_unique();
            let reader = Arc::new(StubReader::with_account(obl_pk, build));
            let (tool, park) = make_tool(Some(&session_wallet.to_string()), reader);
            let out = tool
                .execute(input_for(&SessionId::new(), &obl_pk.to_string()))
                .await
                .unwrap();
            assert_eq!(
                data_of(&out)["status"],
                STATUS_POLICY_BLOCKED,
                "label={label}"
            );
            assert_eq!(park.parked_count(), 0, "label={label} must not park");
        }
    }

    // ── Required test 10 — source guard ────────────────────────────────

    #[test]
    fn module_has_no_forbidden_runtime_references() {
        const SOURCE: &str = include_str!("solend_withdraw_all_usdc.rs");
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
            format!("{}{}", "Keypair::", "from"),
            format!("{}{}", "create_signing_", "handoff("),
            format!("{}{}", "submit_signed_solend_", "transaction("),
            format!("{}{}", "priv", "ate_key"),
            format!("{}{}", "tx_", "bytes"),
            format!("{}{}", "sign", "ed_bytes"),
            format!("{}{}", "transaction_", "base64"),
        ];
        for n in &needles {
            assert!(
                !SOURCE.contains(n),
                "solend_withdraw_all_usdc.rs must not contain `{n}`"
            );
        }
    }

    // ── Required test 11 — park stores explicit obligation pubkey ──────

    /// The parked intent's `obligation_pubkey` field must equal the
    /// one the LLM/user supplied — not a derived PDA, not a wallet
    /// address, not an arbitrary substitute. (The same invariant is
    /// also asserted at the park-store level in
    /// `solend_withdraw_park::tests::park_explicit_obligation_pubkey_is_stored_verbatim_not_derived`.)
    #[tokio::test]
    async fn park_intent_stores_explicit_obligation_pubkey_not_derived() {
        let wallet = Pubkey::new_unique();
        let obligation_pk =
            Pubkey::from_str("HcKrv5Jo5f6qvzSGhJVYTNSqwKudRizn6fxbjPW7M8SV").unwrap();
        let bytes = build_obligation(
            wallet,
            save_main_pool_lending_market_pk(),
            usdc_reserve_pk(),
            3_857_506,
            &[],
        );
        let reader = Arc::new(StubReader::with_account(obligation_pk, bytes));
        let (tool, park) = make_tool(Some(&wallet.to_string()), reader);
        let out = tool
            .execute(input_for(&SessionId::new(), &obligation_pk.to_string()))
            .await
            .unwrap();
        let arid =
            Uuid::from_str(data_of(&out)["approval_request_id"].as_str().unwrap()).unwrap();
        let parked = park.get(&arid).expect("parked");
        assert_eq!(
            parked.obligation_pubkey, obligation_pk,
            "parked intent must carry the explicit obligation pubkey verbatim"
        );
        // Defense: the parked obligation pubkey is NOT the wallet
        // (a common bug where a derive routine accidentally returns
        // the owner).
        assert_ne!(parked.obligation_pubkey, wallet);
    }

    // ── Required test 12 — signer set is user wallet only ──────────────

    /// The parked action's structural intent fields name only the
    /// session wallet as the signer side. The output's
    /// `required_signers` lists only `"wallet"`. There is no
    /// reference to a daemon / fee-payer / service signer anywhere.
    #[tokio::test]
    async fn signer_set_is_user_wallet_only_no_daemon_signer() {
        let wallet = Pubkey::new_unique();
        let obligation_pk = Pubkey::new_unique();
        let bytes = build_obligation(
            wallet,
            save_main_pool_lending_market_pk(),
            usdc_reserve_pk(),
            3_857_506,
            &[],
        );
        let reader = Arc::new(StubReader::with_account(obligation_pk, bytes));
        let (tool, _park) = make_tool(Some(&wallet.to_string()), reader);
        let out = tool
            .execute(input_for(&SessionId::new(), &obligation_pk.to_string()))
            .await
            .unwrap();
        let d = data_of(&out);
        assert_eq!(d["required_signers"], json!(["wallet"]));
        assert_eq!(d["wallet_pubkey"], wallet.to_string());

        // Output JSON must not mention a daemon / service signer.
        let s = serde_json::to_string(&out.data).unwrap();
        let forbidden = [
            format!("{}{}", "daemon_", "signer"),
            format!("{}{}", "fee_payer_", "signer"),
            format!("{}{}", "service_", "keypair"),
            format!("{}{}", "claw_", "signer_keypair"),
        ];
        for n in &forbidden {
            assert!(!s.contains(n), "must not surface signer identifier `{n}`");
        }
    }

    // ── Required test 13 — obligation keypair NOT required ─────────────

    #[tokio::test]
    async fn obligation_keypair_not_required_in_happy_path_output() {
        let wallet = Pubkey::new_unique();
        let obligation_pk = Pubkey::new_unique();
        let bytes = build_obligation(
            wallet,
            save_main_pool_lending_market_pk(),
            usdc_reserve_pk(),
            3_857_506,
            &[],
        );
        let reader = Arc::new(StubReader::with_account(obligation_pk, bytes));
        let (tool, _park) = make_tool(Some(&wallet.to_string()), reader);
        let out = tool
            .execute(input_for(&SessionId::new(), &obligation_pk.to_string()))
            .await
            .unwrap();
        assert_eq!(
            data_of(&out)["requires_obligation_keypair"],
            JsonValue::Bool(false)
        );
    }

    // ── Required test 14 — extra fields rejected by deny_unknown_fields ─

    #[tokio::test]
    async fn extra_fields_in_input_rejected_by_deny_unknown_fields() {
        let wallet = Pubkey::new_unique();
        let obl_pk = Pubkey::new_unique();
        let (tool, _park) = make_tool(Some(&wallet.to_string()), Arc::new(StubReader::empty()));
        // amount field — partial-withdraw attempt.
        let res = tool
            .execute(input_with_raw_params(
                &SessionId::new(),
                json!({
                    "obligation_pubkey": obl_pk.to_string(),
                    "amount": 1_000_000,
                }),
            ))
            .await;
        match res {
            Err(ToolError::InvalidInput { reason }) => {
                assert!(
                    reason.contains("only `obligation_pubkey`"),
                    "rejection reason should name the only allowed field; got: {reason}"
                );
            }
            other => panic!("expected InvalidInput rejecting `amount`; got {other:?}"),
        }

        // mode field — sneaky enum.
        let res = tool
            .execute(input_with_raw_params(
                &SessionId::new(),
                json!({
                    "obligation_pubkey": obl_pk.to_string(),
                    "mode": "withdraw_all_collateral",
                }),
            ))
            .await;
        assert!(matches!(res, Err(ToolError::InvalidInput { .. })));

        // wallet_pubkey field — session-binding bypass attempt.
        let res = tool
            .execute(input_with_raw_params(
                &SessionId::new(),
                json!({
                    "obligation_pubkey": obl_pk.to_string(),
                    "wallet_pubkey": wallet.to_string(),
                }),
            ))
            .await;
        assert!(matches!(res, Err(ToolError::InvalidInput { .. })));
    }

    // ── Bonus: pending-action guard ────────────────────────────────────

    #[tokio::test]
    async fn second_proposal_for_same_session_wallet_is_blocked() {
        let wallet = Pubkey::new_unique();
        let obl_pk = Pubkey::new_unique();
        let bytes = build_obligation(
            wallet,
            save_main_pool_lending_market_pk(),
            usdc_reserve_pk(),
            3_857_506,
            &[],
        );
        let reader = Arc::new(StubReader::with_account(obl_pk, bytes));
        let (tool, park) = make_tool(Some(&wallet.to_string()), reader);
        let sid = SessionId::new();
        // First call parks.
        let out1 = tool.execute(input_for(&sid, &obl_pk.to_string())).await.unwrap();
        assert_eq!(data_of(&out1)["status"], STATUS_AWAITING_APPROVAL);
        assert_eq!(park.parked_count(), 1);
        // Second call from the same session+wallet pair is refused.
        let out2 = tool.execute(input_for(&sid, &obl_pk.to_string())).await.unwrap();
        assert_eq!(data_of(&out2)["status"], STATUS_PENDING_ACTION_EXISTS);
        assert_eq!(park.parked_count(), 1);
    }

    // ── Bonus: spec invariant ──────────────────────────────────────────

    #[tokio::test]
    async fn spec_input_schema_demands_only_obligation_pubkey() {
        let (tool, _park) = make_tool(None, Arc::new(StubReader::empty()));
        let spec = tool.spec();
        assert_eq!(spec.name, TOOL_NAME);
        assert_eq!(spec.input_schema["additionalProperties"], json!(false));
        assert_eq!(spec.input_schema["required"], json!(["obligation_pubkey"]));
        let props = spec.input_schema["properties"].as_object().unwrap();
        assert!(props.contains_key("obligation_pubkey"));
        assert_eq!(props.len(), 1);
    }

    #[tokio::test]
    async fn rpc_error_during_get_account_returns_rpc_error() {
        let owner = Pubkey::new_unique();
        let pk = Pubkey::new_unique();
        let reader = Arc::new(StubReader::with_rpc_err("503 Service Unavailable"));
        let (tool, park) = make_tool(Some(&owner.to_string()), reader);
        let out = tool
            .execute(input_for(&SessionId::new(), &pk.to_string()))
            .await
            .unwrap();
        let d = data_of(&out);
        assert_eq!(d["status"], STATUS_RPC_ERROR);
        assert_eq!(d["phase"], "get_account_data");
        assert!(d["reason"].as_str().unwrap_or_default().contains("503"));
        assert_eq!(park.parked_count(), 0);
    }
}
