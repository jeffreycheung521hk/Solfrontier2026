//! Stage 2 P5b-1 — Solend CPI builders + shadow-mapping substrate.
//!
//! # What this module is
//!
//! Pure builders that construct the **byte-shape** and **`Vec<AccountMeta>`**
//! that a future P5b-2 slice will pass to `invoke_signed` against the
//! Solend mainnet token-lending program. This slice also wires the
//! **shadow-mapping validator** that cross-checks every account
//! `clawsol-authority` is about to forward to Solend against the
//! decoded fixtures from P5a (`solend_account_decode`) and the
//! committed `AuthorizationRecord`.
//!
//! Three Solend instructions are covered:
//!
//! 1. `RefreshReserve` (variant `3`)
//! 2. `RefreshObligation` (variant `7`)
//! 3. `WithdrawObligationCollateralAndRedeemReserveCollateral`
//!    (variant `15`)
//!
//! # What this module is NOT
//!
//! - **Not a CPI.** No `invoke`, no `invoke_signed`. No transaction
//!   construction. No signing. No broadcast. No `RpcClient`. P5b-2
//!   will land same-tx Refresh + Withdraw `invoke_signed`.
//! - **Not an oracle verifier.** Solend reserve oracle pubkeys (Pyth
//!   slot, Switchboard slot, optional extra-oracle slot) cannot be
//!   derived from the P5a `DecodedSolendReserve` projection, which
//!   only reads the 42-byte prefix (`version` + `last_update` +
//!   `lending_market`). P5b-1 provides a fail-closed
//!   [`verify_oracles_or_fail_closed_p5b1`] hook that always errors
//!   with [`SolendCpiOracleUnverified`]. Wiring real oracle validation
//!   is a P5b-2 prerequisite.
//! - **Not a `find_program_address` caller.** The Solend SDK builder
//!   for variant 15 derives `lending_market_authority` via
//!   `Pubkey::find_program_address(&[&lending_market.to_bytes()],
//!   &solend_program_id)`. The on-chain hot path here MUST NOT pay
//!   that BPF compute cost: callers (the future P5b-2 processor)
//!   resolve the PDA off-chain or via a cached committed pubkey, then
//!   pass it in. The only `find_program_address` call sites in this
//!   module are inside `#[cfg(test)]` fixture builders.
//! - **Not a state mutator.** No `AuthorizationRecord` field is
//!   written here. P5b-2's `process_execute_action` boundary path
//!   continues to be the only writer.
//! - **Not a sweep / rent-cleanup helper.** The withdraw-all path does
//!   NOT close the cToken account afterwards, and emits no
//!   rent-cleanup instruction.
//!
//! # Signer / bump policy for P5b-2 (DOCUMENT ONLY — not exercised here)
//!
//! Solend variant-15 `WithdrawObligationCollateralAndRedeemReserveCollateral`
//! requires TWO signers (per the deployed mainnet processor):
//!
//! - slot 9 — `obligation_owner`
//! - slot 10 — `user_transfer_authority`
//!
//! Stage 2 invariant (`solend_boundary.rs`): the obligation owner
//! field equals `AuthorizationRecord.delegated_wallet` and is hard-
//! rejected if it equals `AuthorizationRecord.user`. So the signer
//! for both slot 9 and slot 10 in any P5b-2 CPI MUST be the
//! `delegated_wallet`. Under no circumstances is the user main
//! wallet signed for Solend operations.
//!
//! P5b-2 needs one of:
//!
//! - **(A) `delegated_wallet` is a PDA owned by `clawsol-authority`.**
//!   Then the P5b-2 processor signs via `invoke_signed(..., &[&[…,
//!   &[bump]]])` using the bump committed to
//!   `AuthorizationRecord.bump`. NOTE: today
//!   `AuthorizationRecord.bump` is the **bump for the
//!   `AuthorizationRecord` PDA itself**, not for the
//!   `delegated_wallet` PDA. P5b-2 therefore needs an additional
//!   committed bump field (or a deterministic derivation rule) for
//!   the delegated-wallet PDA. **This is a P5b-2 blocker** — see the
//!   final-report "Signer / bump policy" section.
//!
//! - **(B) `delegated_wallet` is a fully external Ed25519 keypair
//!   that the user's daemon holds.** Then `clawsol-authority` cannot
//!   sign as the delegated wallet — the daemon must co-sign the
//!   transaction at the wire level. The `invoke_signed` path in P5b-2
//!   would only sign the `AuthorizationRecord` PDA's seeds (for any
//!   PDA-owned ATA / state account), and the delegated wallet's
//!   signature comes from the off-chain transaction signer. This
//!   matches the live Slice 3G `BPfDMm…hhs5L` controlled-wallet model
//!   pinned in `reference_slice3c_controlled_wallet.md`. Under this
//!   model, `clawsol-authority`'s P5b-2 signer slot for slot 9 / 10
//!   is filled at the transaction level, not via `invoke_signed`'s
//!   signer-seeds. The boundary verifier must still enforce that the
//!   AccountInfo at slot 9 / 10 has `is_signer == true` AND
//!   `key == record.delegated_wallet`.
//!
//! P5b-1 takes NO position on (A) vs (B) — both are open in the
//! current state.rs schema. The final report flags this as a P5b-2
//! prerequisite.
//!
//! # Sources consulted
//!
//! - Solend mainnet `token-lending/sdk/src/instruction.rs`,
//!   `pub fn refresh_reserve(...)` constructor — variant tag and
//!   AccountMeta order.
//!   ([raw](https://raw.githubusercontent.com/solendprotocol/solana-program-library/mainnet/token-lending/sdk/src/instruction.rs))
//! - Solend mainnet `token-lending/sdk/src/instruction.rs`,
//!   `pub fn refresh_obligation(...)` constructor.
//! - Solend mainnet `token-lending/sdk/src/instruction.rs`,
//!   `pub fn withdraw_obligation_collateral_and_redeem_reserve_collateral(...)`
//!   constructor (12 base accounts, signer pair at slots 9 & 10,
//!   spl_token at slot 11, then 0..N obligation deposit reserves).
//! - Solend mainnet `token-lending/program/src/processor.rs`,
//!   `process_refresh_reserve`, `process_refresh_obligation`,
//!   `process_withdraw_obligation_collateral_and_redeem_reserve_collateral`
//!   — `is_signer` and writable assertions; `Clock::get()?` is read
//!   via sysvar syscall, NOT from the account list, on RefreshObligation
//!   and Withdraw.
//! - Repo parity: `crates/gateway/src/integrations/solend/refresh.rs`
//!   and `crates/gateway/src/integrations/solend/withdraw.rs` —
//!   live-mainnet validated layouts (Phase 6I-H / 6I-J corrections).
//!   The deployed mainnet `RefreshReserve` keeps the clock account at
//!   slot 3; `RefreshObligation` and `Withdraw` do NOT. This module's
//!   builders match the gateway's pinned layout.
//! - SPL Token program id constant — `TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA`,
//!   re-exported from [`crate::solend_account_decode::SPL_TOKEN_PROGRAM_ID`].
//! - Solana sysvar pubkey:
//!   `solana_program::sysvar::clock::id()` →
//!   `SysvarC1ock11111111111111111111111111111111`.
//!   `solana_program::sysvar::rent::id()` →
//!   `SysvarRent111111111111111111111111111111111`.

use solana_program::{
    account_info::AccountInfo,
    instruction::AccountMeta,
    program_error::ProgramError,
    pubkey::Pubkey,
    sysvar,
};

use crate::error::AuthorityError;
use crate::solend_account_decode::{
    decode_obligation, decode_reserve, decode_spl_token_account_lite,
    require_obligation_lending_market, require_obligation_owned_by_delegated_wallet,
    require_reserve_lending_market, require_spl_token_authority, require_spl_token_mint,
    DecodedSolendObligation, DecodedSolendReserve, DecodedSplTokenAccountLite,
    SPL_TOKEN_PROGRAM_ID,
};
use crate::solend_boundary::SOLEND_PROGRAM_ID_MAINNET;
use crate::state::AuthorizationRecord;

// ── Pinned Solend variant bytes ─────────────────────────────────────────────

/// `LendingInstruction::RefreshReserve` ordinal.
///
/// Source: `token-lending/sdk/src/instruction.rs` —
/// `Self::RefreshReserve => { buf.push(3); }` in the `pack()` impl.
pub const SOLEND_VARIANT_REFRESH_RESERVE: u8 = 3;

/// `LendingInstruction::RefreshObligation` ordinal.
///
/// Source: `token-lending/sdk/src/instruction.rs` —
/// `Self::RefreshObligation => { buf.push(7); }` in the `pack()` impl.
pub const SOLEND_VARIANT_REFRESH_OBLIGATION: u8 = 7;

/// `LendingInstruction::WithdrawObligationCollateralAndRedeemReserveCollateral`
/// ordinal.
///
/// Source: `token-lending/sdk/src/instruction.rs` —
/// `Self::WithdrawObligationCollateralAndRedeemReserveCollateral { collateral_amount } => { buf.push(15); buf.extend_from_slice(&collateral_amount.to_le_bytes()); }`
/// in the `pack()` impl.
pub const SOLEND_VARIANT_WITHDRAW_OBLIGATION_COLLATERAL_AND_REDEEM: u8 = 15;

/// Number of base accounts for variant 15 before the variable-length
/// deposit-reserves tail. Source: `processor.rs`'s
/// `process_withdraw_obligation_collateral_and_redeem_reserve_collateral`
/// pulls slots 0..=11 with `next_account_info` then passes
/// `&accounts[12..]` into `_withdraw_obligation_collateral`.
pub const WITHDRAW_BASE_ACCOUNTS_LEN: usize = 12;

// ── Instruction-data builders ───────────────────────────────────────────────

/// Build the instruction-data bytes for `RefreshReserve`.
///
/// Wire shape: a single byte `[3]`. The variant carries no payload.
///
/// Source: `token-lending/sdk/src/instruction.rs`,
/// `LendingInstruction::pack()` for `RefreshReserve`:
/// ```text
/// Self::RefreshReserve => { buf.push(3); }
/// ```
pub fn build_refresh_reserve_data() -> Vec<u8> {
    vec![SOLEND_VARIANT_REFRESH_RESERVE]
}

/// Build the instruction-data bytes for `RefreshObligation`.
///
/// Wire shape: a single byte `[7]`. The variant carries no payload.
///
/// Source: `token-lending/sdk/src/instruction.rs`,
/// `LendingInstruction::pack()` for `RefreshObligation`:
/// ```text
/// Self::RefreshObligation => { buf.push(7); }
/// ```
pub fn build_refresh_obligation_data() -> Vec<u8> {
    vec![SOLEND_VARIANT_REFRESH_OBLIGATION]
}

