//! Phase 6I-D — Solend withdraw-all park store.
//!
//! Companion to [`crate::integrations::solend_park`] (deposit park
//! store) but strictly scoped to the **withdraw-all** flow. Holds the
//! parked intent that a future slice will resume at sign-click time:
//!
//!   1. `solend_withdraw_all_usdc` chat tool validates inputs and parks
//!      a [`ParkedSolendWithdrawAllIntent`] in [`SolendWithdrawAllParkStore`].
//!   2. (Future slice) approval-routing branch fires when the operator
//!      decides; the resume task assembles a fresh withdraw plan via
//!      the still-`#[cfg(test)]`-gated
//!      [`crate::integrations::solend_withdraw_tx_plan::assemble_solend_withdraw_tx_plan`]
//!      and creates a JIT signing handoff.
//!   3. (Future slice) Phantom signs → daemon submits → Phase 6D / 6E
//!      rebroadcast handles confirmation.
//!
//! # What this module owns (Phase 6I-D)
//!
//! - The parked-intent shape (`ParkedSolendWithdrawAllIntent`) — frozen
//!   facts the chat tool gathered: explicit `obligation_pubkey` from
//!   the user, the wallet, the propose-time validation outcome, the
//!   action.
//! - A simple in-memory park store keyed by `approval_request_id`.
//!
//! # What this module deliberately does NOT do
//!
//! - Does NOT register with [`crate::approval_store::ApprovalStore`].
//!   Phase 6I-D returns a generated `approval_request_id` for the
//!   parked-intent key, but the daemon-wide approval-routing pipeline
//!   is NOT yet aware of withdraws. A follow-up slice (likely 6I-E)
//!   will register the approval and add the withdraw arm in
//!   `route_approval_outcome`.
//! - Does NOT spawn a resume task. No fresh-snapshot reassembly, no
//!   JIT signing handoff, no transaction build, no signature, no
//!   broadcast.
//! - Does NOT carry transaction bytes, blockhashes, or signer handles.
//!
//! # Why a separate park store?
//!
//! Deposit's [`crate::integrations::solend_park::ParkedSolendDepositIntent`]
//! carries deposit-specific facts (`obligation_exists`, `source_ata_exists`,
//! `collateral_ata_exists`, propose-time `LendingSnapshot`). Withdraw's
//! preconditions are different (the obligation MUST already exist; the
//! collateral amount is the on-chain decoded amount, not a user-supplied
//! quantity). Co-locating both shapes in one store would create
//! variant-explosion and force every consumer to handle both kinds.
//! A separate store keeps the deposit pipeline byte-identical and
//! avoids any risk to the live deposit flow.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use solana_sdk::pubkey::Pubkey;
use uuid::Uuid;

use claw_types::session::SessionId;

use crate::lending::ProposedAction;

/// Parked withdraw-all intent. The fields here are the facts the chat
/// tool gathered at propose time AND verified against on-chain state
/// — preserving them makes a future fresh-reassembly slice (the
/// pre-sign re-check) able to detect drift between propose-time and
/// sign-time obligation state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParkedSolendWithdrawAllIntent {
    /// The chat tool's intent id (distinct from `approval_request_id`).
    /// Used for log correlation across propose / approve / sign /
    /// broadcast spans.
    pub intent_id: Uuid,

    /// The session that originated this intent. Cleanup / pending-action
    /// guards key off this.
    pub session_id: SessionId,

    /// The session-bound external wallet — the signer the future JIT
    /// handoff will hand off to Phantom.
    pub session_wallet: Pubkey,

    /// **Explicit** obligation pubkey supplied by the LLM / user. This
    /// is NOT derived; it is the same string the Phase 6H scanner
    /// reported. The future resume / assembler MUST thread this value
    /// verbatim into the withdraw ix at slot 3.
    pub obligation_pubkey: Pubkey,

    /// Validated lending market — frozen at propose time. Sign-time
    /// re-check compares against this to detect on-chain drift.
    pub lending_market: Pubkey,

    /// Reserve pubkey — Solend / Save Main Pool USDC reserve at propose
    /// time. The withdraw substrate is currently scoped to this single
    /// reserve.
    pub reserve_pubkey: Pubkey,

    /// USDC mint — frozen for symmetry with deposit's parked intent.
    pub reserve_mint: Pubkey,

    /// Decoded `deposited_amount` (cToken collateral base units) at
    /// propose time. The future withdraw-all submit uses the
    /// `u64::MAX` sentinel inside the action, but recording the
    /// observed amount lets a sign-time re-check detect a drift to
    /// zero (e.g. the user withdrew via another path between propose
    /// and approve).
    pub propose_time_deposited_collateral_raw: u64,

    /// The `ProposedAction::Withdraw` constructed by the chat tool. The
    /// future resume task uses this verbatim so the policy verdict is
    /// re-evaluated against the same logical action.
    pub action: ProposedAction,

    /// Wallclock timestamps mirror deposit's parked-intent shape. The
    /// `expires_at` field is the propose-time TTL the future
    /// approval-routing layer will enforce.
    pub proposed_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

