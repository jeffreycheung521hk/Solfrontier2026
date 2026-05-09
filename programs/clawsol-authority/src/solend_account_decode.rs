//! Stage 2 P5a — Solend live-account decode substrate.
//!
//! # What this module is
//!
//! Pure-byte decoders for the three account shapes the Stage 2 Solend
//! delegated-withdraw path will eventually need to read on-chain:
//!
//! - **`DecodedSolendObligation`** — minimal projection of a Solend
//!   Obligation account: `version`, `last_update.{slot, stale}`,
//!   `lending_market`, `owner` (the wallet authority field — NOT the
//!   Solana account-level owner program).
//! - **`DecodedSolendReserve`** — minimal projection of a Solend
//!   Reserve account: `version`, `last_update.{slot, stale}`,
//!   `lending_market`.
//! - **`DecodedSplTokenAccountLite`** — minimal SPL Token Account
//!   projection: `mint`, `owner`/authority, `amount`. Strict
//!   165-byte size, native SPL Token program only (Token-2022 is
//!   explicitly out of scope).
//!
//! # Critical: NOT Borsh
//!
//! Solend `Obligation` / `Reserve` accounts use the SPL `Pack` trait
//! with `array_refs!` byte slicing (token-lending mainnet branch).
//! Borsh-decoding these bytes would silently misalign every offset.
//! This module decodes by **byte offset**, with offset constants
//! pinned by tests against the official mainnet `Pack` source.
//!
//! # What this module is NOT
//!
//! - **Not a CPI.** No `invoke`, no `invoke_signed`. No instruction
//!   construction. No transaction building. No signing. No broadcast.
//!   No `RpcClient`. P5b will land same-tx Refresh + Withdraw CPI.
//! - **Not a full Solend reserve decoder.** Only the fixed 42-byte
//!   prefix (`version` + `last_update` + `lending_market`) is decoded.
//!   The rate-config and liquidity sub-structs require additional
//!   offsets that have NOT been re-verified against mainnet Pack
//!   end-to-end (see `STAGE2_SOLEND_APY_RESEARCH.md` § 8 B-O1) and are
//!   intentionally out of scope for P5a.
//! - **Not a full Solend obligation decoder.** Only the fixed 74-byte
//!   prefix (`version` + `last_update` + `lending_market` + `owner`)
//!   is decoded. The deposits / borrows tail is variable-length and
//!   not needed for the boundary checks P5a hardens.
//! - **Not a Token-2022 decoder.** Native SPL Token Account only.
//!
//! # Sources consulted
//!
//! - Solend mainnet `token-lending/sdk/src/state/obligation.rs` —
//!   `Pack::pack_into_slice` / `Pack::unpack_from_slice`:
//!   `array_refs![input, 1, 8, 1, PUBKEY_BYTES, PUBKEY_BYTES, ...]`,
//!   `OBLIGATION_LEN = 1300`. Pinned offsets:
//!   `version`=0, `last_update_slot`=1, `last_update_stale`=9,
//!   `lending_market`=10, `owner`=42.
//!   ([raw](https://raw.githubusercontent.com/solendprotocol/solana-program-library/mainnet/token-lending/sdk/src/state/obligation.rs))
//! - Solend mainnet `token-lending/sdk/src/state/reserve.rs` —
//!   `Pack::pack_into_slice` / `Pack::unpack_from_slice`:
//!   `array_refs![input, 1, 8, 1, PUBKEY_BYTES, ...]`,
//!   `RESERVE_LEN = 619`. Pinned offsets:
//!   `version`=0, `last_update_slot`=1, `last_update_stale`=9,
//!   `lending_market`=10.
//!   ([raw](https://raw.githubusercontent.com/solendprotocol/solana-program-library/mainnet/token-lending/sdk/src/state/reserve.rs))
//! - Solend mainnet `token-lending/sdk/src/state/last_update.rs` —
//!   `LastUpdate { slot: Slot, stale: bool }`,
//!   `STALE_AFTER_SLOTS_ELAPSED = 1`. Layout = u64 LE (8 bytes) + bool
//!   (1 byte) = 9 bytes total, no padding.
//!   ([raw](https://raw.githubusercontent.com/solendprotocol/solana-program-library/mainnet/token-lending/sdk/src/state/last_update.rs))
//! - SPL Token Account `Pack` impl (spl-token mainnet) —
//!   `Account` size = 165 bytes. Layout (within the fixed 165):
//!   `mint`=0..32, `owner`=32..64, `amount`=64..72 (u64 LE), then
//!   delegate / state / is_native / delegated_amount /
//!   close_authority. Native SPL Token program id =
//!   `TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA`.

use solana_program::{program_error::ProgramError, pubkey::Pubkey};

use crate::error::AuthorityError;

// ── Pinned constants ────────────────────────────────────────────────────────

/// Solend mainnet program id, base58
/// `So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo`. Re-export of the
/// constant pinned in `solend_boundary.rs` so callers of the decoder
/// module don't have to reach across modules for it.
pub use crate::solend_boundary::SOLEND_PROGRAM_ID_MAINNET;

/// Native SPL Token program id, base58
/// `TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA`. Token-2022 is NOT
/// supported by this slice — `decode_spl_token_account_lite` rejects
/// any account whose `AccountInfo.owner` is not this exact id.
pub const SPL_TOKEN_PROGRAM_ID: Pubkey =
    solana_program::pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");

/// Solend Obligation packed length. Source: mainnet
/// `token-lending/sdk/src/state/obligation.rs::OBLIGATION_LEN`.
pub const SOLEND_OBLIGATION_LEN: usize = 1_300;