/// Build the instruction-data bytes for
/// `WithdrawObligationCollateralAndRedeemReserveCollateral`.
///
/// Wire shape: variant byte `15` followed by `amount.to_le_bytes()`
/// (8 bytes) — total 9 bytes.
///
/// Source: `token-lending/sdk/src/instruction.rs`,
/// `LendingInstruction::pack()` for the variant:
/// ```text
/// Self::WithdrawObligationCollateralAndRedeemReserveCollateral { collateral_amount } => {
///     buf.push(15);
///     buf.extend_from_slice(&collateral_amount.to_le_bytes());
/// }
/// ```
///
/// `amount` is in cToken (collateral) base units. P5b-1 callers pass
/// the live cToken balance read from the delegated cToken account via
/// the P5a `DecodedSplTokenAccountLite` decoder. P5b-1 does NOT use
/// the `u64::MAX` "redeem all" sentinel that the off-chain gateway
/// uses; instead, the P5b-1 invariant is:
///
/// - read the live cToken `amount`, and
/// - fail closed with [`AuthorityError::SolendCpiZeroCollateralAmount`]
///   if `amount == 0` BEFORE building the instruction data.
///
/// The on-chain Solend processor itself accepts both shapes; the
/// stricter P5b-1 rule is a defence-in-depth choice — the
/// authorization PDA is single-shot, so a zero-balance withdraw would
/// burn the authorization while transferring nothing, which is
/// indistinguishable from a successful no-op and a wedge. Failing
/// closed at zero forces the user to revoke + re-authorize after a
/// drained-out position, instead of silently losing the rule.
///
/// TODO(P5b-2): if the official Solend mainnet
/// `_withdraw_obligation_collateral` clamping behavior is re-verified
/// to match the gateway-side `WITHDRAW_ALL_COLLATERAL_AMOUNT_SENTINEL_RAW
/// = u64::MAX` model (and the live-mainnet evidence holds), P5b-2 may
/// allow the `u64::MAX` sentinel as an explicit caller opt-in. Even
/// then, the P5b-1 default (read live amount, fail at 0) remains
/// safer for the autonomous-execution path because it surfaces the
/// drained-position case immediately.
pub fn build_withdraw_obligation_collateral_and_redeem_reserve_collateral_data(
    amount: u64,
) -> Vec<u8> {
    let mut data = Vec::with_capacity(9);
    data.push(SOLEND_VARIANT_WITHDRAW_OBLIGATION_COLLATERAL_AND_REDEEM);
    data.extend_from_slice(&amount.to_le_bytes());
    data
}

// ── AccountMeta builders ────────────────────────────────────────────────────

/// Build the `Vec<AccountMeta>` for `RefreshReserve`.
///
/// Source: `token-lending/sdk/src/instruction.rs`,
/// `pub fn refresh_reserve(...)` and the deployed mainnet processor:
///
/// ```text
/// idx | name                  | is_signer | is_writable
/// ----+-----------------------+-----------+-----------
///   0 | reserve               | false     | true
///   1 | pyth_oracle           | false     | false
///   2 | switchboard_oracle    | false     | false
///   3 | clock_sysvar          | false     | false   // mainnet branch keeps this slot
///   4 | extra_oracle (opt.)   | false     | false   // appended only when Some
/// ```
///
/// **Clock at slot 3 is load-bearing on mainnet.** The Phase 6I-H
/// audit in `crates/gateway/src/integrations/solend/refresh.rs`
/// verified this against live mainnet by submitting a layout that
/// dropped the clock slot — the deployed processor returned
/// `InvalidAccountOwner`. RefreshObligation and Withdraw do NOT take
/// clock as an account (their handlers read `Clock::get()?` via
/// sysvar syscall); only RefreshReserve does. See the gateway file's
/// docs for the live failure receipts.
///
/// Caller MUST supply the canonical `solana_program::sysvar::clock::id()`
/// for `clock_sysvar`. This builder does NOT inject it on behalf of
/// the caller — the canonical sysvar id is checked by
/// [`validate_solend_withdraw_shadow_mapping`] when the boundary
/// dispatches. P5b-2 will ensure the boundary verifier rejects any
/// non-canonical clock pubkey before the CPI fires.
///
/// `extra_oracle_pubkey` is appended ONLY when `Some(_)` is supplied.
/// Solend's modern reserves use the 4-account layout; older reserves
/// configured for an extra oracle source append a 5th readonly slot.
/// The variant byte is unchanged.
pub fn build_refresh_reserve_metas(
    reserve_pubkey: Pubkey,
    pyth_oracle_pubkey: Pubkey,
    switchboard_oracle_pubkey: Pubkey,
    clock_sysvar_pubkey: Pubkey,
    extra_oracle_pubkey: Option<Pubkey>,
) -> Vec<AccountMeta> {
    let mut metas = Vec::with_capacity(if extra_oracle_pubkey.is_some() { 5 } else { 4 });
    metas.push(AccountMeta::new(reserve_pubkey, false));
    metas.push(AccountMeta::new_readonly(pyth_oracle_pubkey, false));
    metas.push(AccountMeta::new_readonly(switchboard_oracle_pubkey, false));
    metas.push(AccountMeta::new_readonly(clock_sysvar_pubkey, false));
    if let Some(extra) = extra_oracle_pubkey {
        metas.push(AccountMeta::new_readonly(extra, false));
    }
    metas
}

/// Build the `Vec<AccountMeta>` for `RefreshObligation`.
///
/// Source: `token-lending/sdk/src/instruction.rs`,
/// `pub fn refresh_obligation(...)`:
///
/// ```text
/// idx          | name                | is_signer | is_writable
/// -------------+---------------------+-----------+-----------
///   0          | obligation          | false     | true
///   1..=N      | reserve_pubkeys[i]  | false     | true
/// ```
///
/// `reserve_pubkeys` MUST be the obligation's deposit-then-borrow
/// reserve list, in obligation-order. The deployed processor passes
/// `&accounts[1..]` into `update_borrow_attribution_values`, which
/// `pack`s each reserve back at the end of the call — hence the
/// writable flag on every reserve slot. Marking any of these readonly
/// is a latent `ReadonlyDataModified` failure (audit Phase 6I-K).
///
/// Clock is NOT in this list (Phase 6I-H correction); the deployed
/// processor reads it via `Clock::get()?` syscall.
pub fn build_refresh_obligation_metas(
    obligation_pubkey: Pubkey,
    reserve_pubkeys: &[Pubkey],
) -> Vec<AccountMeta> {
    let mut metas = Vec::with_capacity(1 + reserve_pubkeys.len());
    metas.push(AccountMeta::new(obligation_pubkey, false));
    for r in reserve_pubkeys {
        metas.push(AccountMeta::new(*r, false));
    }
    metas
}

/// Typed collector for the Solend variant-15
/// `WithdrawObligationCollateralAndRedeemReserveCollateral` accounts.
///
/// Each field has a stable name; downstream readers (the future P5b-2
/// processor, the shadow-mapping validator) reference fields by name
/// rather than by index. `accounts[7]` magic indices are forbidden.
///
/// The signer pair at slots 9 and 10 (`obligation_owner` and
/// `user_transfer_authority`) is, in the Stage 2 delegated model,
/// always the same pubkey — `AuthorizationRecord.delegated_wallet`.
/// The shadow-mapping validator enforces both must equal the
/// committed `delegated_wallet`.
///
/// `deposit_reserves` carries the 0..N obligation-deposit-reserve
/// accounts that follow `token_program`. They MUST be in
/// obligation-deposit order — the on-chain processor consumes them
/// via `update_borrow_attribution_values` and `pack`s each back
/// (writable). For the simple Stage 2 single-deposit case this slice
/// will typically be one element, equal to `withdraw_reserve`.
#[derive(Debug, Clone, Copy)]
pub struct SolendWithdrawAccounts<'a, 'info> {
    /// 0 — `source_collateral` (the reserve's collateral supply pool).
    /// Writable, not signer.
    pub source_collateral: &'a AccountInfo<'info>,
    /// 1 — `destination_collateral` (the user / delegated wallet's
    /// cToken ATA, intermediary). Writable, not signer.
    pub delegated_ctoken: &'a AccountInfo<'info>,
    /// 2 — `withdraw_reserve` (the reserve being redeemed from).
    /// Writable, not signer.
    pub reserve: &'a AccountInfo<'info>,
    /// 3 — `obligation`. Writable, not signer.
    pub obligation: &'a AccountInfo<'info>,
    /// 4 — `lending_market`. Writable, not signer.
    pub lending_market: &'a AccountInfo<'info>,
    /// 5 — `lending_market_authority` (PDA derived from
    /// `(lending_market, solend_program_id)`). Readonly, not signer.
    pub lending_market_authority: &'a AccountInfo<'info>,
    /// 6 — `destination_liquidity` (the destination underlying ATA).
    /// Writable, not signer.
    pub destination: &'a AccountInfo<'info>,
    /// 7 — `reserve_collateral_mint` (cToken mint). Writable, not signer.
    pub reserve_collateral_mint: &'a AccountInfo<'info>,
    /// 8 — `reserve_liquidity_supply` (source underlying pool).
    /// Writable, not signer.
    pub reserve_liquidity_supply: &'a AccountInfo<'info>,
    /// 9 — `obligation_owner`. Readonly, **signer**. In the Stage 2
    /// delegated model this is `AuthorizationRecord.delegated_wallet`.
    pub obligation_owner: &'a AccountInfo<'info>,
    /// 10 — `user_transfer_authority`. Readonly, **signer**. Stage 2
    /// invariant: same pubkey as `obligation_owner`
    /// (`delegated_wallet`).
    pub user_transfer_authority: &'a AccountInfo<'info>,
    /// 11 — `token_program` (must be the native SPL Token program).
    /// Readonly, not signer.
    pub token_program: &'a AccountInfo<'info>,
}

impl<'a, 'info> SolendWithdrawAccounts<'a, 'info> {
    /// Parse the 12 base accounts in their pinned order. Returns
    /// `Err(SolendCpiAccountOrderInvalid)` if the input has fewer
    /// than 12 elements. The remaining `accounts[12..]` are NOT
    /// consumed here — the future P5b-2 processor handles the
    /// deposit-reserve tail separately.
    pub fn parse(accounts: &'a [AccountInfo<'info>]) -> Result<Self, ProgramError> {
        if accounts.len() < WITHDRAW_BASE_ACCOUNTS_LEN {
            return Err(AuthorityError::SolendCpiAccountOrderInvalid.into());
        }
        Ok(Self {
            source_collateral: &accounts[0],
            delegated_ctoken: &accounts[1],
            reserve: &accounts[2],
            obligation: &accounts[3],
            lending_market: &accounts[4],
            lending_market_authority: &accounts[5],
            destination: &accounts[6],
            reserve_collateral_mint: &accounts[7],
            reserve_liquidity_supply: &accounts[8],
            obligation_owner: &accounts[9],
            user_transfer_authority: &accounts[10],
            token_program: &accounts[11],
        })
    }
}

/// Build the base `Vec<AccountMeta>` (slots 0..=11) for
/// `WithdrawObligationCollateralAndRedeemReserveCollateral`.
///
/// Source: `token-lending/sdk/src/instruction.rs`,
/// `pub fn withdraw_obligation_collateral_and_redeem_reserve_collateral(...)`,
/// pinned via the live-mainnet validation in the repo's
/// `crates/gateway/src/integrations/solend/withdraw.rs` (Phase 6I-J):
///
/// ```text
/// idx | name                       | is_signer | is_writable
/// ----+----------------------------+-----------+-----------
///   0 | source_collateral          | false     | true
///   1 | delegated_ctoken (dest.)   | false     | true
///   2 | withdraw_reserve           | false     | true
///   3 | obligation                 | false     | true
///   4 | lending_market             | false     | true
///   5 | lending_market_authority   | false     | false
///   6 | destination_liquidity      | false     | true
///   7 | reserve_collateral_mint    | false     | true
///   8 | reserve_liquidity_supply   | false     | true
///   9 | obligation_owner           | true      | false
///  10 | user_transfer_authority    | true      | false
///  11 | token_program              | false     | false
/// ```
///
/// **No clock or rent sysvar.** The deployed mainnet handler reads
/// `Clock::get()?` via sysvar syscall (Phase 6I-J live failure
/// receipts: 2026-05-08 tx `25REcnNp…` and `2mLa5bQv…` documented in
/// `crates/gateway/src/integrations/solend/withdraw.rs`).
///
/// `deposit_reserves` is appended after slot 11; each entry is
/// writable, not signer. Order MUST be the obligation's deposit
/// order — the processor consumes them via
/// `update_borrow_attribution_values(&accounts[12..])` and `pack`s
/// each back.
///
/// **Signer pair invariant.** Slots 9 and 10 are the only signers.
/// In the Stage 2 delegated model both pubkeys are
/// `AuthorizationRecord.delegated_wallet`. Callers building the meta
/// list directly typically pass `delegated_wallet` for both; the
/// shadow-mapping validator enforces this.
#[allow(clippy::too_many_arguments)]
pub fn build_withdraw_obligation_collateral_and_redeem_reserve_collateral_metas(
    source_collateral: Pubkey,
    delegated_ctoken: Pubkey,
    withdraw_reserve: Pubkey,
    obligation: Pubkey,
    lending_market: Pubkey,
    lending_market_authority: Pubkey,
    destination_liquidity: Pubkey,
    reserve_collateral_mint: Pubkey,
    reserve_liquidity_supply: Pubkey,
    obligation_owner: Pubkey,
    user_transfer_authority: Pubkey,
    token_program: Pubkey,
    deposit_reserves: &[Pubkey],
) -> Vec<AccountMeta> {
    let mut metas = Vec::with_capacity(WITHDRAW_BASE_ACCOUNTS_LEN + deposit_reserves.len());
    metas.push(AccountMeta::new(source_collateral, false)); // 0
    metas.push(AccountMeta::new(delegated_ctoken, false)); // 1
    metas.push(AccountMeta::new(withdraw_reserve, false)); // 2
    metas.push(AccountMeta::new(obligation, false)); // 3
    metas.push(AccountMeta::new(lending_market, false)); // 4
    metas.push(AccountMeta::new_readonly(lending_market_authority, false)); // 5
    metas.push(AccountMeta::new(destination_liquidity, false)); // 6
    metas.push(AccountMeta::new(reserve_collateral_mint, false)); // 7
    metas.push(AccountMeta::new(reserve_liquidity_supply, false)); // 8
    metas.push(AccountMeta::new_readonly(obligation_owner, true)); // 9 — signer
    metas.push(AccountMeta::new_readonly(user_transfer_authority, true)); // 10 — signer
    metas.push(AccountMeta::new_readonly(token_program, false)); // 11
    for r in deposit_reserves {
        metas.push(AccountMeta::new(*r, false));
    }
    metas
}

