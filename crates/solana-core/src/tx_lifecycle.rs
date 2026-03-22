//! Transaction lifecycle state machine — pure, deterministic core.
//!
//! This module defines the canonical state machine for tracking Solana
//! transactions from submission through finality (or terminal failure).
//!
//! # Design invariants
//!
//! 1. **Monotonic progress** — state transitions only move forward in the
//!    precedence order. The system never regresses to a less-certain state.
//!
//! 2. **Terminal states are final** — `Finalized`, `Failed`, and `Expired`
//!    are permanent once reached. The single exception: `Expired` may be
//!    overridden by a later `Confirmed`/`Finalized` observation (defensive
//!    correction for RPC lag).
//!
//! 3. **Deterministic expiry** — a transaction expires ONLY when the network
//!    block height exceeds `lastValidBlockHeight`. Wall-clock timeouts are
//!    not used.
//!
//! 4. **No side effects** — all functions in this module are pure. No RPC
//!    calls, no async, no database access, no event emission. This is the
//!    core transition engine that all I/O layers depend on.
//!
//! # Precedence order (highest to lowest)
//!
//! ```text
//! Finalized > Failed > Confirmed > Dropped > Submitted
//! ```
//!
//! `Expired` is terminal but ranked below `Confirmed` — a transaction
//! observed on-chain always overrides a local expiry classification.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ── TxState ────────────────────────────────────────────────────────────────

/// The lifecycle state of a submitted Solana transaction.
///
/// States are ordered by confidence / finality:
///
/// - `Submitted` — RPC accepted the transaction; network outcome unknown.
/// - `Dropped` — not observed on-chain after a grace period; blockhash still valid.
///   **Provisional** — may recover if the transaction appears later.
/// - `Confirmed` — observed on-chain with supermajority vote. May still be
///   rolled back in an (extremely rare) fork.
/// - `Finalized` — the containing block is rooted. **Terminal success.**
/// - `Failed` — landed on-chain but execution failed (non-null `err`).
///   **Terminal failure.** Fees were consumed.
/// - `Expired` — blockhash became invalid before the transaction was observed.
///   **Terminal failure.** The transaction can never land.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TxState {
    /// RPC `sendTransaction` returned successfully. The transaction is in
    /// the RPC node's processing pipeline but has not been observed on-chain.
    Submitted,

    /// The transaction was not found by `getSignatureStatuses` after a grace
    /// period (>= 50 blocks since submission), but the blockhash is still valid.
    ///
    /// This is a **provisional** state — the transaction may still appear.
    /// Dropped is NOT terminal.
    Dropped,

    /// `getSignatureStatuses` returned `confirmationStatus: "confirmed"` with
    /// `err: null`. The transaction is in a block voted on by a supermajority.
    Confirmed {
        /// The slot in which the transaction was included.
        slot: u64,
    },

    /// `getSignatureStatuses` returned `confirmationStatus: "finalized"`.
    /// The containing block is rooted — this is irreversible.
    ///
    /// **Terminal: success.**
    Finalized {
        /// The slot in which the transaction was finalized.
        slot: u64,
    },

    /// The transaction landed on-chain but execution failed. The `err` field
    /// from `getSignatureStatuses` was non-null.
    ///
    /// **Terminal: failure.** Fees and compute were consumed.
    Failed {
        /// The structured error string from the Solana runtime.
        err: String,
    },

    /// The network block height exceeded `lastValidBlockHeight` while the
    /// transaction was still unobserved. The blockhash is permanently invalid;
    /// the transaction can never land.
    ///
    /// **Terminal: failure.**
    ///
    /// # Override exception
    ///
    /// If a later RPC observation shows the transaction as `Confirmed` or
    /// `Finalized`, the `Expired` state may be overridden. This handles the
    /// edge case where RPC block-height reporting lagged behind actual
    /// inclusion.
    Expired,
}