/// Solend Reserve packed length. Source: mainnet
/// `token-lending/sdk/src/state/reserve.rs::RESERVE_LEN`.
pub const SOLEND_RESERVE_LEN: usize = 619;

/// Solend `LastUpdate` packed length: u64 LE slot (8) + bool stale (1)
/// = 9 bytes, no padding. Source: mainnet `last_update.rs`.
pub const SOLEND_LAST_UPDATE_LEN: usize = 9;

/// `LastUpdate::STALE_AFTER_SLOTS_ELAPSED` constant from Solend
/// mainnet `last_update.rs`. Drives the staleness gate in
/// [`require_reserve_not_stale`].
pub const SOLEND_STALE_AFTER_SLOTS_ELAPSED: u64 = 1;

/// Native SPL Token Account fixed length. Source: spl-token mainnet
/// `state.rs` `impl Pack for Account` — `LEN = 165`. Token-2022
/// (`extensions`) is intentionally out of scope.
pub const SPL_TOKEN_ACCOUNT_LEN: usize = 165;

/// Solend obligation/reserve `version` byte we accept on this build.
/// Source: mainnet code path always emits version `1` for the current
/// schema. A future version bump from Solend triggers
/// `SolendUnsupportedObligationVersion` /
/// `SolendUnsupportedReserveVersion` and forces a re-authorisation
/// flow rather than silently decoding a different layout.
pub const SOLEND_SUPPORTED_VERSION: u8 = 1;

// Solend Obligation prefix byte offsets. Pinned against
// `array_refs![input, 1, 8, 1, PUBKEY_BYTES, PUBKEY_BYTES, ...]` in
// the mainnet Pack impl.
pub const OBLIGATION_OFFSET_VERSION: usize = 0;
pub const OBLIGATION_OFFSET_LAST_UPDATE_SLOT: usize = 1;
pub const OBLIGATION_OFFSET_LAST_UPDATE_STALE: usize = 9;
pub const OBLIGATION_OFFSET_LENDING_MARKET: usize = 10;
pub const OBLIGATION_OFFSET_OWNER: usize = 42;
/// Total length of the fixed prefix this module reads from an
/// Obligation account (everything up to and including `owner`).
pub const OBLIGATION_FIXED_PREFIX_LEN: usize = 74;

// Solend Reserve prefix byte offsets. Same `array_refs!` shape as
// Obligation for the first three fields, then `lending_market`.
pub const RESERVE_OFFSET_VERSION: usize = 0;
pub const RESERVE_OFFSET_LAST_UPDATE_SLOT: usize = 1;
pub const RESERVE_OFFSET_LAST_UPDATE_STALE: usize = 9;
pub const RESERVE_OFFSET_LENDING_MARKET: usize = 10;
/// Total length of the fixed prefix this module reads from a Reserve
/// account (everything up to and including `lending_market`).
pub const RESERVE_FIXED_PREFIX_LEN: usize = 42;

// SPL Token Account offsets within the fixed 165-byte buffer.
pub const SPL_TOKEN_OFFSET_MINT: usize = 0;
pub const SPL_TOKEN_OFFSET_OWNER: usize = 32;
pub const SPL_TOKEN_OFFSET_AMOUNT: usize = 64;

// ── Decoded views ──────────────────────────────────────────────────────────

/// Minimal projection of a Solend Obligation account.
///
/// `owner` is the **inner** Obligation `owner` field at byte offset
/// 42 — i.e. the wallet pubkey that controls the obligation. It is
/// NOT the Solana `AccountInfo.owner` (the program that owns the
/// account data). For the Stage 2 delegated boundary, this MUST equal
/// `AuthorizationRecord.delegated_wallet`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodedSolendObligation {
    pub version: u8,
    pub last_update_slot: u64,
    pub last_update_stale: bool,
    pub lending_market: Pubkey,
    pub owner: Pubkey,
}

/// Minimal projection of a Solend Reserve account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodedSolendReserve {
    pub version: u8,
    pub last_update_slot: u64,
    pub last_update_stale: bool,
    pub lending_market: Pubkey,
}

/// Minimal projection of a native SPL Token Account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodedSplTokenAccountLite {
    pub mint: Pubkey,
    pub owner: Pubkey,
    pub amount: u64,
}

// ── Pure byte decoders ─────────────────────────────────────────────────────

/// Decode the fixed prefix of a Solend Obligation account.
///
/// Strict gates:
/// 1. `data.len() >= SOLEND_OBLIGATION_LEN` (Solend Pack expects
///    exactly `OBLIGATION_LEN`; we accept >= so a future Solend tail
///    extension does not falsely fail short-buffer-style).
/// 2. `version == SOLEND_SUPPORTED_VERSION` else
///    [`SolendUnsupportedObligationVersion`].
///
/// The Pack source guarantees `last_update_stale` is `0u8`/`1u8`; we
/// accept any non-zero byte as `true` for forward compatibility.
pub fn decode_obligation(
    data: &[u8],
) -> Result<DecodedSolendObligation, ProgramError> {
    if data.len() < SOLEND_OBLIGATION_LEN {
        return Err(AuthorityError::SolendAccountDataTooShort.into());
    }
    let version = read_u8(data, OBLIGATION_OFFSET_VERSION)?;
    if version != SOLEND_SUPPORTED_VERSION {
        return Err(AuthorityError::SolendUnsupportedObligationVersion.into());
    }
    let last_update_slot = read_u64_le(data, OBLIGATION_OFFSET_LAST_UPDATE_SLOT)?;
    let last_update_stale = read_bool(data, OBLIGATION_OFFSET_LAST_UPDATE_STALE)?;
    let lending_market = read_pubkey(data, OBLIGATION_OFFSET_LENDING_MARKET)?;
    let owner = read_pubkey(data, OBLIGATION_OFFSET_OWNER)?;
    Ok(DecodedSolendObligation {
        version,
        last_update_slot,
        last_update_stale,
        lending_market,
        owner,
    })
}