// ── Shadow-mapping validator ────────────────────────────────────────────────

/// Inputs for [`validate_solend_withdraw_shadow_mapping`].
///
/// Carries the *expected* values (committed via `AuthorizationRecord`,
/// the Solend boundary proof, or the rule body) that each
/// `AccountInfo` in the parsed account list must shadow-match.
///
/// `expected_lending_market` is the lending market the obligation /
/// reserve are bound to; it must agree across the obligation's inner
/// `lending_market` field, the reserve's inner `lending_market`
/// field, and the `accounts[4]` AccountInfo key.
///
/// `expected_ctoken_mint` is the cToken mint Solend issued to the
/// reserve at deposit time; the delegated cToken account at slot 1
/// must have this mint.
///
/// `expected_liquidity_mint` is the underlying mint the destination
/// at slot 6 must have. The destination at slot 6 must also be the
/// committed `AuthorizationRecord.destination` pubkey.
#[derive(Debug, Clone, Copy)]
pub struct SolendWithdrawShadowExpectations<'a> {
    pub solend_program_id: &'a Pubkey,
    pub expected_obligation: &'a Pubkey,
    pub expected_reserve: &'a Pubkey,
    pub expected_lending_market: &'a Pubkey,
    pub expected_ctoken_mint: &'a Pubkey,
    pub expected_liquidity_mint: &'a Pubkey,
}

/// Cross-check every `AccountInfo` in the variant-15 account list
/// against the committed `AuthorizationRecord` + boundary proof.
///
/// **All checks are mandatory and fail closed.** Any single failure
/// surfaces a specific error — no catch-all "validation failed".
///
/// Performed checks (in order):
///
/// 1. `obligation` AccountInfo.key == `expected_obligation`
/// 2. `obligation` AccountInfo.owner == `solend_program_id`
/// 3. decoded obligation `owner` == `record.delegated_wallet`
/// 4. decoded obligation `owner` != `record.user` (poison gate —
///    surfaced as [`AuthorityError::SolendMainWalletObligationRejected`])
/// 5. decoded obligation `lending_market` == `expected_lending_market`
/// 6. `reserve` AccountInfo.key == `expected_reserve`
/// 7. `reserve` AccountInfo.owner == `solend_program_id`
/// 8. decoded reserve `lending_market` == `expected_lending_market`
/// 9. `lending_market` AccountInfo.key == `expected_lending_market`
///    (i.e. the accounts[4] key matches the inner `lending_market`
///    field that the obligation/reserve both reference)
/// 10. `delegated_ctoken` AccountInfo.owner == `spl_token::id()`
/// 11. decoded delegated cToken `authority` (`owner` field) ==
///     `record.delegated_wallet`
/// 12. decoded delegated cToken `mint` == `expected_ctoken_mint`
/// 13. `destination` AccountInfo.key == `record.destination`
/// 14. `destination` AccountInfo.owner == `spl_token::id()`
/// 15. decoded destination `mint` == `expected_liquidity_mint`
/// 16. `token_program` AccountInfo.key == `spl_token::id()`
/// 17. `obligation_owner` AccountInfo.key == `record.delegated_wallet`
/// 18. `obligation_owner` AccountInfo.is_signer == true
/// 19. `user_transfer_authority` AccountInfo.key ==
///     `record.delegated_wallet`
/// 20. `user_transfer_authority` AccountInfo.is_signer == true
/// 21. `obligation_owner` AccountInfo.key != `record.user` (defence-
///     in-depth: the user main wallet must never appear at the signer
///     slot, even if some other check were to be bypassed).
///
/// **Oracles are NOT validated here.** P5b-1 hands oracle validation
/// to [`verify_oracles_or_fail_closed_p5b1`], which always errors.
///
/// **Sysvars are validated by a separate helper**
/// [`require_canonical_clock_sysvar`] — the variant-15 ix list does
/// not include any sysvar at all, so this validator only enforces the
/// no-sysvar invariant for the 12 base accounts (any of slots 0..=11
/// that happens to equal `clock::id()` or `rent::id()` would be a
/// shaped-account attack — surfaced as
/// [`AuthorityError::SolendCpiSysvarMismatch`]).
///
/// Returns the decoded obligation and reserve fixtures alongside the
/// validated cToken / destination decodes, for downstream callers
/// that need the prefix fields without re-decoding.
pub fn validate_solend_withdraw_shadow_mapping<'a, 'info>(
    accounts: &SolendWithdrawAccounts<'a, 'info>,
    record: &AuthorizationRecord,
    expectations: &SolendWithdrawShadowExpectations<'_>,
) -> Result<SolendWithdrawShadowDecoded, ProgramError> {
    // ── Sysvar / token-program defence: no canonical sysvar may
    //    occupy any of the 12 base slots. The variant-15 ix list does
    //    not include any sysvar, so any slot that *equals* the
    //    canonical clock or rent pubkey is a shaped-account attack.
    forbid_canonical_sysvars_in_base_slots(accounts)?;

    // ── Solend program-id self-check.
    if expectations.solend_program_id != &SOLEND_PROGRAM_ID_MAINNET {
        return Err(AuthorityError::SolendProgramMismatch.into());
    }

    // ── Obligation key + Solana account-level owner.
    if accounts.obligation.key != expectations.expected_obligation {
        return Err(AuthorityError::SolendCpiAccountOrderInvalid.into());
    }
    if accounts.obligation.owner != expectations.solend_program_id {
        return Err(AuthorityError::SolendAccountOwnerMismatch.into());
    }
    let obligation_data = accounts
        .obligation
        .try_borrow_data()
        .map_err(|_| ProgramError::AccountBorrowFailed)?;
    let decoded_obligation = decode_obligation(&obligation_data)?;
    drop(obligation_data);
    // Inner-owner gate (also surfaces the dedicated main-wallet error).
    require_obligation_owned_by_delegated_wallet(&decoded_obligation, record)?;
    require_obligation_lending_market(&decoded_obligation, expectations.expected_lending_market)?;

    // ── Reserve key + Solana account-level owner.
    if accounts.reserve.key != expectations.expected_reserve {
        return Err(AuthorityError::SolendReserveMismatch.into());
    }
    if accounts.reserve.owner != expectations.solend_program_id {
        return Err(AuthorityError::SolendAccountOwnerMismatch.into());
    }
    let reserve_data = accounts
        .reserve
        .try_borrow_data()
        .map_err(|_| ProgramError::AccountBorrowFailed)?;
    let decoded_reserve = decode_reserve(&reserve_data)?;
    drop(reserve_data);
    require_reserve_lending_market(&decoded_reserve, expectations.expected_lending_market)?;

    // ── Lending-market key cross-check.
    if accounts.lending_market.key != expectations.expected_lending_market {
        return Err(AuthorityError::SolendLendingMarketMismatch.into());
    }

    // ── Delegated cToken account: SPL Token program owner, mint,
    //    authority all match.
    if accounts.delegated_ctoken.owner != &SPL_TOKEN_PROGRAM_ID {
        return Err(AuthorityError::SolendTokenAccountOwnerMismatch.into());
    }
    let ctoken_data = accounts
        .delegated_ctoken
        .try_borrow_data()
        .map_err(|_| ProgramError::AccountBorrowFailed)?;
    let decoded_ctoken = decode_spl_token_account_lite(&ctoken_data)?;
    drop(ctoken_data);
    require_spl_token_authority(&decoded_ctoken, &record.delegated_wallet)?;
    require_spl_token_mint(&decoded_ctoken, expectations.expected_ctoken_mint)?;

    // ── Destination account: must be record.destination (the
    //    committed sweep target), SPL Token program-owned, with the
    //    underlying liquidity mint.
    if accounts.destination.key != &record.destination {
        return Err(AuthorityError::SolendDestinationMismatch.into());
    }
    if accounts.destination.owner != &SPL_TOKEN_PROGRAM_ID {
        return Err(AuthorityError::SolendTokenAccountOwnerMismatch.into());
    }
    let dest_data = accounts
        .destination
        .try_borrow_data()
        .map_err(|_| ProgramError::AccountBorrowFailed)?;
    let decoded_destination = decode_spl_token_account_lite(&dest_data)?;
    drop(dest_data);
    require_spl_token_mint(&decoded_destination, expectations.expected_liquidity_mint)?;

    // ── Token program account-key gate.
    if accounts.token_program.key != &SPL_TOKEN_PROGRAM_ID {
        return Err(AuthorityError::SolendCpiTokenProgramMismatch.into());
    }

    // ── Signer pair: both slots 9 and 10 are the delegated wallet,
    //    AND both must have is_signer == true. The user main wallet
    //    must never appear at the signer slot.
    if accounts.obligation_owner.key == &record.user {
        return Err(AuthorityError::SolendMainWalletObligationRejected.into());
    }
    if accounts.obligation_owner.key != &record.delegated_wallet {
        return Err(AuthorityError::SolendObligationAuthorityMismatch.into());
    }
    if !accounts.obligation_owner.is_signer {
        return Err(AuthorityError::SolendCpiBuilderInvariantFailed.into());
    }
    if accounts.user_transfer_authority.key == &record.user {
        return Err(AuthorityError::SolendMainWalletObligationRejected.into());
    }
    if accounts.user_transfer_authority.key != &record.delegated_wallet {
        return Err(AuthorityError::SolendObligationAuthorityMismatch.into());
    }
    if !accounts.user_transfer_authority.is_signer {
        return Err(AuthorityError::SolendCpiBuilderInvariantFailed.into());
    }

    Ok(SolendWithdrawShadowDecoded {
        obligation: decoded_obligation,
        reserve: decoded_reserve,
        delegated_ctoken: decoded_ctoken,
        destination: decoded_destination,
    })
}

/// Decoded views returned by
/// [`validate_solend_withdraw_shadow_mapping`] so downstream callers
/// don't pay the decode cost twice.
#[derive(Debug, Clone, Copy)]
pub struct SolendWithdrawShadowDecoded {
    pub obligation: DecodedSolendObligation,
    pub reserve: DecodedSolendReserve,
    pub delegated_ctoken: DecodedSplTokenAccountLite,
    pub destination: DecodedSplTokenAccountLite,
}