/// Phase 6I-D in-memory park store. Cloneable (Arc-backed) so the
/// chat tool, a future resume task, and audit/inspection helpers can
/// share one source of truth.
#[derive(Clone, Default)]
pub struct SolendWithdrawAllParkStore {
    inner: Arc<Mutex<HashMap<Uuid, ParkedSolendWithdrawAllIntent>>>,
}

impl SolendWithdrawAllParkStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Park a new intent under `approval_request_id`. Returns
    /// `Err(())` if a parked entry already exists under that id (UUID
    /// collision is astronomically unlikely; the typed Err lets a
    /// future caller treat it as a programming error rather than
    /// silently overwriting).
    pub fn park(
        &self,
        approval_request_id: Uuid,
        intent: ParkedSolendWithdrawAllIntent,
    ) -> Result<(), ()> {
        let mut g = self.inner.lock().expect("withdraw park store mutex");
        if g.contains_key(&approval_request_id) {
            return Err(());
        }
        g.insert(approval_request_id, intent);
        Ok(())
    }

    /// Look up a parked intent by `approval_request_id`. Returns a
    /// clone (cheap — fixed-size fields; no transaction bytes).
    pub fn get(
        &self,
        approval_request_id: &Uuid,
    ) -> Option<ParkedSolendWithdrawAllIntent> {
        self.inner
            .lock()
            .expect("withdraw park store mutex")
            .get(approval_request_id)
            .cloned()
    }

    /// Drop a parked intent. Used by future resume / cleanup paths.
    /// Returns the removed intent if present.
    pub fn remove(
        &self,
        approval_request_id: &Uuid,
    ) -> Option<ParkedSolendWithdrawAllIntent> {
        self.inner
            .lock()
            .expect("withdraw park store mutex")
            .remove(approval_request_id)
    }

    /// Number of currently parked intents. Used by tests.
    pub fn parked_count(&self) -> usize {
        self.inner
            .lock()
            .expect("withdraw park store mutex")
            .len()
    }

    /// Is there an active parked intent for `(session_id, session_wallet)`?
    /// Used by the chat tool to refuse a duplicate proposal while a
    /// prior one is still in flight (mirroring deposit's Phase 5A
    /// pending-action guard).
    pub fn has_active_for_session_wallet(
        &self,
        session_id: &SessionId,
        session_wallet: &Pubkey,
    ) -> bool {
        let g = self.inner.lock().expect("withdraw park store mutex");
        g.values()
            .any(|i| &i.session_id == session_id && &i.session_wallet == session_wallet)
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lending::{CollateralTokenAmount, ProtocolTag};
    use std::str::FromStr;

    fn fixture_intent(
        approval_request_id: Uuid,
        wallet: Pubkey,
        obligation_pk: Pubkey,
    ) -> ParkedSolendWithdrawAllIntent {
        let lending_market =
            Pubkey::from_str("4UpD2fh7xH3VP9QQaXtsS1YY3bxzWhtfpks7FatyKvdY").unwrap();
        let reserve =
            Pubkey::from_str("BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw").unwrap();
        let mint =
            Pubkey::from_str("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v").unwrap();
        let _ = approval_request_id;
        ParkedSolendWithdrawAllIntent {
            intent_id: Uuid::new_v4(),
            session_id: SessionId::new(),
            session_wallet: wallet,
            obligation_pubkey: obligation_pk,
            lending_market,
            reserve_pubkey: reserve,
            reserve_mint: mint,
            propose_time_deposited_collateral_raw: 3_857_506,
            action: ProposedAction::Withdraw {
                protocol: ProtocolTag::Solend,
                reserve_mint: mint,
                collateral_amount: CollateralTokenAmount::new(u64::MAX),
            },
            proposed_at: Utc::now(),
            expires_at: Utc::now() + chrono::Duration::seconds(300),
        }
    }

    #[test]
    fn park_get_remove_roundtrip() {
        let store = SolendWithdrawAllParkStore::new();
        let id = Uuid::new_v4();
        let wallet = Pubkey::new_unique();
        let obl = Pubkey::new_unique();
        store
            .park(id, fixture_intent(id, wallet, obl))
            .expect("first park ok");
        assert_eq!(store.parked_count(), 1);
        let got = store.get(&id).expect("present");
        assert_eq!(got.session_wallet, wallet);
        assert_eq!(got.obligation_pubkey, obl);
        let removed = store.remove(&id).expect("remove returns");
        assert_eq!(removed.obligation_pubkey, obl);
        assert_eq!(store.parked_count(), 0);
    }

    #[test]
    fn park_explicit_obligation_pubkey_is_stored_verbatim_not_derived() {
        // Required test #11 for Phase 6I-D.
        // The explicit pubkey from the LLM/user MUST land in the
        // parked intent unchanged. Any future "derive obligation from
        // wallet+seed" logic would surface here as a mismatch.
        let store = SolendWithdrawAllParkStore::new();
        let id = Uuid::new_v4();
        let wallet =
            Pubkey::from_str("C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW").unwrap();
        let phase6h_obligation =
            Pubkey::from_str("HcKrv5Jo5f6qvzSGhJVYTNSqwKudRizn6fxbjPW7M8SV").unwrap();
        let intent = fixture_intent(id, wallet, phase6h_obligation);
        store.park(id, intent).expect("first park ok");
        let got = store.get(&id).expect("present");
        assert_eq!(
            got.obligation_pubkey, phase6h_obligation,
            "parked intent must store the explicit Phase 6H obligation pubkey"
        );
        assert_ne!(got.obligation_pubkey, wallet, "obligation must not be the wallet");
    }

    #[test]
    fn double_park_under_same_id_returns_err() {
        let store = SolendWithdrawAllParkStore::new();
        let id = Uuid::new_v4();
        let wallet = Pubkey::new_unique();
        store
            .park(id, fixture_intent(id, wallet, Pubkey::new_unique()))
            .expect("first park ok");
        let second = store.park(id, fixture_intent(id, wallet, Pubkey::new_unique()));
        assert!(second.is_err(), "duplicate park id must error");
        assert_eq!(store.parked_count(), 1);
    }

    #[test]
    fn has_active_for_session_wallet_matches_only_same_pair() {
        let store = SolendWithdrawAllParkStore::new();
        let id = Uuid::new_v4();
        let wallet = Pubkey::new_unique();
        let other_wallet = Pubkey::new_unique();
        let other_session = SessionId::new();
        let intent = fixture_intent(id, wallet, Pubkey::new_unique());
        let session = intent.session_id.clone();
        store.park(id, intent).unwrap();
        assert!(store.has_active_for_session_wallet(&session, &wallet));
        assert!(!store.has_active_for_session_wallet(&session, &other_wallet));
        assert!(!store.has_active_for_session_wallet(&other_session, &wallet));
    }
}