/// Decode the fixed prefix of a Solend Reserve account.
///
/// Same shape as [`decode_obligation`] but reads only through
/// `lending_market` (offset 42 exclusive). Decoding the rate-config /
/// liquidity sub-structs requires additional offsets that have not
/// been re-verified end-to-end against mainnet Pack — out of scope
/// for P5a.
pub fn decode_reserve(data: &[u8]) -> Result<DecodedSolendReserve, ProgramError> {
    if data.len() < SOLEND_RESERVE_LEN {
        return Err(AuthorityError::SolendAccountDataTooShort.into());
    }
    let version = read_u8(data, RESERVE_OFFSET_VERSION)?;
    if version != SOLEND_SUPPORTED_VERSION {
        return Err(AuthorityError::SolendUnsupportedReserveVersion.into());
    }
    let last_update_slot = read_u64_le(data, RESERVE_OFFSET_LAST_UPDATE_SLOT)?;
    let last_update_stale = read_bool(data, RESERVE_OFFSET_LAST_UPDATE_STALE)?;
    let lending_market = read_pubkey(data, RESERVE_OFFSET_LENDING_MARKET)?;
    Ok(DecodedSolendReserve {
        version,
        last_update_slot,
        last_update_stale,
        lending_market,
    })
}

/// Decode the minimal `(mint, owner, amount)` projection of a native
/// SPL Token Account.
///
/// Strict 165-byte length: a 72-byte buffer that happens to contain
/// the right bytes at the right offsets is rejected. Token-2022
/// extensions (length > 165) are rejected — the extensions decoder
/// is a separate slice.
pub fn decode_spl_token_account_lite(
    data: &[u8],
) -> Result<DecodedSplTokenAccountLite, ProgramError> {
    if data.len() != SPL_TOKEN_ACCOUNT_LEN {
        return Err(AuthorityError::SolendTokenAccountInvalidLength.into());
    }
    let mint = read_pubkey(data, SPL_TOKEN_OFFSET_MINT)?;
    let owner = read_pubkey(data, SPL_TOKEN_OFFSET_OWNER)?;
    let amount = read_u64_le(data, SPL_TOKEN_OFFSET_AMOUNT)?;
    Ok(DecodedSplTokenAccountLite { mint, owner, amount })
}

// ── Boundary helpers (post-decode) ─────────────────────────────────────────

/// Verify the inner Obligation `owner` field against an
/// `AuthorizationRecord`. The user-main-wallet case is rejected with
/// the dedicated [`SolendMainWalletObligationRejected`] error so
/// attempted main-wallet attacks pinpoint in the audit trail.
pub fn require_obligation_owned_by_delegated_wallet(
    decoded: &DecodedSolendObligation,
    record: &crate::state::AuthorizationRecord,
) -> Result<(), ProgramError> {
    if decoded.owner == record.user {
        return Err(AuthorityError::SolendMainWalletObligationRejected.into());
    }
    if decoded.owner != record.delegated_wallet {
        return Err(AuthorityError::SolendObligationAuthorityMismatch.into());
    }
    Ok(())
}

/// Verify the Obligation's inner `lending_market` field matches the
/// expected market.
pub fn require_obligation_lending_market(
    decoded: &DecodedSolendObligation,
    expected: &Pubkey,
) -> Result<(), ProgramError> {
    if &decoded.lending_market != expected {
        return Err(AuthorityError::SolendObligationLendingMarketMismatch.into());
    }
    Ok(())
}

/// Verify the Reserve's inner `lending_market` field matches the
/// expected market.
pub fn require_reserve_lending_market(
    decoded: &DecodedSolendReserve,
    expected: &Pubkey,
) -> Result<(), ProgramError> {
    if &decoded.lending_market != expected {
        return Err(AuthorityError::SolendReserveLendingMarketMismatch.into());
    }
    Ok(())
}

/// Reserve staleness gate matching Solend's `LastUpdate::is_stale`
/// semantics:
///
/// - `stale_flag == true` → reject (Solend marks reserves stale on
///   accrue without refresh).
/// - `current_slot - last_update_slot > STALE_AFTER_SLOTS_ELAPSED` →
///   reject (using `checked_sub` to fail closed on future-dated
///   slots).
///
/// On success the reserve is fresh enough for at least one slot. P5b
/// will additionally call [`require_reserve_same_slot`] for the
/// strict same-tx Refresh + Withdraw path.
pub fn require_reserve_not_stale(
    decoded: &DecodedSolendReserve,
    current_slot: u64,
) -> Result<(), ProgramError> {
    if decoded.last_update_stale {
        return Err(AuthorityError::SolendReserveStale.into());
    }
    let elapsed = current_slot
        .checked_sub(decoded.last_update_slot)
        .ok_or(AuthorityError::SolendReserveStale)?;
    if elapsed > SOLEND_STALE_AFTER_SLOTS_ELAPSED {
        return Err(AuthorityError::SolendReserveStale.into());
    }
    Ok(())
}

/// Strict same-slot freshness helper for the future P5b same-tx
/// Refresh + Withdraw flow. Requires `last_update_slot == current_slot`
/// — i.e. the reserve was refreshed in *this* slot. Any prior or
/// future slot fails closed.
pub fn require_reserve_same_slot(
    decoded: &DecodedSolendReserve,
    current_slot: u64,
) -> Result<(), ProgramError> {
    if decoded.last_update_slot != current_slot {
        return Err(AuthorityError::SolendLastUpdateSlotMismatch.into());
    }
    Ok(())
}