/// Reject any of the 12 base variant-15 slots that happens to equal
/// the canonical clock or rent sysvar pubkey. The variant-15 ix list
/// does NOT include any sysvar — a slot pretending to be one is a
/// shaped-account attack.
fn forbid_canonical_sysvars_in_base_slots<'a, 'info>(
    accounts: &SolendWithdrawAccounts<'a, 'info>,
) -> Result<(), ProgramError> {
    let clock = sysvar::clock::id();
    let rent = sysvar::rent::id();
    let slots: [&Pubkey; 12] = [
        accounts.source_collateral.key,
        accounts.delegated_ctoken.key,
        accounts.reserve.key,
        accounts.obligation.key,
        accounts.lending_market.key,
        accounts.lending_market_authority.key,
        accounts.destination.key,
        accounts.reserve_collateral_mint.key,
        accounts.reserve_liquidity_supply.key,
        accounts.obligation_owner.key,
        accounts.user_transfer_authority.key,
        accounts.token_program.key,
    ];
    for k in slots {
        if k == &clock || k == &rent {
            return Err(AuthorityError::SolendCpiSysvarMismatch.into());
        }
    }
    Ok(())
}

/// Defence helper: assert that an `AccountInfo` slot supplied as the
/// "clock sysvar" for `RefreshReserve` is the canonical
/// `solana_program::sysvar::clock::id()`. Used by P5b-2 when the
/// processor receives the RefreshReserve account list and must
/// reject any caller-supplied non-canonical clock pubkey before
/// dispatching.
pub fn require_canonical_clock_sysvar(account: &AccountInfo) -> Result<(), ProgramError> {
    if account.key != &sysvar::clock::id() {
        return Err(AuthorityError::SolendCpiSysvarMismatch.into());
    }
    Ok(())
}

/// Defence helper: assert that an `AccountInfo` supplied as the
/// "token program" is the canonical native SPL Token program id.
/// Used by the P5b-2 boundary path. Token-2022 accounts are NOT
/// supported by this slice.
pub fn require_canonical_token_program(account: &AccountInfo) -> Result<(), ProgramError> {
    if account.key != &SPL_TOKEN_PROGRAM_ID {
        return Err(AuthorityError::SolendCpiTokenProgramMismatch.into());
    }
    Ok(())
}

// ── Withdraw-all amount strategy ────────────────────────────────────────────

/// Read the live cToken `amount` off the delegated cToken account and
/// fail closed if it is zero.
///
/// Inputs:
///
/// - `accounts.delegated_ctoken` — the parsed slot-1 AccountInfo.
/// - `record.delegated_wallet` — committed authority the cToken
///   account must already be owned by.
/// - `expected_ctoken_mint` — the cToken mint the rule was authorised
///   against.
///
/// The function:
///
/// 1. Asserts SPL Token program ownership.
/// 2. Decodes the 165-byte SPL Token Account body via the P5a lite
///    decoder.
/// 3. Asserts the decoded mint and authority match the committed
///    expectations (defence in depth — these are also asserted by the
///    full shadow-mapping validator).
/// 4. Returns the live `amount`, after rejecting zero with
///    [`AuthorityError::SolendCpiZeroCollateralAmount`].
///
/// P5b-1 callers wire the returned `u64` directly into
/// [`build_withdraw_obligation_collateral_and_redeem_reserve_collateral_data`]
/// — the live amount is what the on-chain Solend processor uses to
/// burn cTokens and emit the underlying.
///
/// **No `u64::MAX` sentinel.** P5b-1 takes the conservative live-read
/// path. Callers asking for "withdraw all" get the exact live cToken
/// amount, not a clamping sentinel; if the live amount is zero (e.g.
/// position fully drained off-band) the call fails closed before any
/// instruction data is constructed.
///
/// TODO(P5b-2): if mainnet evidence supports allowing
/// `u64::MAX = WITHDRAW_ALL_COLLATERAL_AMOUNT_SENTINEL_RAW` as a
/// caller opt-in, P5b-2 may add a sibling helper. Even then,
/// `read_live_ctoken_amount_or_fail_closed` remains the recommended
/// default to surface drained-position failures.
///
/// **No close-account / rent-cleanup.** The cToken account is left
/// in place. P5b-1 emits no `CloseAccount` instruction and provides
/// no helper to do so.
pub fn read_live_ctoken_amount_or_fail_closed<'a, 'info>(
    delegated_ctoken: &'a AccountInfo<'info>,
    record: &AuthorizationRecord,
    expected_ctoken_mint: &Pubkey,
) -> Result<u64, ProgramError> {
    if delegated_ctoken.owner != &SPL_TOKEN_PROGRAM_ID {
        return Err(AuthorityError::SolendTokenAccountOwnerMismatch.into());
    }
    let data = delegated_ctoken
        .try_borrow_data()
        .map_err(|_| ProgramError::AccountBorrowFailed)?;
    let decoded = decode_spl_token_account_lite(&data)?;
    drop(data);
    require_spl_token_authority(&decoded, &record.delegated_wallet)?;
    require_spl_token_mint(&decoded, expected_ctoken_mint)?;
    if decoded.amount == 0 {
        return Err(AuthorityError::SolendCpiZeroCollateralAmount.into());
    }
    Ok(decoded.amount)
}

// ── Oracle gap (deferred to P5b-2) ──────────────────────────────────────────

