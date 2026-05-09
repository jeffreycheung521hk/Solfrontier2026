use solana_program::program_error::ProgramError;
use thiserror::Error;

/// Custom program errors. Each variant is encoded into
/// [`ProgramError::Custom`] using the discriminant, so the numeric
/// position of every variant is part of the on-chain wire shape and
/// MUST be append-only.
#[derive(Error, Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum AuthorityError {
    #[error("user (account[0]) must sign the transaction")]
    UserMustSign = 0,

    #[error("authorization PDA account does not match the canonical PDA derivation")]
    InvalidPda = 1,

    #[error("authorization expired: current slot >= expires_at_slot")]
    AuthorizationExpired = 2,

    #[error("max_input_amount_raw must be > 0")]
    InvalidZeroMaxAmount = 3,

    #[error("schema_version does not match the Stage 2 schema this program was built against")]
    UnsupportedSchemaVersion = 4,

    #[error("allowed_action_type is not a recognized Stage 2 discriminator (1=SolendWithdrawAllDelegated, 2=JupiterBuySolWithUsdc)")]
    InvalidActionType = 5,

    #[error("revoke signer does not match the user recorded in the authorization PDA")]
    RevokeWrongUser = 6,

    #[error("close signer does not match the user recorded in the authorization PDA")]
    CloseWrongUser = 7,

    #[error("authorization is not yet eligible to be closed (must be revoked, completed, or expired)")]
    NotCloseable = 8,

    // ── Stage 2 P2 ExecuteAction boundary ───────────────────────────────
    //
    // Discriminants 9..=20 are appended for the ExecuteAction processor.
    // Numeric values are part of the on-chain wire shape (encoded into
    // `ProgramError::Custom`) and MUST stay append-only — never reorder
    // or renumber.

    #[error("execute_action requires the executor account to sign the transaction")]
    MissingExecutorSignature = 9,

    #[error("authorization PDA derived from the provided seeds does not match the supplied account key")]
    InvalidAuthorizationPda = 10,

    #[error("authorization PDA owner is not this program")]
    AuthorizationOwnerMismatch = 11,

    #[error("authorization PDA's recorded user does not match the user account passed to execute_action")]
    AuthorizationUserMismatch = 12,

    #[error("execute_action signer does not match the executor recorded in the authorization PDA")]
    ExecutorMismatch = 13,

    #[error("canonical_rule_hash arg does not match the hash recorded in the authorization PDA")]
    RuleHashMismatch = 14,

    #[error("action_type arg does not match the allowed_action_type recorded in the authorization PDA")]
    ActionTypeMismatch = 15,

    #[error("authorization has already been revoked")]
    AuthorizationRevoked = 16,

    #[error("authorization has already been completed (one-shot v1 — second execution rejected)")]
    AuthorizationCompleted = 17,

    #[error("input_amount_raw must be > 0")]
    InputAmountZero = 18,

    #[error("input_amount_raw + used_amount_raw exceeds max_input_amount_raw")]
    InputAmountExceeded = 19,

    #[error("execution_nonce must equal record.execution_nonce + 1")]
    ExecutionNonceMismatch = 20,

    #[error("execute_action rejected: same-slot replay (current_slot == record.last_execution_slot)")]
    SameSlotReplay = 21,

    // ── Stage 2 P3 condition gate ───────────────────────────────────────
    //
    // Discriminants 22..=31 are appended for the condition-verifier
    // wiring inside ExecuteAction. Numeric values are part of the
    // on-chain wire shape (encoded into `ProgramError::Custom`) and
    // MUST stay append-only — never reorder or renumber. A specific
    // variant per condition_verifier failure mode lets daemon logs
    // attribute fail-closed events to the precise gate that tripped,
    // which we want for live mainnet forensics.

    #[error("execute_action requires a non-empty condition proof payload")]
    MissingConditionProof = 22,

    #[error("condition proof carries 0 conditions or exceeds MAX_PROOF_CONDITIONS")]
    ConditionCountMismatch = 23,

    #[error("condition_logic fold over the proof results returned false")]
    ConditionNotMet = 24,

    #[error("condition verifier rejected the proof (catch-all for unmapped VerifierError variants)")]
    ConditionVerificationFailed = 25,

    #[error("Pyth snapshot is older than condition.max_age_seconds (or future-dated)")]
    PythSnapshotStale = 26,

    #[error("Pyth snapshot feed_id does not match the condition's feed_id")]
    PythFeedIdMismatch = 27,

    #[error("Pyth snapshot verification_level is below the required Full level (audit U-5)")]
    PythVerificationLevelInsufficient = 28,

    #[error("Pyth snapshot confidence/price ratio exceeds condition.max_confidence_bps")]
    PythConfidenceTooWide = 29,

    #[error("Solend condition.formula_version is not supported by this program build")]
    SolendFormulaVersionUnsupported = 30,

    #[error("Solend reserve snapshot is older than condition.max_reserve_staleness_slots, or stale_flag is set")]
    SolendReserveStale = 31,

    // ── Stage 2 Solend Boundary Errors (P4 — append-only at codes 32+) ──

    #[error("execute_action for SolendWithdrawAllDelegated requires a SolendBoundaryProof payload")]
    SolendBoundaryProofMissing = 32,

    /// Reserved for future Solend action types this build does not
    /// know how to verify. Not currently triggered by any code path —
    /// the only Stage 2 Solend action in v1 is
    /// `SolendWithdrawAllDelegated`, which the boundary verifier
    /// fully covers.
    #[error("Solend action type is not supported by this build of the boundary verifier")]
    UnsupportedSolendAction = 33,

    #[error("supplied solend_program_id does not equal SOLEND_PROGRAM_ID_MAINNET")]
    SolendProgramMismatch = 34,

    #[error("obligation account-level owner is not the Solend program")]
    SolendObligationOwnerMismatch = 35,

    #[error("Obligation.owner field does not equal AuthorizationRecord.delegated_wallet")]
    SolendObligationAuthorityMismatch = 36,

    #[error("Obligation.owner field equals AuthorizationRecord.user (main wallet) — Stage 2 hard-rejects main-wallet obligations")]
    SolendMainWalletObligationRejected = 37,

    #[error("sibling Withdraw targets our obligation but a different reserve")]
    SolendReserveMismatch = 38,

    #[error("supplied lending_market does not match expected (obligation/proof/sibling cross-check)")]
    SolendLendingMarketMismatch = 39,

    #[error("supplied destination does not match expected (AuthorizationRecord.destination or sibling cross-check)")]
    SolendDestinationMismatch = 40,

    #[error("required Solend RefreshReserve sibling instruction targeting expected reserve is missing")]
    SolendRefreshMissing = 41,

    #[error("required Solend WithdrawObligationCollateralAndRedeemReserveCollateral sibling instruction targeting expected obligation is missing")]
    SolendWithdrawMissing = 42,

    #[error("RefreshReserve must appear before WithdrawObligationCollateralAndRedeemReserveCollateral in the same transaction")]
    SolendInstructionOrderInvalid = 43,

    #[error("duplicate or conflicting Solend instruction for the expected obligation/reserve in the same transaction")]
    SolendDuplicateOrConflictingInstruction = 44,

    #[error("Solend boundary verification failed (catch-all for malformed descriptors / cap violations)")]
    SolendBoundaryVerificationFailed = 45,
}

impl From<AuthorityError> for ProgramError {
    fn from(e: AuthorityError) -> Self {
        ProgramError::Custom(e as u32)
    }
}