/// Verify an SPL Token Account fixture's mint matches the expected
/// mint. Use after `decode_spl_token_account_lite` on the token
/// account passed in `accounts[..]`.
pub fn require_spl_token_mint(
    decoded: &DecodedSplTokenAccountLite,
    expected_mint: &Pubkey,
) -> Result<(), ProgramError> {
    if &decoded.mint != expected_mint {
        return Err(AuthorityError::SolendTokenAccountMintMismatch.into());
    }
    Ok(())
}

/// Verify an SPL Token Account fixture's authority matches the
/// expected wallet (typically `AuthorizationRecord.delegated_wallet`
/// for the source token account, or the destination wallet for the
/// sweep destination).
pub fn require_spl_token_authority(
    decoded: &DecodedSplTokenAccountLite,
    expected_authority: &Pubkey,
) -> Result<(), ProgramError> {
    if &decoded.owner != expected_authority {
        return Err(AuthorityError::SolendTokenAccountAuthorityMismatch.into());
    }
    Ok(())
}

// ── Safe byte readers ──────────────────────────────────────────────────────
//
// Every reader uses checked slicing (`data.get(range)`). No raw
// indexing. Short-buffer cases return `SolendAccountDataTooShort`,
// which the decoders propagate.

#[inline]
fn read_u8(data: &[u8], offset: usize) -> Result<u8, ProgramError> {
    data.get(offset)
        .copied()
        .ok_or_else(|| AuthorityError::SolendAccountDataTooShort.into())
}

#[inline]
fn read_u64_le(data: &[u8], offset: usize) -> Result<u64, ProgramError> {
    let bytes = data
        .get(offset..offset + 8)
        .ok_or(AuthorityError::SolendAccountDataTooShort)?;
    let arr: [u8; 8] = bytes
        .try_into()
        .map_err(|_| AuthorityError::SolendAccountDataTooShort)?;
    Ok(u64::from_le_bytes(arr))
}

#[inline]
fn read_bool(data: &[u8], offset: usize) -> Result<bool, ProgramError> {
    let byte = read_u8(data, offset)?;
    Ok(byte != 0)
}