/// Fail-closed oracle verification hook.
///
/// **Always errors with [`AuthorityError::SolendCpiOracleUnverified`].**
///
/// The Solend reserve account stores the Pyth, Switchboard, and
/// optional extra-oracle pubkeys at offsets that the P5a `Reserve`
/// decoder (`solend_account_decode::DecodedSolendReserve`) does NOT
/// currently project — only the 42-byte prefix
/// (`version` + `last_update` + `lending_market`) is decoded.
/// Extending the decoder to project the oracle pubkeys requires
/// re-verifying the full mainnet `Pack` layout offsets that the P5a
/// research (`docs/STAGE2_SOLEND_APY_RESEARCH.md` § 8 B-O1) explicitly
/// flagged as un-pinned end-to-end.
///
/// Until that decoder extension lands and the boundary verifier can
/// match the Pyth / Switchboard pubkeys passed at slots 1 / 2 of
/// `RefreshReserve` against the committed reserve's stored oracle
/// fields, P5b-1 leaves the gap explicit. Any caller that tries to
/// "validate" oracles via this hook MUST surface the error
/// transparently.
///
/// TODO(P5b-2):
///
/// 1. Extend `solend_account_decode::DecodedSolendReserve` to project
///    the reserve's `liquidity.pyth_oracle_pubkey`,
///    `liquidity.switchboard_oracle_pubkey`, and the optional
///    `extra_oracle_pubkey` field.
/// 2. Re-pin the offsets against the Solend mainnet `Pack` source
///    (`token-lending/sdk/src/state/reserve.rs`) and add direct
///    mainnet-fixture round-trip tests for the new offsets.
/// 3. Replace this hook with a per-slot validator
///    `verify_refresh_reserve_oracles(...)` that decodes the reserve
///    and compares each oracle pubkey to the corresponding
///    AccountInfo slot.
pub fn verify_oracles_or_fail_closed_p5b1(
    _refresh_reserve_pyth_account: &AccountInfo,
    _refresh_reserve_switchboard_account: &AccountInfo,
    _refresh_reserve_extra_oracle_account: Option<&AccountInfo>,
    _expected_reserve: &Pubkey,
) -> Result<(), ProgramError> {
    Err(AuthorityError::SolendCpiOracleUnverified.into())
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::solend_account_decode::{
        OBLIGATION_OFFSET_LAST_UPDATE_SLOT, OBLIGATION_OFFSET_LAST_UPDATE_STALE,
        OBLIGATION_OFFSET_LENDING_MARKET, OBLIGATION_OFFSET_OWNER,
        OBLIGATION_OFFSET_VERSION, RESERVE_OFFSET_LAST_UPDATE_SLOT,
        RESERVE_OFFSET_LAST_UPDATE_STALE, RESERVE_OFFSET_LENDING_MARKET,
        RESERVE_OFFSET_VERSION, SOLEND_OBLIGATION_LEN, SOLEND_RESERVE_LEN,
        SOLEND_SUPPORTED_VERSION, SPL_TOKEN_ACCOUNT_LEN, SPL_TOKEN_OFFSET_AMOUNT,
        SPL_TOKEN_OFFSET_MINT, SPL_TOKEN_OFFSET_OWNER,
    };
    use crate::state::{Stage2ActionType, STAGE2_AUTHORITY_SCHEMA_VERSION};

    fn pk(b: u8) -> Pubkey {
        Pubkey::new_from_array([b; 32])
    }

    fn record_fixture() -> AuthorizationRecord {
        AuthorizationRecord {
            schema_version: STAGE2_AUTHORITY_SCHEMA_VERSION,
            rule_id: [0xD0; 16],
            user: pk(0xAA),
            executor: pk(0xEE),
            delegated_wallet: pk(0xBB),
            canonical_rule_hash: [0; 32],
            allowed_action_type: Stage2ActionType::SolendWithdrawAllDelegated.to_u8(),
            max_input_amount_raw: 5_000_000,
            used_amount_raw: 0,
            destination: pk(0xCC),
            expires_at_slot: 1_000_000,
            revoked: false,
            completed: false,
            execution_nonce: 0,
            last_execution_slot: 0,
            bump: 255,
        }
    }

    fn populate_obligation_prefix(
        data: &mut [u8],
        slot: u64,
        stale: bool,
        lending_market: [u8; 32],
        inner_owner: [u8; 32],
    ) {
        data[OBLIGATION_OFFSET_VERSION] = SOLEND_SUPPORTED_VERSION;
        data[OBLIGATION_OFFSET_LAST_UPDATE_SLOT..OBLIGATION_OFFSET_LAST_UPDATE_SLOT + 8]
            .copy_from_slice(&slot.to_le_bytes());
        data[OBLIGATION_OFFSET_LAST_UPDATE_STALE] = if stale { 1 } else { 0 };
        data[OBLIGATION_OFFSET_LENDING_MARKET..OBLIGATION_OFFSET_LENDING_MARKET + 32]
            .copy_from_slice(&lending_market);
        data[OBLIGATION_OFFSET_OWNER..OBLIGATION_OFFSET_OWNER + 32]
            .copy_from_slice(&inner_owner);
    }

    fn populate_reserve_prefix(
        data: &mut [u8],
        slot: u64,
        stale: bool,
        lending_market: [u8; 32],
    ) {
        data[RESERVE_OFFSET_VERSION] = SOLEND_SUPPORTED_VERSION;
        data[RESERVE_OFFSET_LAST_UPDATE_SLOT..RESERVE_OFFSET_LAST_UPDATE_SLOT + 8]
            .copy_from_slice(&slot.to_le_bytes());
        data[RESERVE_OFFSET_LAST_UPDATE_STALE] = if stale { 1 } else { 0 };
        data[RESERVE_OFFSET_LENDING_MARKET..RESERVE_OFFSET_LENDING_MARKET + 32]
            .copy_from_slice(&lending_market);
    }

    fn populate_spl_token(data: &mut [u8], mint: [u8; 32], owner: [u8; 32], amount: u64) {
        data[SPL_TOKEN_OFFSET_MINT..SPL_TOKEN_OFFSET_MINT + 32].copy_from_slice(&mint);
        data[SPL_TOKEN_OFFSET_OWNER..SPL_TOKEN_OFFSET_OWNER + 32].copy_from_slice(&owner);
        data[SPL_TOKEN_OFFSET_AMOUNT..SPL_TOKEN_OFFSET_AMOUNT + 8]
            .copy_from_slice(&amount.to_le_bytes());
    }

    fn assert_err(actual: ProgramError, expected: AuthorityError) {
        assert_eq!(
            actual,
            ProgramError::Custom(expected as u32),
            "expected {expected:?} (code {})",
            expected as u32,
        );
    }

    /// Storage backing for a synthetic `AccountInfo` — we keep all the
    /// fields the test needs to mutate on the heap so each AccountInfo
    /// can hold exclusive borrows.
    struct AcctStore {
        key: Pubkey,
        owner: Pubkey,
        lamports: u64,
        data: Vec<u8>,
        is_signer: bool,
    }

    impl AcctStore {
        fn obligation(record: &AuthorizationRecord, lm: [u8; 32]) -> Self {
            let mut data = vec![0u8; SOLEND_OBLIGATION_LEN];
            populate_obligation_prefix(
                &mut data,
                100,
                false,
                lm,
                record.delegated_wallet.to_bytes(),
            );
            Self {
                key: pk(0x11),
                owner: SOLEND_PROGRAM_ID_MAINNET,
                lamports: 0,
                data,
                is_signer: false,
            }
        }

        fn reserve(lm: [u8; 32]) -> Self {
            let mut data = vec![0u8; SOLEND_RESERVE_LEN];
            populate_reserve_prefix(&mut data, 100, false, lm);
            Self {
                key: pk(0x10),
                owner: SOLEND_PROGRAM_ID_MAINNET,
                lamports: 0,
                data,
                is_signer: false,
            }
        }

        fn token(key_byte: u8, mint: [u8; 32], authority: [u8; 32], amount: u64) -> Self {
            let mut data = vec![0u8; SPL_TOKEN_ACCOUNT_LEN];
            populate_spl_token(&mut data, mint, authority, amount);
            Self {
                key: pk(key_byte),
                owner: SPL_TOKEN_PROGRAM_ID,
                lamports: 0,
                data,
                is_signer: false,
            }
        }

        fn bare(key: Pubkey, owner: Pubkey) -> Self {
            Self {
                key,
                owner,
                lamports: 0,
                data: vec![],
                is_signer: false,
            }
        }

        fn signer(key: Pubkey) -> Self {
            Self {
                key,
                owner: Pubkey::default(),
                lamports: 0,
                data: vec![],
                is_signer: true,
            }
        }
    }

    /// Macro-light wrapper to spin a `&[AccountInfo]` from a
    /// `&mut [AcctStore]`. AccountInfo references inside the returned
    /// Vec are bound to `s`'s lifetime.
    fn build_accounts<'a>(stores: &'a mut [AcctStore]) -> Vec<AccountInfo<'a>> {
        stores
            .iter_mut()
            .map(|s| {
                AccountInfo::new(
                    &s.key,
                    s.is_signer,
                    s.owner != Pubkey::default()
                        && s.owner != SOLEND_PROGRAM_ID_MAINNET
                        && s.owner != SPL_TOKEN_PROGRAM_ID,
                    &mut s.lamports,
                    &mut s.data,
                    &s.owner,
                    false,
                    0,
                )
            })
            .collect()
    }

    /// Build the canonical "happy path" 12-account base array for
    /// variant-15 shadow-mapping tests.
    ///
    /// `lm` = lending market bytes, `ctoken_mint` = expected cToken
    /// mint, `liquidity_mint` = expected destination mint, `record` =
    /// the AuthorizationRecord.
    fn good_withdraw_stores(
        record: &AuthorizationRecord,
        lm: [u8; 32],
        ctoken_mint: [u8; 32],
        liquidity_mint: [u8; 32],
        ctoken_amount: u64,
    ) -> Vec<AcctStore> {
        vec![
            // 0 — source_collateral (any token-program-owned account).
            AcctStore::token(0x20, ctoken_mint, [0xFA; 32], 0),
            // 1 — delegated_ctoken: authority = delegated_wallet, mint
            //    = ctoken_mint.
            AcctStore::token(
                0x21,
                ctoken_mint,
                record.delegated_wallet.to_bytes(),
                ctoken_amount,
            ),
            // 2 — withdraw reserve.
            AcctStore::reserve(lm),
            // 3 — obligation (inner owner = delegated_wallet).
            AcctStore::obligation(record, lm),
            // 4 — lending market (key = lm).
            AcctStore::bare(Pubkey::new_from_array(lm), pk(0xFE)),
            // 5 — lending market authority (PDA-shaped; its bytes
            //    don't matter for the shadow-mapping shape tests).
            AcctStore::bare(pk(0x40), pk(0xFE)),
            // 6 — destination = record.destination, mint = liquidity_mint.
            AcctStore {
                key: record.destination,
                owner: SPL_TOKEN_PROGRAM_ID,
                lamports: 0,
                data: {
                    let mut d = vec![0u8; SPL_TOKEN_ACCOUNT_LEN];
                    populate_spl_token(&mut d, liquidity_mint, record.user.to_bytes(), 0);
                    d
                },
                is_signer: false,
            },
            // 7 — reserve_collateral_mint (= ctoken_mint, but here we
            //    only need it as a structural slot).
            AcctStore::bare(Pubkey::new_from_array(ctoken_mint), pk(0xFE)),
            // 8 — reserve_liquidity_supply.
            AcctStore::bare(pk(0x80), pk(0xFE)),
            // 9 — obligation_owner (= delegated_wallet, signer).
            AcctStore::signer(record.delegated_wallet),
            // 10 — user_transfer_authority (= delegated_wallet, signer).
            AcctStore::signer(record.delegated_wallet),
            // 11 — token program.
            AcctStore::bare(SPL_TOKEN_PROGRAM_ID, pk(0xFE)),
        ]
    }

    fn expectations<'a>(
        lm: &'a Pubkey,
        ctoken_mint: &'a Pubkey,
        liquidity_mint: &'a Pubkey,
        obligation: &'a Pubkey,
        reserve: &'a Pubkey,
    ) -> SolendWithdrawShadowExpectations<'a> {
        SolendWithdrawShadowExpectations {
            solend_program_id: &SOLEND_PROGRAM_ID_MAINNET,
            expected_obligation: obligation,
            expected_reserve: reserve,
            expected_lending_market: lm,
            expected_ctoken_mint: ctoken_mint,
            expected_liquidity_mint: liquidity_mint,
        }
    }

    // ── 1-3: Instruction-data shape ───────────────────────────────────────

    #[test]
    fn refresh_reserve_instruction_data_matches_official_variant() {
        // Source: token-lending/sdk/src/instruction.rs pack() emits a
        // single byte 3 for RefreshReserve.
        assert_eq!(build_refresh_reserve_data(), vec![3u8]);
        assert_eq!(SOLEND_VARIANT_REFRESH_RESERVE, 3);
    }

    #[test]
    fn refresh_obligation_instruction_data_matches_official_variant() {
        // Source: token-lending/sdk/src/instruction.rs pack() emits a
        // single byte 7 for RefreshObligation.
        assert_eq!(build_refresh_obligation_data(), vec![7u8]);
        assert_eq!(SOLEND_VARIANT_REFRESH_OBLIGATION, 7);
    }

    #[test]
    fn withdraw_instruction_data_matches_variant_15_plus_u64_le_amount() {
        // Source: token-lending/sdk/src/instruction.rs pack() emits
        //   buf.push(15);
        //   buf.extend_from_slice(&collateral_amount.to_le_bytes());
        let data = build_withdraw_obligation_collateral_and_redeem_reserve_collateral_data(
            1_000_000_000u64,
        );
        assert_eq!(data.len(), 9);
        assert_eq!(data[0], 15);
        assert_eq!(SOLEND_VARIANT_WITHDRAW_OBLIGATION_COLLATERAL_AND_REDEEM, 15);
        assert_eq!(&data[1..], &1_000_000_000u64.to_le_bytes());
    }

    #[test]
    fn withdraw_instruction_data_uses_little_endian_amount() {
        // 0xCAFEBABE_DEADBEEF anchors little-endian byte order.
        let amount: u64 = 0xCAFE_BABE_DEAD_BEEF;
        let data = build_withdraw_obligation_collateral_and_redeem_reserve_collateral_data(amount);
        assert_eq!(data[0], 15);
        assert_eq!(&data[1..], &amount.to_le_bytes());
        // Big-endian sanity check — the BE bytes would NOT match.
        assert_ne!(&data[1..], &amount.to_be_bytes());
        // Spot-check the LE byte order — least significant byte first.
        assert_eq!(data[1], 0xEF);
        assert_eq!(data[8], 0xCA);
    }

    // ── 5-7: AccountMeta order ────────────────────────────────────────────

    #[test]
    fn refresh_reserve_account_metas_match_official_order() {
        // Source: token-lending/sdk/src/instruction.rs refresh_reserve()
        // — reserve(W), pyth(R), switchboard(R), [+ extra_oracle(R)],
        // plus the deployed-mainnet clock sysvar slot at index 3
        // (Phase 6I-H live validation, see module docs).
        let reserve = pk(0x10);
        let pyth = pk(0x11);
        let switch = pk(0x12);
        let clock = sysvar::clock::id();
        let metas = build_refresh_reserve_metas(reserve, pyth, switch, clock, None);
        assert_eq!(metas.len(), 4);
        assert_eq!(metas[0].pubkey, reserve);
        assert_eq!(metas[1].pubkey, pyth);
        assert_eq!(metas[2].pubkey, switch);
        assert_eq!(metas[3].pubkey, clock);

        // With extra_oracle present.
        let extra = pk(0x13);
        let metas =
            build_refresh_reserve_metas(reserve, pyth, switch, clock, Some(extra));
        assert_eq!(metas.len(), 5);
        assert_eq!(metas[4].pubkey, extra);
    }

    #[test]
    fn refresh_obligation_account_metas_match_official_order() {
        // Source: token-lending/sdk/src/instruction.rs refresh_obligation()
        // — obligation(W) followed by N reserves (each writable).
        let obligation = pk(0x11);
        let r1 = pk(0x20);
        let r2 = pk(0x21);
        let metas = build_refresh_obligation_metas(obligation, &[r1, r2]);
        assert_eq!(metas.len(), 3);
        assert_eq!(metas[0].pubkey, obligation);
        assert_eq!(metas[1].pubkey, r1);
        assert_eq!(metas[2].pubkey, r2);
    }

    #[test]
    fn withdraw_account_metas_match_official_order() {
        // Source: token-lending/sdk/src/instruction.rs
        // withdraw_obligation_collateral_and_redeem_reserve_collateral()
        // 12 base accounts in pinned order, then deposit_reserves tail.
        let source_collateral = pk(0x20);
        let delegated_ctoken = pk(0x21);
        let withdraw_reserve = pk(0x10);
        let obligation = pk(0x11);
        let lending_market = pk(0x01);
        let lma = pk(0x40);
        let dest = pk(0xCC);
        let collateral_mint = pk(0x55);
        let liq_supply = pk(0x80);
        let owner = pk(0xBB);
        let xferauth = pk(0xBB);
        let token_prog = SPL_TOKEN_PROGRAM_ID;
        let dep1 = pk(0x10);
        let metas = build_withdraw_obligation_collateral_and_redeem_reserve_collateral_metas(
            source_collateral,
            delegated_ctoken,
            withdraw_reserve,
            obligation,
            lending_market,
            lma,
            dest,
            collateral_mint,
            liq_supply,
            owner,
            xferauth,
            token_prog,
            &[dep1],
        );
        assert_eq!(metas.len(), 13);
        assert_eq!(metas[0].pubkey, source_collateral);
        assert_eq!(metas[1].pubkey, delegated_ctoken);
        assert_eq!(metas[2].pubkey, withdraw_reserve);
        assert_eq!(metas[3].pubkey, obligation);
        assert_eq!(metas[4].pubkey, lending_market);
        assert_eq!(metas[5].pubkey, lma);
        assert_eq!(metas[6].pubkey, dest);
        assert_eq!(metas[7].pubkey, collateral_mint);
        assert_eq!(metas[8].pubkey, liq_supply);
        assert_eq!(metas[9].pubkey, owner);
        assert_eq!(metas[10].pubkey, xferauth);
        assert_eq!(metas[11].pubkey, token_prog);
        assert_eq!(metas[12].pubkey, dep1);
    }

    // ── 8-9: Writable / signer flags pinned ───────────────────────────────

    #[test]
    fn account_metas_pin_writable_flags() {
        // RefreshReserve: only reserve is writable.
        let metas = build_refresh_reserve_metas(
            pk(0x10),
            pk(0x11),
            pk(0x12),
            sysvar::clock::id(),
            None,
        );
        assert!(metas[0].is_writable, "reserve must be writable");
        assert!(!metas[1].is_writable, "pyth oracle must be readonly");
        assert!(!metas[2].is_writable, "switchboard oracle must be readonly");
        assert!(!metas[3].is_writable, "clock sysvar must NEVER be writable");

        // RefreshObligation: every account is writable.
        let metas = build_refresh_obligation_metas(pk(0x11), &[pk(0x20), pk(0x21)]);
        for (i, m) in metas.iter().enumerate() {
            assert!(m.is_writable, "RefreshObligation slot {i} must be writable");
        }

        // Withdraw: writable matrix per Phase 6I-J pin.
        let metas = build_withdraw_obligation_collateral_and_redeem_reserve_collateral_metas(
            pk(0x20),
            pk(0x21),
            pk(0x10),
            pk(0x11),
            pk(0x01),
            pk(0x40),
            pk(0xCC),
            pk(0x55),
            pk(0x80),
            pk(0xBB),
            pk(0xBB),
            SPL_TOKEN_PROGRAM_ID,
            &[pk(0x10)],
        );
        // Writable: 0,1,2,3,4,6,7,8 + 12..
        assert!(metas[0].is_writable, "source_collateral writable");
        assert!(metas[1].is_writable, "delegated_ctoken writable");
        assert!(metas[2].is_writable, "reserve writable");
        assert!(metas[3].is_writable, "obligation writable");
        assert!(metas[4].is_writable, "lending_market writable");
        assert!(!metas[5].is_writable, "lending_market_authority readonly");
        assert!(metas[6].is_writable, "destination_liquidity writable");
        assert!(metas[7].is_writable, "reserve_collateral_mint writable");
        assert!(metas[8].is_writable, "reserve_liquidity_supply writable");
        assert!(!metas[9].is_writable, "obligation_owner readonly");
        assert!(
            !metas[10].is_writable,
            "user_transfer_authority readonly"
        );
        assert!(!metas[11].is_writable, "token_program readonly");
        assert!(metas[12].is_writable, "deposit_reserves[0] writable");
    }

    #[test]
    fn account_metas_pin_signer_flags() {
        // RefreshReserve / RefreshObligation: NO signers.
        let metas = build_refresh_reserve_metas(
            pk(0x10),
            pk(0x11),
            pk(0x12),
            sysvar::clock::id(),
            None,
        );
        for (i, m) in metas.iter().enumerate() {
            assert!(!m.is_signer, "RefreshReserve slot {i} must NOT be signer");
        }

        let metas = build_refresh_obligation_metas(pk(0x11), &[pk(0x20)]);
        for (i, m) in metas.iter().enumerate() {
            assert!(
                !m.is_signer,
                "RefreshObligation slot {i} must NOT be signer"
            );
        }

        // Withdraw: only slots 9 and 10 are signers.
        let metas = build_withdraw_obligation_collateral_and_redeem_reserve_collateral_metas(
            pk(0x20),
            pk(0x21),
            pk(0x10),
            pk(0x11),
            pk(0x01),
            pk(0x40),
            pk(0xCC),
            pk(0x55),
            pk(0x80),
            pk(0xBB),
            pk(0xBB),
            SPL_TOKEN_PROGRAM_ID,
            &[],
        );
        for (i, m) in metas.iter().enumerate() {
            let expected_signer = i == 9 || i == 10;
            assert_eq!(
                m.is_signer, expected_signer,
                "Withdraw slot {i} signer flag",
            );
        }
    }

    // ── 10: User main wallet never appears as signer ──────────────────────

    #[test]
    fn no_user_main_wallet_signer_in_any_builder() {
        let record = record_fixture();

        // RefreshReserve: no signers, so no main-wallet slot to begin with.
        let metas = build_refresh_reserve_metas(
            pk(0x10),
            pk(0x11),
            pk(0x12),
            sysvar::clock::id(),
            None,
        );
        for m in metas.iter() {
            if m.is_signer {
                assert_ne!(
                    m.pubkey, record.user,
                    "user main wallet must never be signer in RefreshReserve",
                );
            }
        }

        // RefreshObligation: no signers either.
        let metas = build_refresh_obligation_metas(pk(0x11), &[pk(0x20), pk(0x21)]);
        for m in metas.iter() {
            assert!(!m.is_signer);
        }

        // Withdraw: even if a malicious caller tried to put the user
        // main wallet in slot 9 / 10, the Stage 2 invariant is that
        // the builder's signer slots get the delegated wallet — and
        // the shadow-mapping validator additionally rejects the user
        // main wallet at slots 9 / 10. Here we just assert that
        // building with delegated_wallet (the canonical Stage 2 path)
        // never produces metas[9..=10].pubkey == record.user.
        let metas = build_withdraw_obligation_collateral_and_redeem_reserve_collateral_metas(
            pk(0x20),
            pk(0x21),
            pk(0x10),
            pk(0x11),
            pk(0x01),
            pk(0x40),
            record.destination,
            pk(0x55),
            pk(0x80),
            record.delegated_wallet,
            record.delegated_wallet,
            SPL_TOKEN_PROGRAM_ID,
            &[],
        );
        for (i, m) in metas.iter().enumerate() {
            if m.is_signer {
                assert_eq!(
                    m.pubkey, record.delegated_wallet,
                    "Withdraw signer slot {i} must be delegated_wallet",
                );
                assert_ne!(
                    m.pubkey, record.user,
                    "Withdraw signer slot {i} must NEVER be user main wallet",
                );
            }
        }
    }

    // ── 11-13: Sysvar / token program defence ─────────────────────────────

    #[test]
    fn fake_clock_sysvar_rejected() {
        // require_canonical_clock_sysvar fails closed on a non-canonical
        // clock pubkey.
        let key = pk(0xFA);
        let mut lamports = 0u64;
        let mut data = vec![];
        let owner = Pubkey::default();
        let acct = AccountInfo::new(
            &key, false, false, &mut lamports, &mut data, &owner, false, 0,
        );
        let err = require_canonical_clock_sysvar(&acct).unwrap_err();
        assert_err(err, AuthorityError::SolendCpiSysvarMismatch);

        // Canonical clock sysvar passes.
        let canonical = sysvar::clock::id();
        let mut lamports2 = 0u64;
        let mut data2 = vec![];
        let acct2 = AccountInfo::new(
            &canonical,
            false,
            false,
            &mut lamports2,
            &mut data2,
            &owner,
            false,
            0,
        );
        require_canonical_clock_sysvar(&acct2).unwrap();
    }

    #[test]
    fn fake_rent_sysvar_rejected_if_rent_used() {
        // Rent sysvar is NOT used in any of P5b-1's three Solend
        // instructions (RefreshReserve, RefreshObligation, Withdraw).
        // The deployed mainnet processors all read Clock via syscall
        // and never consume a Rent account on these variants. So this
        // test asserts the negative invariant: NONE of the variant-15
        // base meta builders place rent::id() in any slot, AND the
        // shadow-mapping validator rejects rent::id() if it appears
        // in any of slots 0..=11.
        //
        // (The test name is preserved per spec to keep the gate
        // visible.)
        let metas = build_withdraw_obligation_collateral_and_redeem_reserve_collateral_metas(
            pk(0x20),
            pk(0x21),
            pk(0x10),
            pk(0x11),
            pk(0x01),
            pk(0x40),
            pk(0xCC),
            pk(0x55),
            pk(0x80),
            pk(0xBB),
            pk(0xBB),
            SPL_TOKEN_PROGRAM_ID,
            &[],
        );
        let rent = sysvar::rent::id();
        for (i, m) in metas.iter().enumerate() {
            assert_ne!(
                m.pubkey, rent,
                "Withdraw slot {i} must not be the rent sysvar (no rent in mainnet variant 15)",
            );
        }
        // RefreshReserve / RefreshObligation similarly.
        let r_metas = build_refresh_reserve_metas(
            pk(0x10),
            pk(0x11),
            pk(0x12),
            sysvar::clock::id(),
            None,
        );
        for m in r_metas.iter() {
            assert_ne!(m.pubkey, rent);
        }
        let o_metas = build_refresh_obligation_metas(pk(0x11), &[pk(0x20)]);
        for m in o_metas.iter() {
            assert_ne!(m.pubkey, rent);
        }

        // And: shadow-mapping validator rejects a base slot pretending
        // to be the rent sysvar.
        let record = record_fixture();
        let lm: [u8; 32] = [0x42; 32];
        let ctoken_mint: [u8; 32] = [0x55; 32];
        let liquidity_mint: [u8; 32] = [0x66; 32];
        let mut stores = good_withdraw_stores(&record, lm, ctoken_mint, liquidity_mint, 1_000);
        // Poison slot 8 (reserve_liquidity_supply) with the rent sysvar.
        stores[8].key = rent;
        let accounts = build_accounts(&mut stores);
        let parsed = SolendWithdrawAccounts::parse(&accounts).unwrap();
        let lm_pk = Pubkey::new_from_array(lm);
        let ctoken_pk = Pubkey::new_from_array(ctoken_mint);
        let liq_pk = Pubkey::new_from_array(liquidity_mint);
        let obligation_pk = pk(0x11);
        let reserve_pk = pk(0x10);
        let exp = expectations(&lm_pk, &ctoken_pk, &liq_pk, &obligation_pk, &reserve_pk);
        let err =
            validate_solend_withdraw_shadow_mapping(&parsed, &record, &exp).unwrap_err();
        assert_err(err, AuthorityError::SolendCpiSysvarMismatch);
    }

    #[test]
    fn fake_token_program_rejected() {
        // require_canonical_token_program rejects a non-canonical token
        // program pubkey.
        let key = pk(0xFA);
        let mut lamports = 0u64;
        let mut data = vec![];
        let owner = Pubkey::default();
        let acct = AccountInfo::new(
            &key, false, false, &mut lamports, &mut data, &owner, false, 0,
        );
        let err = require_canonical_token_program(&acct).unwrap_err();
        assert_err(err, AuthorityError::SolendCpiTokenProgramMismatch);

        // Canonical SPL Token program passes.
        let canonical = SPL_TOKEN_PROGRAM_ID;
        let mut lamports2 = 0u64;
        let mut data2 = vec![];
        let acct2 = AccountInfo::new(
            &canonical,
            false,
            false,
            &mut lamports2,
            &mut data2,
            &owner,
            false,
            0,
        );
        require_canonical_token_program(&acct2).unwrap();

        // And the shadow-mapping validator surfaces
        // SolendCpiTokenProgramMismatch when slot 11 isn't the SPL
        // Token program id.
        let record = record_fixture();
        let lm: [u8; 32] = [0x42; 32];
        let ctoken_mint: [u8; 32] = [0x55; 32];
        let liquidity_mint: [u8; 32] = [0x66; 32];
        let mut stores = good_withdraw_stores(&record, lm, ctoken_mint, liquidity_mint, 1_000);
        // Poison slot 11 — set the key to a fake token program.
        stores[11].key = pk(0x99);
        let accounts = build_accounts(&mut stores);
        let parsed = SolendWithdrawAccounts::parse(&accounts).unwrap();
        let lm_pk = Pubkey::new_from_array(lm);
        let ctoken_pk = Pubkey::new_from_array(ctoken_mint);
        let liq_pk = Pubkey::new_from_array(liquidity_mint);
        let obligation_pk = pk(0x11);
        let reserve_pk = pk(0x10);
        let exp = expectations(&lm_pk, &ctoken_pk, &liq_pk, &obligation_pk, &reserve_pk);
        let err =
            validate_solend_withdraw_shadow_mapping(&parsed, &record, &exp).unwrap_err();
        assert_err(err, AuthorityError::SolendCpiTokenProgramMismatch);
    }

    // ── 14-21: Shadow-mapping rejection tests ─────────────────────────────

    #[test]
    fn happy_path_shadow_mapping_passes() {
        let record = record_fixture();
        let lm: [u8; 32] = [0x42; 32];
        let ctoken_mint: [u8; 32] = [0x55; 32];
        let liquidity_mint: [u8; 32] = [0x66; 32];
        let mut stores = good_withdraw_stores(&record, lm, ctoken_mint, liquidity_mint, 1_000);
        let accounts = build_accounts(&mut stores);
        let parsed = SolendWithdrawAccounts::parse(&accounts).unwrap();
        let lm_pk = Pubkey::new_from_array(lm);
        let ctoken_pk = Pubkey::new_from_array(ctoken_mint);
        let liq_pk = Pubkey::new_from_array(liquidity_mint);
        let obligation_pk = pk(0x11);
        let reserve_pk = pk(0x10);
        let exp = expectations(&lm_pk, &ctoken_pk, &liq_pk, &obligation_pk, &reserve_pk);
        let decoded =
            validate_solend_withdraw_shadow_mapping(&parsed, &record, &exp).unwrap();
        assert_eq!(decoded.delegated_ctoken.amount, 1_000);
    }

    #[test]
    fn main_wallet_obligation_rejected_before_builder() {
        let record = record_fixture();
        let lm: [u8; 32] = [0x42; 32];
        let ctoken_mint: [u8; 32] = [0x55; 32];
        let liquidity_mint: [u8; 32] = [0x66; 32];
        let mut stores = good_withdraw_stores(&record, lm, ctoken_mint, liquidity_mint, 1_000);
        // Poison: rewrite the obligation's inner owner to record.user.
        let obligation_data = &mut stores[3].data;
        obligation_data[OBLIGATION_OFFSET_OWNER..OBLIGATION_OFFSET_OWNER + 32]
            .copy_from_slice(&record.user.to_bytes());
        let accounts = build_accounts(&mut stores);
        let parsed = SolendWithdrawAccounts::parse(&accounts).unwrap();
        let lm_pk = Pubkey::new_from_array(lm);
        let ctoken_pk = Pubkey::new_from_array(ctoken_mint);
        let liq_pk = Pubkey::new_from_array(liquidity_mint);
        let obligation_pk = pk(0x11);
        let reserve_pk = pk(0x10);
        let exp = expectations(&lm_pk, &ctoken_pk, &liq_pk, &obligation_pk, &reserve_pk);
        let err =
            validate_solend_withdraw_shadow_mapping(&parsed, &record, &exp).unwrap_err();
        assert_err(err, AuthorityError::SolendMainWalletObligationRejected);
    }

    #[test]
    fn wrong_reserve_rejected_before_builder() {
        let record = record_fixture();
        let lm: [u8; 32] = [0x42; 32];
        let ctoken_mint: [u8; 32] = [0x55; 32];
        let liquidity_mint: [u8; 32] = [0x66; 32];
        let mut stores = good_withdraw_stores(&record, lm, ctoken_mint, liquidity_mint, 1_000);
        // Poison: change the reserve account's key from the expected.
        stores[2].key = pk(0x91); // not 0x10 (the canonical good_withdraw_stores reserve key)
        let accounts = build_accounts(&mut stores);
        let parsed = SolendWithdrawAccounts::parse(&accounts).unwrap();
        let lm_pk = Pubkey::new_from_array(lm);
        let ctoken_pk = Pubkey::new_from_array(ctoken_mint);
        let liq_pk = Pubkey::new_from_array(liquidity_mint);
        let obligation_pk = pk(0x11);
        let reserve_pk = pk(0x10);
        let exp = expectations(&lm_pk, &ctoken_pk, &liq_pk, &obligation_pk, &reserve_pk);
        let err =
            validate_solend_withdraw_shadow_mapping(&parsed, &record, &exp).unwrap_err();
        assert_err(err, AuthorityError::SolendReserveMismatch);
    }

    #[test]
    fn wrong_lending_market_rejected_before_builder() {
        let record = record_fixture();
        let lm: [u8; 32] = [0x42; 32];
        let ctoken_mint: [u8; 32] = [0x55; 32];
        let liquidity_mint: [u8; 32] = [0x66; 32];
        let mut stores = good_withdraw_stores(&record, lm, ctoken_mint, liquidity_mint, 1_000);
        // Poison: set the obligation's inner lending_market to a
        // value that doesn't match the expected (we keep the reserve's
        // inner LM matching, so the obligation gate fires first).
        let bogus_lm: [u8; 32] = [0x77; 32];
        stores[3].data[OBLIGATION_OFFSET_LENDING_MARKET
            ..OBLIGATION_OFFSET_LENDING_MARKET + 32]
            .copy_from_slice(&bogus_lm);
        let accounts = build_accounts(&mut stores);
        let parsed = SolendWithdrawAccounts::parse(&accounts).unwrap();
        let lm_pk = Pubkey::new_from_array(lm);
        let ctoken_pk = Pubkey::new_from_array(ctoken_mint);
        let liq_pk = Pubkey::new_from_array(liquidity_mint);
        let obligation_pk = pk(0x11);
        let reserve_pk = pk(0x10);
        let exp = expectations(&lm_pk, &ctoken_pk, &liq_pk, &obligation_pk, &reserve_pk);
        let err =
            validate_solend_withdraw_shadow_mapping(&parsed, &record, &exp).unwrap_err();
        assert_err(err, AuthorityError::SolendObligationLendingMarketMismatch);
    }

    #[test]
    fn wrong_ctoken_mint_rejected_before_builder() {
        let record = record_fixture();
        let lm: [u8; 32] = [0x42; 32];
        let ctoken_mint: [u8; 32] = [0x55; 32];
        let liquidity_mint: [u8; 32] = [0x66; 32];
        let mut stores = good_withdraw_stores(&record, lm, ctoken_mint, liquidity_mint, 1_000);
        // Poison: rewrite the delegated cToken account's mint to something
        // other than the expected ctoken_mint.
        stores[1].data[SPL_TOKEN_OFFSET_MINT..SPL_TOKEN_OFFSET_MINT + 32]
            .copy_from_slice(&[0x99; 32]);
        let accounts = build_accounts(&mut stores);
        let parsed = SolendWithdrawAccounts::parse(&accounts).unwrap();
        let lm_pk = Pubkey::new_from_array(lm);
        let ctoken_pk = Pubkey::new_from_array(ctoken_mint);
        let liq_pk = Pubkey::new_from_array(liquidity_mint);
        let obligation_pk = pk(0x11);
        let reserve_pk = pk(0x10);
        let exp = expectations(&lm_pk, &ctoken_pk, &liq_pk, &obligation_pk, &reserve_pk);
        let err =
            validate_solend_withdraw_shadow_mapping(&parsed, &record, &exp).unwrap_err();
        assert_err(err, AuthorityError::SolendTokenAccountMintMismatch);
    }

    #[test]
    fn wrong_destination_mint_rejected_before_builder() {
        let record = record_fixture();
        let lm: [u8; 32] = [0x42; 32];
        let ctoken_mint: [u8; 32] = [0x55; 32];
        let liquidity_mint: [u8; 32] = [0x66; 32];
        let mut stores = good_withdraw_stores(&record, lm, ctoken_mint, liquidity_mint, 1_000);
        // Poison: rewrite destination's mint.
        stores[6].data[SPL_TOKEN_OFFSET_MINT..SPL_TOKEN_OFFSET_MINT + 32]
            .copy_from_slice(&[0xAB; 32]);
        let accounts = build_accounts(&mut stores);
        let parsed = SolendWithdrawAccounts::parse(&accounts).unwrap();
        let lm_pk = Pubkey::new_from_array(lm);
        let ctoken_pk = Pubkey::new_from_array(ctoken_mint);
        let liq_pk = Pubkey::new_from_array(liquidity_mint);
        let obligation_pk = pk(0x11);
        let reserve_pk = pk(0x10);
        let exp = expectations(&lm_pk, &ctoken_pk, &liq_pk, &obligation_pk, &reserve_pk);
        let err =
            validate_solend_withdraw_shadow_mapping(&parsed, &record, &exp).unwrap_err();
        assert_err(err, AuthorityError::SolendTokenAccountMintMismatch);
    }

    #[test]
    fn wrong_ctoken_authority_rejected_before_builder() {
        let record = record_fixture();
        let lm: [u8; 32] = [0x42; 32];
        let ctoken_mint: [u8; 32] = [0x55; 32];
        let liquidity_mint: [u8; 32] = [0x66; 32];
        let mut stores = good_withdraw_stores(&record, lm, ctoken_mint, liquidity_mint, 1_000);
        // Poison: rewrite the delegated cToken authority to NOT be
        // delegated_wallet (try a random third-party wallet, NOT the
        // user main wallet — the main-wallet case is a separate
        // pre-existing P5a poison gate that fires earlier).
        stores[1].data[SPL_TOKEN_OFFSET_OWNER..SPL_TOKEN_OFFSET_OWNER + 32]
            .copy_from_slice(&[0x77; 32]);
        let accounts = build_accounts(&mut stores);
        let parsed = SolendWithdrawAccounts::parse(&accounts).unwrap();
        let lm_pk = Pubkey::new_from_array(lm);
        let ctoken_pk = Pubkey::new_from_array(ctoken_mint);
        let liq_pk = Pubkey::new_from_array(liquidity_mint);
        let obligation_pk = pk(0x11);
        let reserve_pk = pk(0x10);
        let exp = expectations(&lm_pk, &ctoken_pk, &liq_pk, &obligation_pk, &reserve_pk);
        let err =
            validate_solend_withdraw_shadow_mapping(&parsed, &record, &exp).unwrap_err();
        assert_err(err, AuthorityError::SolendTokenAccountAuthorityMismatch);
    }

    #[test]
    fn wrong_destination_account_rejected_before_builder() {
        let record = record_fixture();
        let lm: [u8; 32] = [0x42; 32];
        let ctoken_mint: [u8; 32] = [0x55; 32];
        let liquidity_mint: [u8; 32] = [0x66; 32];
        let mut stores = good_withdraw_stores(&record, lm, ctoken_mint, liquidity_mint, 1_000);
        // Poison: rewrite destination's KEY (account-info pubkey) to
        // something other than record.destination.
        stores[6].key = pk(0x44);
        let accounts = build_accounts(&mut stores);
        let parsed = SolendWithdrawAccounts::parse(&accounts).unwrap();
        let lm_pk = Pubkey::new_from_array(lm);
        let ctoken_pk = Pubkey::new_from_array(ctoken_mint);
        let liq_pk = Pubkey::new_from_array(liquidity_mint);
        let obligation_pk = pk(0x11);
        let reserve_pk = pk(0x10);
        let exp = expectations(&lm_pk, &ctoken_pk, &liq_pk, &obligation_pk, &reserve_pk);
        let err =
            validate_solend_withdraw_shadow_mapping(&parsed, &record, &exp).unwrap_err();
        assert_err(err, AuthorityError::SolendDestinationMismatch);
    }

    #[test]
    fn wrong_solend_program_owner_rejected_before_builder() {
        let record = record_fixture();
        let lm: [u8; 32] = [0x42; 32];
        let ctoken_mint: [u8; 32] = [0x55; 32];
        let liquidity_mint: [u8; 32] = [0x66; 32];
        let mut stores = good_withdraw_stores(&record, lm, ctoken_mint, liquidity_mint, 1_000);
        // Poison: rewrite the obligation account's Solana account-level
        // owner to something other than the Solend program.
        stores[3].owner = pk(0xDE);
        let accounts = build_accounts(&mut stores);
        let parsed = SolendWithdrawAccounts::parse(&accounts).unwrap();
        let lm_pk = Pubkey::new_from_array(lm);
        let ctoken_pk = Pubkey::new_from_array(ctoken_mint);
        let liq_pk = Pubkey::new_from_array(liquidity_mint);
        let obligation_pk = pk(0x11);
        let reserve_pk = pk(0x10);
        let exp = expectations(&lm_pk, &ctoken_pk, &liq_pk, &obligation_pk, &reserve_pk);
        let err =
            validate_solend_withdraw_shadow_mapping(&parsed, &record, &exp).unwrap_err();
        assert_err(err, AuthorityError::SolendAccountOwnerMismatch);
    }

    // ── 22-24: Withdraw-all amount semantics ──────────────────────────────

    #[test]
    fn withdraw_all_reads_live_ctoken_amount() {
        let record = record_fixture();
        let ctoken_mint = pk(0x55);
        let store = AcctStore::token(
            0x21,
            ctoken_mint.to_bytes(),
            record.delegated_wallet.to_bytes(),
            42_424_242,
        );
        let mut stores = vec![store];
        let accounts = build_accounts(&mut stores);
        let amt = read_live_ctoken_amount_or_fail_closed(
            &accounts[0],
            &record,
            &ctoken_mint,
        )
        .unwrap();
        assert_eq!(amt, 42_424_242);
        // And the data builder picks up the same number.
        let data = build_withdraw_obligation_collateral_and_redeem_reserve_collateral_data(amt);
        assert_eq!(&data[1..], &42_424_242u64.to_le_bytes());
    }

    #[test]
    fn zero_ctoken_amount_rejected_before_builder() {
        let record = record_fixture();
        let ctoken_mint = pk(0x55);
        let mut stores = vec![AcctStore::token(
            0x21,
            ctoken_mint.to_bytes(),
            record.delegated_wallet.to_bytes(),
            0, // zero
        )];
        let accounts = build_accounts(&mut stores);
        let err = read_live_ctoken_amount_or_fail_closed(
            &accounts[0],
            &record,
            &ctoken_mint,
        )
        .unwrap_err();
        assert_err(err, AuthorityError::SolendCpiZeroCollateralAmount);
    }

    /// Build the "executable production" view of this file: the
    /// section before the `#[cfg(test)]` test-module marker, with
    /// every comment line stripped (so module docs / inline comments
    /// that legitimately mention forbidden patterns when explaining
    /// WHY they're forbidden don't trip the source-grep tests).
    ///
    /// A "comment line" is any line whose first non-whitespace
    /// character starts a `//` (covers `//`, `///`, `//!`).
    fn production_executable_source() -> String {
        // Normalize CRLF → LF before scanning so the marker split below
        // works on Windows checkouts (where `core.autocrlf` rewrites
        // line endings to CRLF on the working tree). Without this, the
        // `\n`-only marker fails to match, the function falls back to
        // the whole file, and the self-grep tests find their own
        // pattern strings inside the test section.
        let src_raw = include_str!("solend_cpi_builder.rs");
        let src = src_raw.replace("\r\n", "\n");
        let test_marker = "#[cfg(test)]\nmod tests";
        let prod_section = match src.find(test_marker) {
            Some(idx) => &src[..idx],
            None => &src[..],
        };
        prod_section
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn no_close_account_instruction_emitted() {
        // Module-level invariant: production EXECUTABLE code (i.e. not
        // doc comments) MUST NOT reference the SPL Token close-account
        // variant in any form. Doc comments may legitimately discuss
        // the variant when explaining the no-close-account policy, so
        // we strip comment lines via `production_executable_source()`.
        //
        // Patterns are built via char-concat so the test source itself
        // does not contain a self-triggering literal.
        let prod = production_executable_source();
        let pat_capital = ['C', 'l', 'o', 's', 'e', 'A', 'c', 'c', 'o', 'u', 'n', 't']
            .iter()
            .collect::<String>();
        assert!(
            !prod.contains(&pat_capital),
            "P5b-1 production executable code must not reference the close-account variant",
        );
        let pat_snake = ['c', 'l', 'o', 's', 'e', '_', 'a', 'c', 'c', 'o', 'u', 'n', 't']
            .iter()
            .collect::<String>();
        assert!(
            !prod.contains(&pat_snake),
            "P5b-1 production executable code must not call the SPL Token close-account builder",
        );
    }

    // ── 25-27: Signer / bump policy and source-grep guards ────────────────

    #[test]
    fn signer_model_is_documented_and_user_main_wallet_never_signs() {
        // Source-text assertion: the module-level docs MUST contain
        // the documented signer/bump policy section so future readers
        // see the P5b-2 prerequisite explicitly. We grep for the
        // landmark sentence.
        //
        // Normalize CRLF → LF so the multiline grep below works on
        // Windows checkouts (where `core.autocrlf` rewrites line
        // endings to CRLF in the working tree).
        let src = include_str!("solend_cpi_builder.rs").replace("\r\n", "\n");
        assert!(
            src.contains("Signer / bump policy for P5b-2"),
            "module docs must carry the Signer / bump policy section",
        );
        assert!(
            src.contains("Under no circumstances is the user main\n//! wallet signed"),
            "module docs must explicitly call out user main wallet must never sign",
        );

        // Also: behavioural assertion. With a record whose user is
        // 0xAA and delegated_wallet 0xBB, the canonical builder for
        // Withdraw must produce metas[9..=10].pubkey == 0xBB and
        // is_signer == true; never 0xAA at a signer slot.
        let record = record_fixture();
        assert_ne!(record.user, record.delegated_wallet);
        let metas = build_withdraw_obligation_collateral_and_redeem_reserve_collateral_metas(
            pk(0x20),
            pk(0x21),
            pk(0x10),
            pk(0x11),
            pk(0x01),
            pk(0x40),
            record.destination,
            pk(0x55),
            pk(0x80),
            record.delegated_wallet,
            record.delegated_wallet,
            SPL_TOKEN_PROGRAM_ID,
            &[],
        );
        for (i, m) in metas.iter().enumerate() {
            if m.is_signer {
                assert_eq!(m.pubkey, record.delegated_wallet, "slot {i}");
                assert_ne!(m.pubkey, record.user, "slot {i}");
            }
        }
    }

    #[test]
    fn builder_does_not_call_find_program_address_hot_path() {
        // Source-grep test: production EXECUTABLE code (i.e. not doc
        // comments) MUST NOT contain `find_program_address`. Doc
        // comments may legitimately reference it when explaining why
        // the on-chain hot path delegates the PDA derivation to the
        // off-chain caller. The Solend SDK's own withdraw builder
        // derives `lending_market_authority` via `find_program_address`
        // — we do NOT, callers pass the PDA in.
        let prod = production_executable_source();
        let pat = ['f', 'i', 'n', 'd', '_', 'p', 'r', 'o', 'g', 'r', 'a', 'm', '_', 'a', 'd', 'd', 'r', 'e', 's', 's']
            .iter()
            .collect::<String>();
        assert!(
            !prod.contains(&pat),
            "find_program_address must not be called in production executable code",
        );
    }

    #[test]
    fn no_invoke_or_invoke_signed_in_p5b1_code() {
        // Source-grep test: production EXECUTABLE code MUST NOT contain
        // `invoke(` or `invoke_signed(`. P5b-2 will add these; P5b-1
        // strictly forbids them in production code. Doc comments may
        // legitimately reference these entrypoints when explaining the
        // P5b-1-vs-P5b-2 split, so we strip comment lines via
        // `production_executable_source()`.
        //
        // Patterns are built via char-concat so the test source itself
        // does not contain a self-triggering literal.
        let prod = production_executable_source();
        let pat_invoke =
            ['i', 'n', 'v', 'o', 'k', 'e', '('].iter().collect::<String>();
        let pat_invoke_signed = ['i', 'n', 'v', 'o', 'k', 'e', '_', 's', 'i', 'g', 'n', 'e', 'd', '(']
            .iter()
            .collect::<String>();
        assert!(
            !prod.contains(&pat_invoke),
            "P5b-1 production executable code must not call the invoke entrypoint",
        );
        assert!(
            !prod.contains(&pat_invoke_signed),
            "P5b-1 production executable code must not call the invoke_signed entrypoint",
        );
        // Additionally: no transaction-broadcast / RPC client surface in
        // production. Patterns are also char-concatenated to avoid
        // self-trigger.
        let pat_rpc_client =
            ['R', 'p', 'c', 'C', 'l', 'i', 'e', 'n', 't'].iter().collect::<String>();
        let pat_send_tx = [
            's', 'e', 'n', 'd', '_', 't', 'r', 'a', 'n', 's', 'a', 'c', 't', 'i', 'o', 'n',
        ]
        .iter()
        .collect::<String>();
        let pat_send_raw = [
            's', 'e', 'n', 'd', 'R', 'a', 'w', 'T', 'r', 'a', 'n', 's', 'a', 'c', 't', 'i', 'o', 'n',
        ]
        .iter()
        .collect::<String>();
        let pat_broadcast = ['b', 'r', 'o', 'a', 'd', 'c', 'a', 's', 't']
            .iter()
            .collect::<String>();
        assert!(!prod.contains(&pat_rpc_client));
        assert!(!prod.contains(&pat_send_tx));
        assert!(!prod.contains(&pat_send_raw));
        assert!(!prod.contains(&pat_broadcast));
    }

    // ── Oracle gap fail-closed gate ───────────────────────────────────────

    #[test]
    fn oracle_verification_always_fails_closed_in_p5b1() {
        // verify_oracles_or_fail_closed_p5b1 ALWAYS returns
        // SolendCpiOracleUnverified. The signature accepts AccountInfo
        // arguments so P5b-2 can swap in a real verifier without a wire
        // change.
        let key = pk(0xFA);
        let mut lamports = 0u64;
        let mut data = vec![];
        let owner = Pubkey::default();
        let pyth = AccountInfo::new(
            &key, false, false, &mut lamports, &mut data, &owner, false, 0,
        );
        let key2 = pk(0xFB);
        let mut lamports2 = 0u64;
        let mut data2 = vec![];
        let switch = AccountInfo::new(
            &key2, false, false, &mut lamports2, &mut data2, &owner, false, 0,
        );
        let reserve = pk(0x10);
        let err =
            verify_oracles_or_fail_closed_p5b1(&pyth, &switch, None, &reserve).unwrap_err();
        assert_err(err, AuthorityError::SolendCpiOracleUnverified);
    }

    // ── Parse error path ─────────────────────────────────────────────────

    #[test]
    fn parse_account_order_invalid_when_too_few_accounts() {
        let mut stores: Vec<AcctStore> = (0..11)
            .map(|i| AcctStore::bare(pk(i as u8), Pubkey::default()))
            .collect();
        let accounts = build_accounts(&mut stores);
        let err = SolendWithdrawAccounts::parse(&accounts).unwrap_err();
        assert_err(err, AuthorityError::SolendCpiAccountOrderInvalid);
    }

    #[test]
    fn ctoken_amount_helper_rejects_non_token_program_owner() {
        let record = record_fixture();
        let ctoken_mint = pk(0x55);
        // Build a token-shaped account whose AccountInfo.owner is NOT
        // the SPL Token program.
        let mut data = vec![0u8; SPL_TOKEN_ACCOUNT_LEN];
        populate_spl_token(
            &mut data,
            ctoken_mint.to_bytes(),
            record.delegated_wallet.to_bytes(),
            1_000,
        );
        let key = pk(0x21);
        let owner = pk(0xDE); // wrong owner program
        let mut lamports = 0u64;
        let acct = AccountInfo::new(
            &key, false, false, &mut lamports, &mut data, &owner, false, 0,
        );
        let err = read_live_ctoken_amount_or_fail_closed(&acct, &record, &ctoken_mint)
            .unwrap_err();
        assert_err(err, AuthorityError::SolendTokenAccountOwnerMismatch);
    }
}