impl TxState {
    /// Returns the numeric precedence rank of this state.
    ///
    /// Higher values represent more-certain / more-terminal states.
    /// Used to enforce monotonic progress.
    ///
    /// Precedence: `Submitted(0) < Dropped(1) < Confirmed(2) < Failed(3) < Finalized(4)`
    ///
    /// `Expired` is ranked at 1 (same as Dropped) because it can be
    /// overridden by a positive on-chain observation. Its terminality is
    /// enforced by the transition function, not by precedence alone.
    fn precedence(&self) -> u8 {
        match self {
            TxState::Submitted => 0,
            TxState::Dropped => 1,
            TxState::Expired => 1,
            TxState::Confirmed { .. } => 2,
            TxState::Failed { .. } => 3,
            TxState::Finalized { .. } => 4,
        }
    }

    /// Returns `true` if this state is a hard terminal — no further
    /// transitions are allowed (except the Expired override).
    pub fn is_terminal(&self) -> bool {
        matches!(self, TxState::Finalized { .. } | TxState::Failed { .. } | TxState::Expired)
    }

    /// Returns `true` if this state represents a successful outcome.
    pub fn is_success(&self) -> bool {
        matches!(self, TxState::Finalized { .. })
    }

    /// Returns `true` if this state represents a failure outcome.
    pub fn is_failure(&self) -> bool {
        matches!(self, TxState::Failed { .. } | TxState::Expired)
    }

    /// Returns the slot if the state carries one.
    pub fn slot(&self) -> Option<u64> {
        match self {
            TxState::Confirmed { slot } | TxState::Finalized { slot } => Some(*slot),
            _ => None,
        }
    }
}

impl std::fmt::Display for TxState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TxState::Submitted => write!(f, "submitted"),
            TxState::Dropped => write!(f, "dropped"),
            TxState::Confirmed { slot } => write!(f, "confirmed(slot={slot})"),
            TxState::Finalized { slot } => write!(f, "finalized(slot={slot})"),
            TxState::Failed { err } => write!(f, "failed({err})"),
            TxState::Expired => write!(f, "expired"),
        }
    }
}

// ── RpcObservation ─────────────────────────────────────────────────────────

/// A snapshot of what an RPC node reports about a transaction signature.
///
/// This is the input to the state machine. It is constructed from
/// `getSignatureStatuses` responses or WebSocket notifications.
///
/// # Fields
///
/// - `status`: the `confirmationStatus` string from Solana RPC.
///   Valid values: `"processed"`, `"confirmed"`, `"finalized"`.
///   `None` means the RPC node does not know about this transaction.
///
/// - `err`: the transaction execution error, if any. Non-null means the
///   transaction landed but its instructions failed.
///
/// - `slot`: the slot in which the transaction was included.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcObservation {
    /// Confirmation status: `"processed"`, `"confirmed"`, `"finalized"`, or `None`.
    pub status: Option<String>,
    /// On-chain execution error. Non-null means the transaction failed.
    pub err: Option<String>,
    /// The slot in which the transaction was included.
    pub slot: Option<u64>,
}

impl RpcObservation {
    /// Returns `true` if the RPC node has no information about this transaction.
    pub fn is_null(&self) -> bool {
        self.status.is_none()
    }

    /// Returns `true` if an on-chain execution error was reported.
    pub fn has_error(&self) -> bool {
        self.err.is_some()
    }
}

// ── TrackingRecord ─────────────────────────────────────────────────────────