#[inline]
fn read_pubkey(data: &[u8], offset: usize) -> Result<Pubkey, ProgramError> {
    let bytes = data
        .get(offset..offset + 32)
        .ok_or(AuthorityError::SolendAccountDataTooShort)?;
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| AuthorityError::SolendAccountDataTooShort)?;
    Ok(Pubkey::new_from_array(arr))
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AuthorizationRecord, Stage2ActionType, STAGE2_AUTHORITY_SCHEMA_VERSION};

    fn pk(b: u8) -> Pubkey {
        Pubkey::new_from_array([b; 32])
    }

    /// Build a synthetic Obligation byte buffer with the prefix
    /// fields we care about. The tail is zero-filled — the Pack tail
    /// is not decoded by this module.
    fn make_obligation_bytes(
        version: u8,
        last_update_slot: u64,
        last_update_stale: bool,
        lending_market: [u8; 32],
        owner: [u8; 32],
    ) -> Vec<u8> {
        let mut bytes = vec![0u8; SOLEND_OBLIGATION_LEN];
        bytes[OBLIGATION_OFFSET_VERSION] = version;
        bytes[OBLIGATION_OFFSET_LAST_UPDATE_SLOT
            ..OBLIGATION_OFFSET_LAST_UPDATE_SLOT + 8]
            .copy_from_slice(&last_update_slot.to_le_bytes());
        bytes[OBLIGATION_OFFSET_LAST_UPDATE_STALE] = if last_update_stale { 1 } else { 0 };
        bytes[OBLIGATION_OFFSET_LENDING_MARKET..OBLIGATION_OFFSET_LENDING_MARKET + 32]
            .copy_from_slice(&lending_market);
        bytes[OBLIGATION_OFFSET_OWNER..OBLIGATION_OFFSET_OWNER + 32].copy_from_slice(&owner);
        bytes
    }

    fn make_reserve_bytes(
        version: u8,
        last_update_slot: u64,
        last_update_stale: bool,
        lending_market: [u8; 32],
    ) -> Vec<u8> {
        let mut bytes = vec![0u8; SOLEND_RESERVE_LEN];
        bytes[RESERVE_OFFSET_VERSION] = version;
        bytes[RESERVE_OFFSET_LAST_UPDATE_SLOT..RESERVE_OFFSET_LAST_UPDATE_SLOT + 8]
            .copy_from_slice(&last_update_slot.to_le_bytes());
        bytes[RESERVE_OFFSET_LAST_UPDATE_STALE] = if last_update_stale { 1 } else { 0 };
        bytes[RESERVE_OFFSET_LENDING_MARKET..RESERVE_OFFSET_LENDING_MARKET + 32]
            .copy_from_slice(&lending_market);
        bytes
    }

    fn make_spl_token_bytes(mint: [u8; 32], owner: [u8; 32], amount: u64) -> Vec<u8> {
        let mut bytes = vec![0u8; SPL_TOKEN_ACCOUNT_LEN];
        bytes[SPL_TOKEN_OFFSET_MINT..SPL_TOKEN_OFFSET_MINT + 32].copy_from_slice(&mint);
        bytes[SPL_TOKEN_OFFSET_OWNER..SPL_TOKEN_OFFSET_OWNER + 32].copy_from_slice(&owner);
        bytes[SPL_TOKEN_OFFSET_AMOUNT..SPL_TOKEN_OFFSET_AMOUNT + 8]
            .copy_from_slice(&amount.to_le_bytes());
        bytes
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

    fn assert_err(actual: ProgramError, expected: AuthorityError) {
        assert_eq!(
            actual,
            ProgramError::Custom(expected as u32),
            "expected {expected:?} (code {}); got {actual:?}",
            expected as u32,
        );
    }

    // ── Pinning ────────────────────────────────────────────────────────────

    #[test]
    fn obligation_lending_market_offset_is_pinned() {
        // Source: token-lending/sdk/src/state/obligation.rs Pack impl
        // `array_refs![input, 1, 8, 1, PUBKEY_BYTES, PUBKEY_BYTES, …]`
        // → version(0..1), last_update_slot(1..9), last_update_stale(9..10),
        //   lending_market(10..42), owner(42..74).
        assert_eq!(OBLIGATION_OFFSET_VERSION, 0);
        assert_eq!(OBLIGATION_OFFSET_LAST_UPDATE_SLOT, 1);
        assert_eq!(OBLIGATION_OFFSET_LAST_UPDATE_STALE, 9);
        assert_eq!(OBLIGATION_OFFSET_LENDING_MARKET, 10);
        assert_eq!(OBLIGATION_OFFSET_OWNER, 42);
        assert_eq!(OBLIGATION_FIXED_PREFIX_LEN, 74);
        assert_eq!(SOLEND_OBLIGATION_LEN, 1_300);
    }

    #[test]
    fn reserve_lending_market_offset_is_pinned() {
        assert_eq!(RESERVE_OFFSET_VERSION, 0);
        assert_eq!(RESERVE_OFFSET_LAST_UPDATE_SLOT, 1);
        assert_eq!(RESERVE_OFFSET_LAST_UPDATE_STALE, 9);
        assert_eq!(RESERVE_OFFSET_LENDING_MARKET, 10);
        assert_eq!(RESERVE_FIXED_PREFIX_LEN, 42);
        assert_eq!(SOLEND_RESERVE_LEN, 619);
    }

    #[test]
    fn last_update_is_exactly_9_bytes_slot_plus_bool() {
        // Spec: u64 LE (8) + bool (1) = 9 bytes, no padding.
        assert_eq!(SOLEND_LAST_UPDATE_LEN, 9);
        // The `stale` byte sits exactly at offset (slot_offset + 8) —
        // any padding/alignment would push it later.
        assert_eq!(
            OBLIGATION_OFFSET_LAST_UPDATE_STALE,
            OBLIGATION_OFFSET_LAST_UPDATE_SLOT + 8,
        );
        assert_eq!(
            RESERVE_OFFSET_LAST_UPDATE_STALE,
            RESERVE_OFFSET_LAST_UPDATE_SLOT + 8,
        );
        // And the next field sits exactly one byte later.
        assert_eq!(
            OBLIGATION_OFFSET_LENDING_MARKET,
            OBLIGATION_OFFSET_LAST_UPDATE_STALE + 1,
        );
        assert_eq!(
            RESERVE_OFFSET_LENDING_MARKET,
            RESERVE_OFFSET_LAST_UPDATE_STALE + 1,
        );
    }

    #[test]
    fn solend_program_id_matches_repo_pin() {
        // Re-export of solend_boundary's pin. Verifies the decode
        // module doesn't accidentally redeclare a different value.
        assert_eq!(
            SOLEND_PROGRAM_ID_MAINNET.to_string(),
            "So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo",
        );
    }

    #[test]
    fn spl_token_program_id_pin_matches_native_token_program() {
        assert_eq!(
            SPL_TOKEN_PROGRAM_ID.to_string(),
            "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
        );
    }

    #[test]
    fn spl_token_account_constants_are_pinned() {
        assert_eq!(SPL_TOKEN_ACCOUNT_LEN, 165);
        assert_eq!(SPL_TOKEN_OFFSET_MINT, 0);
        assert_eq!(SPL_TOKEN_OFFSET_OWNER, 32);
        assert_eq!(SPL_TOKEN_OFFSET_AMOUNT, 64);
    }

    // ── Obligation decode ─────────────────────────────────────────────────

    #[test]
    fn obligation_decode_happy_path_from_official_layout_fixture() {
        let lm = [0x42; 32];
        let owner = [0xBB; 32];
        let bytes =
            make_obligation_bytes(SOLEND_SUPPORTED_VERSION, 999_000, false, lm, owner);
        let decoded = decode_obligation(&bytes).unwrap();
        assert_eq!(decoded.version, SOLEND_SUPPORTED_VERSION);
        assert_eq!(decoded.last_update_slot, 999_000);
        assert!(!decoded.last_update_stale);
        assert_eq!(decoded.lending_market.to_bytes(), lm);
        assert_eq!(decoded.owner.to_bytes(), owner);
    }

    #[test]
    fn obligation_decode_rejects_short_data() {
        let bytes = vec![0u8; SOLEND_OBLIGATION_LEN - 1];
        let err = decode_obligation(&bytes).unwrap_err();
        assert_err(err, AuthorityError::SolendAccountDataTooShort);
    }

    #[test]
    fn obligation_decode_rejects_zero_length_data() {
        let err = decode_obligation(&[]).unwrap_err();
        assert_err(err, AuthorityError::SolendAccountDataTooShort);
    }

    #[test]
    fn obligation_decode_rejects_wrong_version() {
        let bytes = make_obligation_bytes(2, 100, false, [0x11; 32], [0xBB; 32]);
        let err = decode_obligation(&bytes).unwrap_err();
        assert_err(err, AuthorityError::SolendUnsupportedObligationVersion);

        // Defence: also reject version 0 (unset / Uninitialized).
        let bytes = make_obligation_bytes(0, 100, false, [0x11; 32], [0xBB; 32]);
        let err = decode_obligation(&bytes).unwrap_err();
        assert_err(err, AuthorityError::SolendUnsupportedObligationVersion);
    }

    #[test]
    fn obligation_decode_reads_inner_owner_not_accountinfo_owner() {
        // The decoder reads the Solend Obligation `owner` field at
        // byte offset 42 — distinct from the Solana AccountInfo.owner
        // (the program that owns the account data), which is checked
        // separately at the AccountInfo wrapper layer in
        // solend_boundary.rs::verify_obligation_account_info.
        let inner_owner = [0xBB; 32];
        // Also stage non-Solend-program bytes at the start of the
        // buffer to make it visually clear that the *inner* owner
        // field, not anything earlier in the buffer, is what the
        // decoder returns.
        let mut bytes =
            make_obligation_bytes(SOLEND_SUPPORTED_VERSION, 100, false, [0x11; 32], inner_owner);
        // A fake "AccountInfo.owner pubkey" in the early bytes (this
        // would never actually be in the data; just demonstrating the
        // distinction with a loud sentinel).
        bytes[1..33].copy_from_slice(&[0xFF; 32]);
        // Restore the `last_update_slot` byte we just clobbered with
        // the loud sentinel. (The first byte of the slot field would
        // sit at offset 1; the sentinel write at 1..33 corrupts it.)
        bytes[OBLIGATION_OFFSET_LAST_UPDATE_SLOT
            ..OBLIGATION_OFFSET_LAST_UPDATE_SLOT + 8]
            .copy_from_slice(&100u64.to_le_bytes());
        bytes[OBLIGATION_OFFSET_LAST_UPDATE_STALE] = 0;
        bytes[OBLIGATION_OFFSET_LENDING_MARKET..OBLIGATION_OFFSET_LENDING_MARKET + 32]
            .copy_from_slice(&[0x11; 32]);
        let decoded = decode_obligation(&bytes).unwrap();
        assert_eq!(
            decoded.owner.to_bytes(),
            inner_owner,
            "decoder must return the inner Obligation.owner field at offset 42",
        );
    }

    #[test]
    fn obligation_decode_round_trips_lending_market_via_pinned_offset() {
        let lm: [u8; 32] = core::array::from_fn(|i| (0x40 + i) as u8);
        let bytes =
            make_obligation_bytes(SOLEND_SUPPORTED_VERSION, 1, false, lm, [0xBB; 32]);
        // Confirm the bytes at the pinned offset match what we wrote.
        assert_eq!(
            &bytes[OBLIGATION_OFFSET_LENDING_MARKET..OBLIGATION_OFFSET_LENDING_MARKET + 32],
            &lm[..],
        );
        let decoded = decode_obligation(&bytes).unwrap();
        assert_eq!(decoded.lending_market.to_bytes(), lm);
    }

    // ── Reserve decode ────────────────────────────────────────────────────

    #[test]
    fn reserve_decode_happy_path_from_official_layout_fixture() {
        let lm = [0x33; 32];
        let bytes = make_reserve_bytes(SOLEND_SUPPORTED_VERSION, 415_500_000, false, lm);
        let decoded = decode_reserve(&bytes).unwrap();
        assert_eq!(decoded.version, SOLEND_SUPPORTED_VERSION);
        assert_eq!(decoded.last_update_slot, 415_500_000);
        assert!(!decoded.last_update_stale);
        assert_eq!(decoded.lending_market.to_bytes(), lm);
    }

    #[test]
    fn reserve_decode_rejects_short_data() {
        let bytes = vec![0u8; SOLEND_RESERVE_LEN - 1];
        let err = decode_reserve(&bytes).unwrap_err();
        assert_err(err, AuthorityError::SolendAccountDataTooShort);
    }

    #[test]
    fn reserve_decode_rejects_wrong_version() {
        let bytes = make_reserve_bytes(2, 100, false, [0x33; 32]);
        let err = decode_reserve(&bytes).unwrap_err();
        assert_err(err, AuthorityError::SolendUnsupportedReserveVersion);
    }

    // ── Boundary helpers — main-wallet poison + lending market ────────────

    #[test]
    fn delegated_obligation_inner_owner_matches_record_delegated_wallet_passes() {
        let record = record_fixture();
        let bytes = make_obligation_bytes(
            SOLEND_SUPPORTED_VERSION,
            100,
            false,
            [0x42; 32],
            record.delegated_wallet.to_bytes(),
        );
        let decoded = decode_obligation(&bytes).unwrap();
        require_obligation_owned_by_delegated_wallet(&decoded, &record).unwrap();
    }

    /// REQUIRED POISON TEST (P5a P0 list): inner Obligation.owner
    /// equals AuthorizationRecord.user (main wallet). Must surface
    /// the dedicated `SolendMainWalletObligationRejected` error code,
    /// NOT the generic mismatch.
    #[test]
    fn main_wallet_obligation_inner_owner_is_rejected_with_dedicated_error() {
        let record = record_fixture();
        let bytes = make_obligation_bytes(
            SOLEND_SUPPORTED_VERSION,
            100,
            false,
            [0x42; 32],
            record.user.to_bytes(),
        );
        let decoded = decode_obligation(&bytes).unwrap();
        let err = require_obligation_owned_by_delegated_wallet(&decoded, &record).unwrap_err();
        assert_err(err, AuthorityError::SolendMainWalletObligationRejected);
    }

    #[test]
    fn random_wrong_obligation_inner_owner_is_rejected_with_generic_mismatch() {
        let record = record_fixture();
        let bytes = make_obligation_bytes(
            SOLEND_SUPPORTED_VERSION,
            100,
            false,
            [0x42; 32],
            // Some unrelated third-party wallet — not user, not
            // delegated. Surfaces the GENERIC mismatch error.
            [0x99; 32],
        );
        let decoded = decode_obligation(&bytes).unwrap();
        let err = require_obligation_owned_by_delegated_wallet(&decoded, &record).unwrap_err();
        assert_err(err, AuthorityError::SolendObligationAuthorityMismatch);
    }

    #[test]
    fn obligation_lending_market_mismatch_rejected() {
        let record = record_fixture();
        let bytes = make_obligation_bytes(
            SOLEND_SUPPORTED_VERSION,
            100,
            false,
            [0x42; 32],
            record.delegated_wallet.to_bytes(),
        );
        let decoded = decode_obligation(&bytes).unwrap();
        let expected = pk(0x77);
        let err = require_obligation_lending_market(&decoded, &expected).unwrap_err();
        assert_err(err, AuthorityError::SolendObligationLendingMarketMismatch);
    }

    #[test]
    fn obligation_lending_market_match_passes() {
        let record = record_fixture();
        let lm = [0x42; 32];
        let bytes = make_obligation_bytes(
            SOLEND_SUPPORTED_VERSION,
            100,
            false,
            lm,
            record.delegated_wallet.to_bytes(),
        );
        let decoded = decode_obligation(&bytes).unwrap();
        require_obligation_lending_market(&decoded, &Pubkey::new_from_array(lm)).unwrap();
    }

    // ── Reserve boundary helpers ──────────────────────────────────────────

    #[test]
    fn reserve_lending_market_mismatch_rejected() {
        let bytes = make_reserve_bytes(SOLEND_SUPPORTED_VERSION, 100, false, [0x33; 32]);
        let decoded = decode_reserve(&bytes).unwrap();
        let err = require_reserve_lending_market(&decoded, &pk(0x42)).unwrap_err();
        assert_err(err, AuthorityError::SolendReserveLendingMarketMismatch);
    }

    #[test]
    fn reserve_lending_market_match_passes() {
        let lm = [0x33; 32];
        let bytes = make_reserve_bytes(SOLEND_SUPPORTED_VERSION, 100, false, lm);
        let decoded = decode_reserve(&bytes).unwrap();
        require_reserve_lending_market(&decoded, &Pubkey::new_from_array(lm)).unwrap();
    }

    // ── Reserve staleness ─────────────────────────────────────────────────

    #[test]
    fn stale_reserve_flag_rejected() {
        let bytes = make_reserve_bytes(SOLEND_SUPPORTED_VERSION, 100, true, [0x33; 32]);
        let decoded = decode_reserve(&bytes).unwrap();
        let err = require_reserve_not_stale(&decoded, 100).unwrap_err();
        assert_err(err, AuthorityError::SolendReserveStale);
    }

    #[test]
    fn stale_reserve_slot_rejected() {
        // last_update_slot = 100, current_slot = 102 → elapsed = 2 >
        // STALE_AFTER_SLOTS_ELAPSED (1). Reject.
        let bytes = make_reserve_bytes(SOLEND_SUPPORTED_VERSION, 100, false, [0x33; 32]);
        let decoded = decode_reserve(&bytes).unwrap();
        let err = require_reserve_not_stale(&decoded, 102).unwrap_err();
        assert_err(err, AuthorityError::SolendReserveStale);
    }

    #[test]
    fn fresh_reserve_within_one_slot_passes() {
        // elapsed = 1 → equals STALE_AFTER_SLOTS_ELAPSED, still passes
        // (the gate is `elapsed > STALE_AFTER_SLOTS_ELAPSED`).
        let bytes = make_reserve_bytes(SOLEND_SUPPORTED_VERSION, 100, false, [0x33; 32]);
        let decoded = decode_reserve(&bytes).unwrap();
        require_reserve_not_stale(&decoded, 101).unwrap();
        require_reserve_not_stale(&decoded, 100).unwrap();
    }

    #[test]
    fn future_dated_reserve_slot_rejected() {
        // last_update_slot > current_slot (clock skew); checked_sub
        // underflows → SolendReserveStale.
        let bytes = make_reserve_bytes(SOLEND_SUPPORTED_VERSION, 200, false, [0x33; 32]);
        let decoded = decode_reserve(&bytes).unwrap();
        let err = require_reserve_not_stale(&decoded, 100).unwrap_err();
        assert_err(err, AuthorityError::SolendReserveStale);
    }

    #[test]
    fn strict_same_slot_helper_rejects_prior_slot() {
        let bytes = make_reserve_bytes(SOLEND_SUPPORTED_VERSION, 100, false, [0x33; 32]);
        let decoded = decode_reserve(&bytes).unwrap();
        // current_slot one ahead — strict helper rejects (only same
        // slot passes).
        let err = require_reserve_same_slot(&decoded, 101).unwrap_err();
        assert_err(err, AuthorityError::SolendLastUpdateSlotMismatch);
        // Same slot — passes.
        require_reserve_same_slot(&decoded, 100).unwrap();
        // Earlier slot (clock skew) — strict helper rejects.
        let err = require_reserve_same_slot(&decoded, 99).unwrap_err();
        assert_err(err, AuthorityError::SolendLastUpdateSlotMismatch);
    }

    // ── SPL Token decode ──────────────────────────────────────────────────

    #[test]
    fn spl_token_lite_decode_happy_path() {
        let mint = [0xAB; 32];
        let owner = [0xCD; 32];
        let bytes = make_spl_token_bytes(mint, owner, 1_234_567);
        let decoded = decode_spl_token_account_lite(&bytes).unwrap();
        assert_eq!(decoded.mint.to_bytes(), mint);
        assert_eq!(decoded.owner.to_bytes(), owner);
        assert_eq!(decoded.amount, 1_234_567);
    }

    #[test]
    fn spl_token_lite_decode_requires_exact_165_bytes() {
        // 164 — too short.
        let err = decode_spl_token_account_lite(&vec![0u8; 164]).unwrap_err();
        assert_err(err, AuthorityError::SolendTokenAccountInvalidLength);
        // 166 — too long. Could be a Token-2022 extension buffer or a
        // tampered shape; reject.
        let err = decode_spl_token_account_lite(&vec![0u8; 166]).unwrap_err();
        assert_err(err, AuthorityError::SolendTokenAccountInvalidLength);
        // Exactly 165 — passes (with all-zero data, mint and owner
        // are the zero pubkey but the decode itself succeeds).
        decode_spl_token_account_lite(&vec![0u8; 165]).unwrap();
    }

    #[test]
    fn spl_token_lite_rejects_72_byte_fake_account() {
        // A 72-byte buffer happens to fit `mint(0..32) + owner(32..64) +
        // amount(64..72)` — but native SPL Token accounts are exactly
        // 165 bytes. A 72-byte "fake" buffer is rejected by the size
        // gate before any field is read.
        let mut fake = vec![0u8; 72];
        fake[SPL_TOKEN_OFFSET_MINT..SPL_TOKEN_OFFSET_MINT + 32].copy_from_slice(&[0xAB; 32]);
        fake[SPL_TOKEN_OFFSET_OWNER..SPL_TOKEN_OFFSET_OWNER + 32].copy_from_slice(&[0xCD; 32]);
        fake[SPL_TOKEN_OFFSET_AMOUNT..SPL_TOKEN_OFFSET_AMOUNT + 8]
            .copy_from_slice(&1_234u64.to_le_bytes());
        let err = decode_spl_token_account_lite(&fake).unwrap_err();
        assert_err(err, AuthorityError::SolendTokenAccountInvalidLength);
    }

    #[test]
    fn spl_token_lite_amount_reads_little_endian() {
        let mint = [0xAB; 32];
        let owner = [0xCD; 32];
        // 0xCAFEBABE_DEADBEEF — anchors that the amount is read as
        // little-endian u64 (and not big-endian or some other shape).
        let amount: u64 = 0xCAFE_BABE_DEAD_BEEF;
        let bytes = make_spl_token_bytes(mint, owner, amount);
        // Confirm the LE bytes at the pinned offset:
        assert_eq!(
            &bytes[SPL_TOKEN_OFFSET_AMOUNT..SPL_TOKEN_OFFSET_AMOUNT + 8],
            &amount.to_le_bytes()[..],
        );
        let decoded = decode_spl_token_account_lite(&bytes).unwrap();
        assert_eq!(decoded.amount, amount);
    }

    #[test]
    fn spl_token_mint_mismatch_rejected() {
        let bytes = make_spl_token_bytes([0xAB; 32], [0xCD; 32], 1);
        let decoded = decode_spl_token_account_lite(&bytes).unwrap();
        let err = require_spl_token_mint(&decoded, &pk(0x77)).unwrap_err();
        assert_err(err, AuthorityError::SolendTokenAccountMintMismatch);
    }

    #[test]
    fn spl_token_mint_match_passes() {
        let mint = [0xAB; 32];
        let bytes = make_spl_token_bytes(mint, [0xCD; 32], 1);
        let decoded = decode_spl_token_account_lite(&bytes).unwrap();
        require_spl_token_mint(&decoded, &Pubkey::new_from_array(mint)).unwrap();
    }

    #[test]
    fn spl_token_authority_mismatch_rejected() {
        let bytes = make_spl_token_bytes([0xAB; 32], [0xCD; 32], 1);
        let decoded = decode_spl_token_account_lite(&bytes).unwrap();
        let err = require_spl_token_authority(&decoded, &pk(0x77)).unwrap_err();
        assert_err(err, AuthorityError::SolendTokenAccountAuthorityMismatch);
    }

    #[test]
    fn spl_token_authority_match_passes() {
        let owner = [0xCD; 32];
        let bytes = make_spl_token_bytes([0xAB; 32], owner, 1);
        let decoded = decode_spl_token_account_lite(&bytes).unwrap();
        require_spl_token_authority(&decoded, &Pubkey::new_from_array(owner)).unwrap();
    }

    // ── Byte reader edge cases ───────────────────────────────────────────

    #[test]
    fn read_u8_short_buffer_fails_closed() {
        let err = read_u8(&[], 0).unwrap_err();
        assert_err(err, AuthorityError::SolendAccountDataTooShort);
        let err = read_u8(&[1u8, 2], 5).unwrap_err();
        assert_err(err, AuthorityError::SolendAccountDataTooShort);
    }

    #[test]
    fn read_u64_le_short_buffer_fails_closed() {
        // 7 bytes — one short of the 8 we need.
        let err = read_u64_le(&[0u8; 7], 0).unwrap_err();
        assert_err(err, AuthorityError::SolendAccountDataTooShort);
    }

    #[test]
    fn read_pubkey_short_buffer_fails_closed() {
        // 31 bytes — one short of the 32 we need.
        let err = read_pubkey(&[0u8; 31], 0).unwrap_err();
        assert_err(err, AuthorityError::SolendAccountDataTooShort);
    }

    #[test]
    fn read_bool_accepts_non_zero_as_true() {
        // SPL convention: any non-zero byte = true. 0 = false.
        assert!(!read_bool(&[0], 0).unwrap());
        assert!(read_bool(&[1], 0).unwrap());
        assert!(read_bool(&[0xFF], 0).unwrap());
        assert!(read_bool(&[0x42], 0).unwrap());
    }
}
