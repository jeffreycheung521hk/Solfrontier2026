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

    // ── Stage 2 Jupiter Bracket Verifier (J4 — append-only at codes 46+) ──
    //
    // Discriminants 46..=58 are appended for the Jupiter bracket
    // sibling-instruction verifier introduced in J4. Numeric values are
    // part of the on-chain wire shape (encoded into
    // `ProgramError::Custom`) and MUST stay append-only — never reorder
    // or renumber.
    //
    // J4 deliberately implements ONLY the sibling-instruction verifier.
    // The trustless pre/post balance bracket (dual-instruction
    // checkpoint PDA model from STAGE2_JUPITER_BRACKET_ALT_FEASIBILITY
    // § 4.1) is explicitly DEFERRED to a future slice — any code path
    // that accepts an executor-supplied `pre_balance` would be a
    // violation. The `JupiterBracketCheckpointMissing` /
    // `JupiterBracketCheckpointConsumed` codes are pre-allocated here
    // as part of an append-only contract so the future bracket slice
    // can wire them without renumbering anything else.

    #[error("execute_action for JupiterBuySolWithUsdc requires a JupiterBoundaryProof payload")]
    JupiterBoundaryProofMissing = 46,

    #[error("Jupiter boundary verification failed (catch-all for malformed descriptors / cap violations)")]
    JupiterBoundaryVerificationFailed = 47,

    #[error("sibling instruction's program_id is not in the Jupiter per-class allowlist")]
    JupiterIllegalSiblingInstruction = 48,

    #[error("sibling instructions violate the Jupiter setup → swap → cleanup → other → tip relative ordering")]
    JupiterInstructionOrderInvalid = 49,

    #[error("Jupiter swap sibling instruction's program_id does not equal the pinned Jupiter v6 mainnet program id")]
    JupiterProgramIdMismatch = 50,

    #[error("a sibling instruction marks an unexpected account as writable (outside the deterministic allowed-writable set)")]
    JupiterUnexpectedWritableAccount = 51,

    #[error("Jupiter boundary proof's destination_token_account does not match AuthorizationRecord.destination")]
    JupiterDestinationMismatch = 52,

    #[error("token-account fixture is not a recognized SPL Token account shape (owner / size / discriminator)")]
    JupiterTokenAccountInvalid = 53,

    #[error("token-account fixture's mint does not match the canonical action's expected mint")]
    JupiterMintMismatch = 54,

    /// Reserved for the dual-instruction balance-bracket slice — when
    /// `checked_sub(post_balance, pre_balance)` underflows, the bracket
    /// MUST surface this exact code rather than silently saturating.
    /// J4 does not yet wire a balance bracket; this discriminant is
    /// pre-allocated to keep the future slice append-only.
    #[error("post_balance < pre_balance — reserved for future trustless balance bracket")]
    JupiterPostBalanceUnderflow = 55,

    /// Reserved for the dual-instruction balance-bracket slice — when
    /// `delta < min_output_amount_raw`, the bracket MUST surface this
    /// exact code. J4 does not yet wire a balance bracket; this
    /// discriminant is pre-allocated to keep the future slice
    /// append-only.
    #[error("post_balance - pre_balance < min_output_amount_raw — reserved for future trustless balance bracket")]
    JupiterBalanceDeltaTooSmall = 56,

    #[error("Jupiter sibling instruction list exceeds MAX_JUPITER_SIBLING_INSTRUCTIONS")]
    JupiterSiblingListTooLarge = 57,

    /// Reserved for the dual-instruction balance-bracket slice —
    /// `PostJupiterBracketCheck` (or the post-bracket phase of
    /// `ExecuteAction`) finds no matching `JupiterBracketCheckpoint`
    /// PDA for the current authorization. J4 does not yet create
    /// checkpoint PDAs; this discriminant is pre-allocated.
    #[error("Jupiter bracket checkpoint PDA missing — reserved for future trustless balance bracket")]
    JupiterBracketCheckpointMissing = 58,

    /// Reserved for the dual-instruction balance-bracket slice — the
    /// checkpoint PDA was already consumed by an earlier
    /// `PostJupiterBracketCheck` and cannot be reused. J4 does not yet
    /// wire a balance bracket; this discriminant is pre-allocated.
    #[error("Jupiter bracket checkpoint already consumed — reserved for future trustless balance bracket")]
    JupiterBracketCheckpointConsumed = 59,

    // ── Stage 2 P5a Solend Account Decode + Boundary Hardening ──────────
    //
    // Discriminants 60..=72 are appended for the Solend live-account
    // decode substrate introduced in P5a (`solend_account_decode.rs`).
    // Numeric values are part of the on-chain wire shape (encoded into
    // `ProgramError::Custom`) and MUST stay append-only — never reorder
    // or renumber. P5a is decoder + boundary-helper substrate only —
    // no CPI, no invoke, no transaction construction. P5b will land
    // same-tx Refresh + Withdraw CPI and re-use these errors.

    #[error("Solend account data is shorter than the required Pack length (Obligation 1300, Reserve 619, or fixed prefix)")]
    SolendAccountDataTooShort = 60,

    #[error("Solana account-level owner program does not match the expected Solend program id (catch-all for non-program-specific account-info owner mismatches)")]
    SolendAccountOwnerMismatch = 61,

    #[error("Solend obligation decode failed (catch-all for malformed Pack tail / unrecoverable read)")]
    SolendObligationDecodeFailed = 62,

    #[error("Solend reserve decode failed (catch-all for malformed Pack tail / unrecoverable read)")]
    SolendReserveDecodeFailed = 63,

    #[error("Solend obligation version is not supported by this build (current SOLEND_SUPPORTED_VERSION = 1)")]
    SolendUnsupportedObligationVersion = 64,

    #[error("Solend reserve version is not supported by this build (current SOLEND_SUPPORTED_VERSION = 1)")]
    SolendUnsupportedReserveVersion = 65,

    #[error("decoded Obligation.lending_market does not match expected lending_market")]
    SolendObligationLendingMarketMismatch = 66,

    #[error("decoded Reserve.lending_market does not match expected lending_market")]
    SolendReserveLendingMarketMismatch = 67,

    #[error("Solend reserve last_update.slot does not equal current slot (strict same-slot helper for the future P5b Refresh + Withdraw flow)")]
    SolendLastUpdateSlotMismatch = 68,

    #[error("SPL token account-level owner program is not the native SPL Token program id")]
    SolendTokenAccountOwnerMismatch = 69,

    #[error("decoded SPL token account mint does not match the expected mint")]
    SolendTokenAccountMintMismatch = 70,

    #[error("decoded SPL token account authority/owner does not match the expected wallet")]
    SolendTokenAccountAuthorityMismatch = 71,

    #[error("SPL token account data length is not the native 165-byte SPL Token Account size (Token-2022 not supported by this slice)")]
    SolendTokenAccountInvalidLength = 72,

    // ── Stage 2 P5b-1 Solend CPI Builder Boundary Errors ────────────────
    //
    // Discriminants 73..=78 are appended for the Solend CPI builder
    // shadow-mapping validator introduced in P5b-1 (`solend_cpi_builder.rs`).
    // Numeric values are part of the on-chain wire shape (encoded into
    // `ProgramError::Custom`) and MUST stay append-only — never reorder
    // or renumber. P5b-1 is a builder + shadow-mapping substrate slice;
    // it issues no CPI, no `invoke`, no `invoke_signed`, no transaction
    // construction. P5b-2 will land same-tx Refresh + Withdraw CPI and
    // re-use these errors at the boundary.

    #[error("Solend CPI account-list parse / order is invalid (fewer than the required base accounts, or a slot is in the wrong position)")]
    SolendCpiAccountOrderInvalid = 73,

    #[error("Solend CPI sysvar slot does not match the canonical Sysvar pubkey, or a base slot is masquerading as a sysvar")]
    SolendCpiSysvarMismatch = 74,

    #[error("Solend CPI token-program slot is not the canonical native SPL Token program id")]
    SolendCpiTokenProgramMismatch = 75,

    #[error("Solend CPI withdraw rejected: live cToken `amount` is zero (drained position) — fail closed before building withdraw bytes")]
    SolendCpiZeroCollateralAmount = 76,

    #[error("Solend CPI oracle verification not yet implemented — P5b-1 fail-closed gate (P5b-2 prerequisite)")]
    SolendCpiOracleUnverified = 77,

    #[error("Solend CPI builder invariant failed (catch-all for malformed account-info shape, missing signer flag, etc.)")]
    SolendCpiBuilderInvariantFailed = 78,

    // ── Stage 2 P5b-2 Solend CPI Skeleton (append-only at codes 79+) ────
    //
    // Discriminants 79..=87 are appended for the Solend CPI skeleton
    // wired into ExecuteAction's SolendWithdrawAllDelegated path in
    // P5b-2. Numeric values are part of the on-chain wire shape (encoded
    // into `ProgramError::Custom`) and MUST stay append-only — never
    // reorder or renumber. P5b-2 introduces `invoke` (NOT `invoke_signed`)
    // CPI helpers gated by an oracle-validation status check; the
    // production success path remains fail-closed under Oracle Mode B
    // until oracle validation lands.

    #[error("Solend CPI requires the delegated_wallet AccountInfo at the canonical slot to be present and parseable")]
    SolendCpiDelegatedWalletMissing = 79,

    #[error("Solend CPI requires AuthorizationRecord.delegated_wallet to be a transaction-level signer")]
    SolendCpiDelegatedWalletNotSigner = 80,

    #[error("Solend CPI delegated_wallet AccountInfo key does not equal AuthorizationRecord.delegated_wallet")]
    SolendCpiDelegatedWalletMismatch = 81,

    #[error("Solend CPI RefreshReserve invocation failed (CPI returned an error)")]
    SolendCpiRefreshReserveFailed = 82,

    #[error("Solend CPI RefreshObligation invocation failed (CPI returned an error)")]
    SolendCpiRefreshObligationFailed = 83,

    #[error("Solend CPI WithdrawObligationCollateralAndRedeemReserveCollateral invocation failed (CPI returned an error)")]
    SolendCpiWithdrawFailed = 84,

    #[error("Solend CPI ExecuteAction success path is blocked: oracle shadow mapping is unresolved (Oracle Mode B fail-closed)")]
    SolendCpiSuccessPathBlockedByOracleValidation = 85,

    #[error("Solend CPI account list is missing required slots (delegated_wallet at 4, base 12 variant-15 accounts at 5..=16, oracle slots at 17..=18, clock sysvar at 19)")]
    SolendCpiInsufficientAccounts = 86,

    #[error("Solend CPI borrow-across-invoke risk detected (defensive sentinel — borrows must be dropped before invoke)")]
    SolendCpiBorrowAcrossInvokeRisk = 87,
}

impl From<AuthorityError> for ProgramError {
    fn from(e: AuthorityError) -> Self {
        ProgramError::Custom(e as u32)
    }
}