/// The durable tracking record for a submitted transaction.
///
/// This struct mirrors the `transaction_tracking` SQLite table. It carries
/// both the current lifecycle state and idempotency flags to prevent
/// duplicate event emission.
///
/// # Event idempotency
///
/// Each `*_emitted` flag is set to `true` after the corresponding event has
/// been emitted AND the tracking record has been persisted. On crash recovery,
/// if a flag is `false`, the event may be re-emitted (at-least-once). This is
/// acceptable because consumers can deduplicate on `(transaction_id, event_type)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackingRecord {
    /// The on-chain transaction signature (base58).
    pub signature: String,

    /// The internal transaction ID (from `TransactionProposal.id`).
    pub transaction_id: Uuid,

    /// The orchestrator request ID.
    pub request_id: Uuid,

    /// The session that originated this transaction.
    pub session_id: String,

    /// The wallet pubkey that signed.
    pub wallet_pubkey: String,

    /// Current lifecycle state.
    pub state: TxState,

    /// The last block height at which this transaction's blockhash is valid.
    /// From `getLatestBlockhash().lastValidBlockHeight`.
    pub last_valid_block_height: u64,

    /// The network block height at the time of submission.
    /// Used to calculate grace periods.
    pub submission_block_height: u64,

    /// The slot in which the transaction was included (if observed).
    pub slot: Option<u64>,

    /// The on-chain error string (if the transaction failed).
    pub err: Option<String>,

    // ── Event idempotency flags ──────────────────────────────────────

    /// Whether `TransactionConfirmed` has been emitted.
    pub confirmed_emitted: bool,
    /// Whether `TransactionFinalized` has been emitted.
    pub finalized_emitted: bool,
    /// Whether `TransactionFailed` has been emitted.
    pub failed_emitted: bool,
    /// Whether `TransactionDropped` has been emitted.
    pub dropped_emitted: bool,
    /// Whether `TransactionExpired` has been emitted.
    pub expired_emitted: bool,
}

impl TrackingRecord {
    /// Creates a new tracking record in the `Submitted` state.
    ///
    /// All event idempotency flags start as `false`.
    pub fn new(
        signature: String,
        transaction_id: Uuid,
        request_id: Uuid,
        session_id: String,
        wallet_pubkey: String,
        last_valid_block_height: u64,
        submission_block_height: u64,
    ) -> Self {
        Self {
            signature,
            transaction_id,
            request_id,
            session_id,
            wallet_pubkey,
            state: TxState::Submitted,
            last_valid_block_height,
            submission_block_height,
            slot: None,
            err: None,
            confirmed_emitted: false,
            finalized_emitted: false,
            failed_emitted: false,
            dropped_emitted: false,
            expired_emitted: false,
        }
    }

    /// Returns `true` if the transaction is in a terminal state.
    pub fn is_terminal(&self) -> bool {
        self.state.is_terminal()
    }
}

// ── Constants ──────────────────────────────────────────────────────────────

/// Number of blocks after submission before a null-status transaction is
/// classified as `Dropped`. (~20 seconds at normal Solana slot pace.)
const DROPPED_GRACE_BLOCKS: u64 = 50;

/// Number of blocks after `lastValidBlockHeight` to continue tracking
/// before closing the record entirely. Allows for RPC lag.
const EXPIRY_TRACKING_BUFFER: u64 = 32;

// ── Transition function ────────────────────────────────────────────────────

/// Calculates the next state for a transaction given an RPC observation
/// and the current network block height.
///
/// This is the **single source of truth** for all state transitions.
/// It is pure, deterministic, and free of side effects.
///
/// # Arguments
///
/// - `current` — the transaction's current lifecycle state.
/// - `observation` — what the RPC node reported (may be null/absent).
/// - `current_block_height` — the latest finalized block height from the network.
/// - `submission_height` — the block height at the time of submission.
/// - `last_valid_height` — the `lastValidBlockHeight` from the blockhash.
///
/// # Invariants
///
/// 1. State transitions are monotonic (never regress).
/// 2. `Finalized` and `Failed` are absolutely terminal.
/// 3. `Expired` is terminal unless overridden by a positive on-chain observation.
/// 4. `Dropped` is provisional (not terminal).
/// 5. `"processed"` status does not upgrade state.
/// 6. A null observation from a lagging RPC node does not regress state.
///
/// # Returns
///
/// The new state. If no transition is warranted, returns a clone of `current`.
pub fn calculate_next_state(
    current: &TxState,
    observation: &RpcObservation,
    current_block_height: u64,
    submission_height: u64,
    last_valid_height: u64,
) -> TxState {
    // ── Hard terminal: Finalized and Failed never change ────────────
    match current {
        TxState::Finalized { .. } => return current.clone(),
        TxState::Failed { .. } => return current.clone(),
        _ => {}
    }

    // ── Observation has an on-chain error → Failed ──────────────────
    //
    // An on-chain error takes priority over everything except Finalized
    // (which is already handled above). This covers the case where
    // a transaction is confirmed/finalized WITH an error.
    if let Some(ref err) = observation.err {
        return TxState::Failed { err: err.clone() };
    }

    // ── Observation has a positive status ───────────────────────────
    if let Some(ref status) = observation.status {
        match status.as_str() {
            "finalized" => {
                // Finalized overrides everything (except Failed, already handled).
                let slot = observation.slot.unwrap_or(0);
                return TxState::Finalized { slot };
            }

            "confirmed" => {
                // Confirmed upgrades Submitted, Dropped, and Expired (override).
                // It does NOT downgrade Finalized or Failed (already handled).
                if current.precedence() < (TxState::Confirmed { slot: 0 }).precedence()
                    || matches!(current, TxState::Expired)
                {
                    let slot = observation.slot.unwrap_or(0);
                    return TxState::Confirmed { slot };
                }
                // Already Confirmed or higher — preserve current.
                return current.clone();
            }

            "processed" => {
                // "processed" is not a stable commitment level. It means
                // the transaction was processed by a single validator but
                // has not been voted on by the cluster. We treat this as
                // informational only — it does NOT upgrade state.
                //
                // Rationale: a "processed" observation can disappear on
                // the next poll if the block is not confirmed.
                return current.clone();
            }

            // Unknown status string — defensively preserve current state.
            _ => return current.clone(),
        }
    }

    // ── Observation is null (RPC does not know the transaction) ─────
    //
    // A null observation means the queried RPC node has no record of
    // this signature. This does NOT mean the transaction is dead — it
    // may be in a different node's mempool, or the node may be lagging.

    // Rule: never regress from a positive on-chain state on a null observation.
    if matches!(current, TxState::Confirmed { .. }) {
        return current.clone();
    }

    // Check deterministic expiry: block height exceeded lastValidBlockHeight.
    if current_block_height > last_valid_height {
        // If already Expired, stay Expired.
        if matches!(current, TxState::Expired) {
            return current.clone();
        }
        return TxState::Expired;
    }

    // Check provisional drop: null status for long enough, blockhash still valid.
    let blocks_elapsed = current_block_height.saturating_sub(submission_height);
    if blocks_elapsed >= DROPPED_GRACE_BLOCKS {
        // Upgrade to Dropped if currently Submitted. If already Dropped, stay.
        if matches!(current, TxState::Submitted) {
            return TxState::Dropped;
        }
    }

    // No transition warranted — stay in current state.
    current.clone()
}

/// Returns `true` if a tracking record should be kept active (not yet
/// closed). A record is active if:
///
/// - It is not in a terminal state, OR
/// - It is Expired but within the tracking buffer (allows for late RPC observations)
///
/// Once `current_block_height > last_valid_height + EXPIRY_TRACKING_BUFFER`,
/// the record can be closed regardless of state.
pub fn should_continue_tracking(
    record: &TrackingRecord,
    current_block_height: u64,
) -> bool {
    match &record.state {
        // Terminal success/failure — stop tracking.
        TxState::Finalized { .. } | TxState::Failed { .. } => false,

        // Expired — keep tracking for a buffer period to catch late arrivals.
        TxState::Expired => {
            current_block_height <= record.last_valid_block_height + EXPIRY_TRACKING_BUFFER
        }

        // Active states — always continue.
        TxState::Submitted | TxState::Dropped | TxState::Confirmed { .. } => true,
    }
}

/// Determines which events need to be emitted based on the current state
/// and the idempotency flags. Returns a list of event kinds that should
/// be emitted (the caller is responsible for actual emission).
///
/// This function is pure — it reads the record but does not mutate it.
/// The caller must set the corresponding `*_emitted` flag after emission.
pub fn pending_events(record: &TrackingRecord) -> Vec<LifecycleEventKind> {
    let mut events = Vec::new();

    match &record.state {
        TxState::Confirmed { slot } => {
            if !record.confirmed_emitted {
                events.push(LifecycleEventKind::Confirmed { slot: *slot });
            }
        }
        TxState::Finalized { slot } => {
            // Finalized implies Confirmed — emit Confirmed first if missed.
            if !record.confirmed_emitted {
                events.push(LifecycleEventKind::Confirmed { slot: *slot });
            }
            if !record.finalized_emitted {
                events.push(LifecycleEventKind::Finalized { slot: *slot });
            }
        }
        TxState::Failed { ref err } => {
            if !record.failed_emitted {
                events.push(LifecycleEventKind::Failed { err: err.clone() });
            }
        }
        TxState::Dropped => {
            if !record.dropped_emitted {
                events.push(LifecycleEventKind::Dropped);
            }
        }
        TxState::Expired => {
            if !record.expired_emitted {
                events.push(LifecycleEventKind::Expired);
            }
        }
        TxState::Submitted => {
            // No event for Submitted — it was emitted at submission time.
        }
    }

    events
}

/// The kind of lifecycle event to emit.
///
/// This is a simplified enum for the transition engine. The actual event
/// structs (with headers, session IDs, etc.) are constructed by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleEventKind {
    /// Transaction reached `confirmed` commitment.
    Confirmed { slot: u64 },
    /// Transaction reached `finalized` commitment.
    Finalized { slot: u64 },
    /// Transaction execution failed on-chain.
    Failed { err: String },
    /// Transaction was not observed after grace period (provisional).
    Dropped,
    /// Transaction's blockhash expired — permanently dead.
    Expired,
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Helper: create a null observation (RPC doesn't know the tx).
    fn null_obs() -> RpcObservation {
        RpcObservation { status: None, err: None, slot: None }
    }

    // Helper: create a confirmed observation.
    fn confirmed_obs(slot: u64) -> RpcObservation {
        RpcObservation {
            status: Some("confirmed".to_string()),
            err: None,
            slot: Some(slot),
        }
    }

    // Helper: create a finalized observation.
    fn finalized_obs(slot: u64) -> RpcObservation {
        RpcObservation {
            status: Some("finalized".to_string()),
            err: None,
            slot: Some(slot),
        }
    }

    // Helper: create a processed observation.
    fn processed_obs(slot: u64) -> RpcObservation {
        RpcObservation {
            status: Some("processed".to_string()),
            err: None,
            slot: Some(slot),
        }
    }

    // Helper: create a failed observation.
    fn failed_obs(err: &str) -> RpcObservation {
        RpcObservation {
            status: Some("confirmed".to_string()),
            err: Some(err.to_string()),
            slot: Some(100),
        }
    }

    // Standard heights: submitted at 1000, blockhash valid until 1150.
    const SUB_HEIGHT: u64 = 1000;
    const LAST_VALID: u64 = 1150;

    // ── Monotonicity tests ─────────────────────────────────────────

    #[test]
    fn finalized_never_changes() {
        let current = TxState::Finalized { slot: 100 };

        // Null observation.
        assert_eq!(
            calculate_next_state(&current, &null_obs(), 1200, SUB_HEIGHT, LAST_VALID),
            current,
        );
        // Failed observation.
        assert_eq!(
            calculate_next_state(&current, &failed_obs("err"), 1100, SUB_HEIGHT, LAST_VALID),
            current,
        );
        // Confirmed observation (lower precedence).
        assert_eq!(
            calculate_next_state(&current, &confirmed_obs(200), 1100, SUB_HEIGHT, LAST_VALID),
            current,
        );
    }

    #[test]
    fn failed_never_changes() {
        let current = TxState::Failed { err: "InstructionError".to_string() };

        assert_eq!(
            calculate_next_state(&current, &null_obs(), 1200, SUB_HEIGHT, LAST_VALID),
            current,
        );
        assert_eq!(
            calculate_next_state(&current, &confirmed_obs(200), 1100, SUB_HEIGHT, LAST_VALID),
            current,
        );
        assert_eq!(
            calculate_next_state(&current, &finalized_obs(200), 1100, SUB_HEIGHT, LAST_VALID),
            current,
        );
    }

    #[test]
    fn confirmed_does_not_regress_on_null() {
        let current = TxState::Confirmed { slot: 100 };

        let next = calculate_next_state(&current, &null_obs(), 1100, SUB_HEIGHT, LAST_VALID);
        assert_eq!(next, current, "null must not regress Confirmed");
    }

    #[test]
    fn confirmed_does_not_regress_even_past_expiry() {
        let current = TxState::Confirmed { slot: 100 };

        // Block height past lastValidBlockHeight — but we're already confirmed!
        let next = calculate_next_state(&current, &null_obs(), 1200, SUB_HEIGHT, LAST_VALID);
        assert_eq!(next, current, "Confirmed must not expire");
    }

    // ── Happy path tests ───────────────────────────────────────────

    #[test]
    fn submitted_to_confirmed() {
        let next = calculate_next_state(
            &TxState::Submitted,
            &confirmed_obs(105),
            1010, SUB_HEIGHT, LAST_VALID,
        );
        assert_eq!(next, TxState::Confirmed { slot: 105 });
    }

    #[test]
    fn confirmed_to_finalized() {
        let next = calculate_next_state(
            &TxState::Confirmed { slot: 105 },
            &finalized_obs(105),
            1020, SUB_HEIGHT, LAST_VALID,
        );
        assert_eq!(next, TxState::Finalized { slot: 105 });
    }

    #[test]
    fn submitted_to_finalized_directly() {
        // Transaction may jump directly to finalized if we poll infrequently.
        let next = calculate_next_state(
            &TxState::Submitted,
            &finalized_obs(105),
            1020, SUB_HEIGHT, LAST_VALID,
        );
        assert_eq!(next, TxState::Finalized { slot: 105 });
    }

    // ── Failure tests ──────────────────────────────────────────────

    #[test]
    fn submitted_to_failed_on_error() {
        let next = calculate_next_state(
            &TxState::Submitted,
            &failed_obs("InstructionError"),
            1010, SUB_HEIGHT, LAST_VALID,
        );
        assert_eq!(next, TxState::Failed { err: "InstructionError".to_string() });
    }

    #[test]
    fn confirmed_to_failed_on_error() {
        let next = calculate_next_state(
            &TxState::Confirmed { slot: 100 },
            &failed_obs("InstructionError"),
            1020, SUB_HEIGHT, LAST_VALID,
        );
        assert_eq!(next, TxState::Failed { err: "InstructionError".to_string() });
    }

    #[test]
    fn dropped_to_failed_on_error() {
        let next = calculate_next_state(
            &TxState::Dropped,
            &failed_obs("InstructionError"),
            1060, SUB_HEIGHT, LAST_VALID,
        );
        assert_eq!(next, TxState::Failed { err: "InstructionError".to_string() });
    }

    // ── Dropped tests ──────────────────────────────────────────────

    #[test]
    fn submitted_stays_submitted_before_grace_period() {
        // Only 30 blocks elapsed — not enough for Dropped.
        let next = calculate_next_state(
            &TxState::Submitted,
            &null_obs(),
            SUB_HEIGHT + 30, SUB_HEIGHT, LAST_VALID,
        );
        assert_eq!(next, TxState::Submitted);
    }

    #[test]
    fn submitted_to_dropped_after_grace_period() {
        // 50 blocks elapsed, blockhash still valid.
        let next = calculate_next_state(
            &TxState::Submitted,
            &null_obs(),
            SUB_HEIGHT + 50, SUB_HEIGHT, LAST_VALID,
        );
        assert_eq!(next, TxState::Dropped);
    }

    #[test]
    fn dropped_stays_dropped_on_null() {
        let next = calculate_next_state(
            &TxState::Dropped,
            &null_obs(),
            SUB_HEIGHT + 80, SUB_HEIGHT, LAST_VALID,
        );
        assert_eq!(next, TxState::Dropped);
    }

    #[test]
    fn dropped_recovers_to_confirmed() {
        let next = calculate_next_state(
            &TxState::Dropped,
            &confirmed_obs(1060),
            SUB_HEIGHT + 80, SUB_HEIGHT, LAST_VALID,
        );
        assert_eq!(next, TxState::Confirmed { slot: 1060 });
    }

    // ── Expiry tests ───────────────────────────────────────────────

    #[test]
    fn submitted_to_expired_when_block_height_exceeded() {
        let next = calculate_next_state(
            &TxState::Submitted,
            &null_obs(),
            LAST_VALID + 1, SUB_HEIGHT, LAST_VALID,
        );
        assert_eq!(next, TxState::Expired);
    }

    #[test]
    fn dropped_to_expired_when_block_height_exceeded() {
        let next = calculate_next_state(
            &TxState::Dropped,
            &null_obs(),
            LAST_VALID + 1, SUB_HEIGHT, LAST_VALID,
        );
        assert_eq!(next, TxState::Expired);
    }

    #[test]
    fn expired_stays_expired_on_null() {
        let next = calculate_next_state(
            &TxState::Expired,
            &null_obs(),
            LAST_VALID + 10, SUB_HEIGHT, LAST_VALID,
        );
        assert_eq!(next, TxState::Expired);
    }

    // ── Expired override (defensive correction) ────────────────────

    #[test]
    fn expired_overridden_by_confirmed() {
        // Edge case: RPC block height was lagging when we classified as Expired,
        // but the transaction actually did land.
        let next = calculate_next_state(
            &TxState::Expired,
            &confirmed_obs(1140),
            LAST_VALID + 5, SUB_HEIGHT, LAST_VALID,
        );
        assert_eq!(next, TxState::Confirmed { slot: 1140 });
    }

    #[test]
    fn expired_overridden_by_finalized() {
        let next = calculate_next_state(
            &TxState::Expired,
            &finalized_obs(1140),
            LAST_VALID + 5, SUB_HEIGHT, LAST_VALID,
        );
        assert_eq!(next, TxState::Finalized { slot: 1140 });
    }

    // ── "processed" does not upgrade ───────────────────────────────

    #[test]
    fn processed_does_not_upgrade_submitted() {
        let next = calculate_next_state(
            &TxState::Submitted,
            &processed_obs(1005),
            1010, SUB_HEIGHT, LAST_VALID,
        );
        assert_eq!(next, TxState::Submitted);
    }

    #[test]
    fn processed_does_not_upgrade_dropped() {
        let next = calculate_next_state(
            &TxState::Dropped,
            &processed_obs(1070),
            SUB_HEIGHT + 70, SUB_HEIGHT, LAST_VALID,
        );
        assert_eq!(next, TxState::Dropped);
    }

    // ── Tracking continuation ──────────────────────────────────────

    #[test]
    fn tracking_continues_for_submitted() {
        let record = TrackingRecord::new(
            "sig".into(), Uuid::new_v4(), Uuid::new_v4(),
            "session".into(), "wallet".into(), LAST_VALID, SUB_HEIGHT,
        );
        assert!(should_continue_tracking(&record, 1050));
    }

    #[test]
    fn tracking_stops_for_finalized() {
        let mut record = TrackingRecord::new(
            "sig".into(), Uuid::new_v4(), Uuid::new_v4(),
            "session".into(), "wallet".into(), LAST_VALID, SUB_HEIGHT,
        );
        record.state = TxState::Finalized { slot: 100 };
        assert!(!should_continue_tracking(&record, 1050));
    }

    #[test]
    fn tracking_stops_for_failed() {
        let mut record = TrackingRecord::new(
            "sig".into(), Uuid::new_v4(), Uuid::new_v4(),
            "session".into(), "wallet".into(), LAST_VALID, SUB_HEIGHT,
        );
        record.state = TxState::Failed { err: "err".into() };
        assert!(!should_continue_tracking(&record, 1050));
    }

    #[test]
    fn tracking_continues_for_expired_within_buffer() {
        let mut record = TrackingRecord::new(
            "sig".into(), Uuid::new_v4(), Uuid::new_v4(),
            "session".into(), "wallet".into(), LAST_VALID, SUB_HEIGHT,
        );
        record.state = TxState::Expired;
        // Within buffer: LAST_VALID + 32 = 1182. current=1160 < 1182.
        assert!(should_continue_tracking(&record, LAST_VALID + 10));
    }

    #[test]
    fn tracking_stops_for_expired_past_buffer() {
        let mut record = TrackingRecord::new(
            "sig".into(), Uuid::new_v4(), Uuid::new_v4(),
            "session".into(), "wallet".into(), LAST_VALID, SUB_HEIGHT,
        );
        record.state = TxState::Expired;
        // Past buffer: LAST_VALID + 32 = 1182. current=1183 > 1182.
        assert!(!should_continue_tracking(&record, LAST_VALID + 33));
    }

    // ── Event idempotency ──────────────────────────────────────────

    #[test]
    fn pending_events_for_confirmed() {
        let mut record = TrackingRecord::new(
            "sig".into(), Uuid::new_v4(), Uuid::new_v4(),
            "session".into(), "wallet".into(), LAST_VALID, SUB_HEIGHT,
        );
        record.state = TxState::Confirmed { slot: 100 };

        let events = pending_events(&record);
        assert_eq!(events, vec![LifecycleEventKind::Confirmed { slot: 100 }]);

        // After marking emitted, no more events.
        record.confirmed_emitted = true;
        let events = pending_events(&record);
        assert!(events.is_empty());
    }

    #[test]
    fn pending_events_for_finalized_emits_confirmed_first() {
        let mut record = TrackingRecord::new(
            "sig".into(), Uuid::new_v4(), Uuid::new_v4(),
            "session".into(), "wallet".into(), LAST_VALID, SUB_HEIGHT,
        );
        record.state = TxState::Finalized { slot: 100 };

        // Neither confirmed nor finalized emitted yet.
        let events = pending_events(&record);
        assert_eq!(events, vec![
            LifecycleEventKind::Confirmed { slot: 100 },
            LifecycleEventKind::Finalized { slot: 100 },
        ]);

        // After confirmed emitted, only finalized remains.
        record.confirmed_emitted = true;
        let events = pending_events(&record);
        assert_eq!(events, vec![LifecycleEventKind::Finalized { slot: 100 }]);
    }

    #[test]
    fn pending_events_for_submitted_is_empty() {
        let record = TrackingRecord::new(
            "sig".into(), Uuid::new_v4(), Uuid::new_v4(),
            "session".into(), "wallet".into(), LAST_VALID, SUB_HEIGHT,
        );
        let events = pending_events(&record);
        assert!(events.is_empty());
    }

    // ── Edge case: unknown status string ───────────────────────────

    #[test]
    fn unknown_status_preserves_current() {
        let obs = RpcObservation {
            status: Some("something_new".to_string()),
            err: None,
            slot: Some(100),
        };
        let next = calculate_next_state(&TxState::Submitted, &obs, 1010, SUB_HEIGHT, LAST_VALID);
        assert_eq!(next, TxState::Submitted);
    }

    // ── Edge case: err with null status ─────────────────────────────

    #[test]
    fn err_with_null_status_still_fails() {
        let obs = RpcObservation {
            status: None,
            err: Some("AccountNotFound".to_string()),
            slot: None,
        };
        let next = calculate_next_state(&TxState::Submitted, &obs, 1010, SUB_HEIGHT, LAST_VALID);
        assert_eq!(next, TxState::Failed { err: "AccountNotFound".to_string() });
    }

    // ── Edge case: exact boundary ──────────────────────────────────

    #[test]
    fn at_exact_last_valid_height_does_not_expire() {
        // current_block_height == last_valid_height — NOT expired (must be >).
        let next = calculate_next_state(
            &TxState::Submitted,
            &null_obs(),
            LAST_VALID, SUB_HEIGHT, LAST_VALID,
        );
        // Should be Dropped (past grace period) but not Expired.
        assert_eq!(next, TxState::Dropped);
    }

    #[test]
    fn one_past_last_valid_height_expires() {
        let next = calculate_next_state(
            &TxState::Submitted,
            &null_obs(),
            LAST_VALID + 1, SUB_HEIGHT, LAST_VALID,
        );
        assert_eq!(next, TxState::Expired);
    }
}
