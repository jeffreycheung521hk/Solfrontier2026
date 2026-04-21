//! Solend raw-account decoders. INTERNAL to `integrations::solend`.
//!
//! Offsets and layout constants are cited from the Solend token-lending
//! program source (`solendprotocol/solana-program-library`), `Pack` impls
//! of `Obligation` and `Reserve`. Per Part 6B §64, only the subset V1
//! Deposit-only needs is decoded; interest-rate / fee / accumulator /
//! value-wad fields are intentionally skipped.
//!
//! The types exposed here (`SolendObligationRaw`, `SolendReserveRaw`,
//! their sub-structs, and `DecodeError`) are Solend-shaped by design and
//! must not cross the Part 5 / Part 6 seam. Mapping to protocol-agnostic
//! `crate::lending` types happens in `mapping.rs`.

use solana_sdk::pubkey::Pubkey;

/// Solend token-lending program id (mainnet).
pub const SOLEND_PROGRAM_ID_BS58: &str = "So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo";

/// Sentinel oracle pubkey, human-readable "nu11…", used by Solend reserves
/// to indicate "no oracle configured for this slot." Spike-observed at
/// mainnet reserves in the Coin98 pool (§30.5).
pub const SOLEND_NULL_ORACLE_SENTINEL_BS58: &str =
    "nu11111111111111111111111111111111111111111";

// ── Obligation layout ────────────────────────────────────────────────────
//
// Citation: solendprotocol/solana-program-library
//   token-lending/program/src/state/obligation.rs, `Pack` impl.
//
//   0   1    version
//   1   8    last_update.slot (u64 LE)
//   9   1    last_update.stale (bool, 0 or 1)
//   10  32   lending_market (Pubkey)
//   42  32   owner (Pubkey)
//   74  16   deposited_value (Decimal wad)          — SKIPPED
//   90  16   borrowed_value (Decimal wad)           — SKIPPED
//   106 16   allowed_borrow_value (Decimal wad)     — SKIPPED
//   122 16   unhealthy_borrow_value (Decimal wad)   — SKIPPED
//   138 64   _padding [u8; 64] (integrity-checked, must be all-zero)
//   202 1    deposits_len (u8)
//   203 1    borrows_len (u8)
//   204 1096 data_flat: deposits[] (88 bytes each) then borrows[] (112 bytes each)
//
//   ObligationCollateral (88 bytes):
//     0   32   deposit_reserve (Pubkey)
//     32  8    deposited_amount (u64 LE)
//     40  16   market_value (Decimal)               — SKIPPED
//     56  32   _padding                             — SKIPPED
//
//   ObligationLiquidity (112 bytes):
//     0   32   borrow_reserve (Pubkey)
//     32  16   cumulative_borrow_rate_wads          — SKIPPED
//     48  16   borrowed_amount_wads (u128 LE)
//     64  16   market_value                         — SKIPPED
//     80  32   _padding                             — SKIPPED

pub const OBLIGATION_LEN: usize = 1300;
pub const OBLIGATION_COLLATERAL_LEN: usize = 88;
pub const OBLIGATION_LIQUIDITY_LEN: usize = 112;

const OBL_VERSION_OFF: usize = 0;
const OBL_LAST_UPDATE_SLOT_OFF: usize = 1;
const OBL_LAST_UPDATE_STALE_OFF: usize = 9;
const OBL_LENDING_MARKET_OFF: usize = 10;
const OBL_OWNER_OFF: usize = 42;
const OBL_PADDING_OFF: usize = 138;
const OBL_PADDING_LEN: usize = 64;
const OBL_DEPOSITS_LEN_OFF: usize = 202;
const OBL_BORROWS_LEN_OFF: usize = 203;
const OBL_DATA_FLAT_OFF: usize = 204;
const OBL_DATA_FLAT_LEN: usize = OBLIGATION_LEN - OBL_DATA_FLAT_OFF; // 1096

const OBL_COLL_DEPOSIT_RESERVE_OFF: usize = 0;
const OBL_COLL_DEPOSITED_AMOUNT_OFF: usize = 32;

const OBL_LIQ_BORROW_RESERVE_OFF: usize = 0;
const OBL_LIQ_BORROWED_AMOUNT_WADS_OFF: usize = 48;

// ── Reserve layout ───────────────────────────────────────────────────────
//
// Citation: solendprotocol/solana-program-library
//   token-lending/program/src/state/reserve.rs, `Pack` impl.
//
//   0     1    version
//   1     8    last_update.slot (u64 LE)
//   9     1    last_update.stale
//   10    32   lending_market (Pubkey)
//   42    32   liquidity.mint_pubkey (Pubkey)
//   74    1    liquidity.mint_decimals (u8)
//   75    32   liquidity.supply_pubkey (Pubkey)
//   107   32   liquidity.pyth_oracle (Pubkey; may be sentinel)
//   139   32   liquidity.switchboard_oracle (Pubkey; may be sentinel)
//   171   8    liquidity.available_amount (u64 LE)
//   179   16   liquidity.borrowed_amount_wads            — SKIPPED
//   195   16   liquidity.cumulative_borrow_rate_wads     — SKIPPED
//   211   16   liquidity.market_price                    — SKIPPED
//   227   32   collateral.mint_pubkey (Pubkey)
//   259   8    collateral.mint_total_supply              — SKIPPED
//   267   32   collateral.supply_pubkey (Pubkey)
//   299   1..  config fields                             — SKIPPED (slice 1)
//   ...   ...  (see reserve.rs)
//   619 total  (RESERVE_LEN)

pub const RESERVE_LEN: usize = 619;

const RES_VERSION_OFF: usize = 0;
const RES_LAST_UPDATE_SLOT_OFF: usize = 1;
const RES_LAST_UPDATE_STALE_OFF: usize = 9;
const RES_LENDING_MARKET_OFF: usize = 10;
const RES_LIQ_MINT_OFF: usize = 42;
const RES_LIQ_MINT_DECIMALS_OFF: usize = 74;
const RES_LIQ_SUPPLY_OFF: usize = 75;
const RES_LIQ_PYTH_ORACLE_OFF: usize = 107;
const RES_LIQ_SWITCHBOARD_ORACLE_OFF: usize = 139;
const RES_LIQ_AVAILABLE_AMOUNT_OFF: usize = 171;
const RES_COLL_MINT_OFF: usize = 227;
const RES_COLL_SUPPLY_OFF: usize = 267;

// ── Decoded intermediate types (Solend-shaped; internal) ────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SolendObligationRaw {
    pub version: u8,
    pub last_update_slot: u64,
    pub last_update_stale: bool,
    pub lending_market: Pubkey,
    pub owner: Pubkey,
    pub deposits: Vec<SolendObligationCollateralRaw>,
    pub borrows: Vec<SolendObligationLiquidityRaw>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SolendObligationCollateralRaw {
    pub deposit_reserve: Pubkey,
    /// c-token units. Pairs with `lending::CollateralTokenAmount` in mapping.
    pub deposited_amount: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SolendObligationLiquidityRaw {
    pub borrow_reserve: Pubkey,
    /// Wad-scaled (typically 10^18). Pairs with `lending::WadAmount` in mapping.
    pub borrowed_amount_wads: u128,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SolendReserveRaw {
    pub version: u8,
    pub last_update_slot: u64,
    pub last_update_stale: bool,
    pub lending_market: Pubkey,
    pub liquidity_mint: Pubkey,
    pub liquidity_mint_decimals: u8,
    pub liquidity_supply: Pubkey,
    pub pyth_oracle: Pubkey,
    pub switchboard_oracle: Pubkey,
    /// Underlying base units. Pairs with `lending::UnderlyingAmount` in mapping.
    pub liquidity_available_amount: u64,
    pub collateral_mint: Pubkey,
    pub collateral_supply: Pubkey,
}

// ── Decode errors ────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error("obligation bytes wrong length: expected {OBLIGATION_LEN}, got {0}")]
    ObligationWrongSize(usize),
    #[error("reserve bytes wrong length: expected {RESERVE_LEN}, got {0}")]
    ReserveWrongSize(usize),
    #[error("obligation padding at bytes 138..202 is not all zero")]
    ObligationPaddingNonZero,
    #[error("obligation arrays overflow data_flat: deposits_len={deposits}, borrows_len={borrows}")]
    ObligationArrayOverflow { deposits: u8, borrows: u8 },
    #[error("obligation stale bit at offset 9 is {0}, expected 0 or 1")]
    ObligationStaleBitInvalid(u8),
    #[error("reserve stale bit at offset 9 is {0}, expected 0 or 1")]
    ReserveStaleBitInvalid(u8),
}

// ── Decoders ────────────────────────────────────────────────────────────

fn read_u64_le(data: &[u8], off: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&data[off..off + 8]);
    u64::from_le_bytes(b)
}

fn read_u128_le(data: &[u8], off: usize) -> u128 {
    let mut b = [0u8; 16];
    b.copy_from_slice(&data[off..off + 16]);
    u128::from_le_bytes(b)
}

fn read_pubkey(data: &[u8], off: usize) -> Pubkey {
    let mut b = [0u8; 32];
    b.copy_from_slice(&data[off..off + 32]);
    Pubkey::new_from_array(b)
}

fn read_bool(data: &[u8], off: usize) -> Option<bool> {
    match data[off] {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }
}

/// Decode the read-model-minimum subset of a Solend Obligation account.
///
/// Only the fields Part 6B §64.1 lists as required for the Deposit-only
/// slice are populated. Value-wad fields at offsets 74..138 are skipped.
/// The 64-byte zero-padding at 138..202 is integrity-checked.
pub fn decode_obligation(data: &[u8]) -> Result<SolendObligationRaw, DecodeError> {
    if data.len() != OBLIGATION_LEN {
        return Err(DecodeError::ObligationWrongSize(data.len()));
    }

    // Integrity check: the 64-byte padding block must be all zero.
    // The spike observed this to hold on real mainnet obligations; a non-zero
    // padding block would signal layout drift or a different program version.
    if data[OBL_PADDING_OFF..OBL_PADDING_OFF + OBL_PADDING_LEN]
        .iter()
        .any(|b| *b != 0)
    {
        return Err(DecodeError::ObligationPaddingNonZero);
    }

    let version = data[OBL_VERSION_OFF];
    let last_update_slot = read_u64_le(data, OBL_LAST_UPDATE_SLOT_OFF);
    let last_update_stale = read_bool(data, OBL_LAST_UPDATE_STALE_OFF)
        .ok_or(DecodeError::ObligationStaleBitInvalid(
            data[OBL_LAST_UPDATE_STALE_OFF],
        ))?;
    let lending_market = read_pubkey(data, OBL_LENDING_MARKET_OFF);
    let owner = read_pubkey(data, OBL_OWNER_OFF);

    let deposits_len = data[OBL_DEPOSITS_LEN_OFF];
    let borrows_len = data[OBL_BORROWS_LEN_OFF];

    let total_array_bytes = (deposits_len as usize) * OBLIGATION_COLLATERAL_LEN
        + (borrows_len as usize) * OBLIGATION_LIQUIDITY_LEN;
    if total_array_bytes > OBL_DATA_FLAT_LEN {
        return Err(DecodeError::ObligationArrayOverflow {
            deposits: deposits_len,
            borrows: borrows_len,
        });
    }

    let mut deposits = Vec::with_capacity(deposits_len as usize);
    for i in 0..(deposits_len as usize) {
        let base = OBL_DATA_FLAT_OFF + i * OBLIGATION_COLLATERAL_LEN;
        deposits.push(SolendObligationCollateralRaw {
            deposit_reserve: read_pubkey(data, base + OBL_COLL_DEPOSIT_RESERVE_OFF),
            deposited_amount: read_u64_le(data, base + OBL_COLL_DEPOSITED_AMOUNT_OFF),
        });
    }

    let borrows_start =
        OBL_DATA_FLAT_OFF + (deposits_len as usize) * OBLIGATION_COLLATERAL_LEN;
    let mut borrows = Vec::with_capacity(borrows_len as usize);
    for i in 0..(borrows_len as usize) {
        let base = borrows_start + i * OBLIGATION_LIQUIDITY_LEN;
        borrows.push(SolendObligationLiquidityRaw {
            borrow_reserve: read_pubkey(data, base + OBL_LIQ_BORROW_RESERVE_OFF),
            borrowed_amount_wads: read_u128_le(data, base + OBL_LIQ_BORROWED_AMOUNT_WADS_OFF),
        });
    }

    Ok(SolendObligationRaw {
        version,
        last_update_slot,
        last_update_stale,
        lending_market,
        owner,
        deposits,
        borrows,
    })
}

/// Decode the read-model-minimum subset of a Solend Reserve account.
///
/// Only the fields Part 6B §64.2 lists as required for the Deposit-only
/// slice are populated. Borrow-rate / config / rate-limiter fields are
/// skipped.
pub fn decode_reserve(data: &[u8]) -> Result<SolendReserveRaw, DecodeError> {
    if data.len() != RESERVE_LEN {
        return Err(DecodeError::ReserveWrongSize(data.len()));
    }

    let version = data[RES_VERSION_OFF];
    let last_update_slot = read_u64_le(data, RES_LAST_UPDATE_SLOT_OFF);
    let last_update_stale = read_bool(data, RES_LAST_UPDATE_STALE_OFF)
        .ok_or(DecodeError::ReserveStaleBitInvalid(
            data[RES_LAST_UPDATE_STALE_OFF],
        ))?;

    Ok(SolendReserveRaw {
        version,
        last_update_slot,
        last_update_stale,
        lending_market: read_pubkey(data, RES_LENDING_MARKET_OFF),
        liquidity_mint: read_pubkey(data, RES_LIQ_MINT_OFF),
        liquidity_mint_decimals: data[RES_LIQ_MINT_DECIMALS_OFF],
        liquidity_supply: read_pubkey(data, RES_LIQ_SUPPLY_OFF),
        pyth_oracle: read_pubkey(data, RES_LIQ_PYTH_ORACLE_OFF),
        switchboard_oracle: read_pubkey(data, RES_LIQ_SWITCHBOARD_ORACLE_OFF),
        liquidity_available_amount: read_u64_le(data, RES_LIQ_AVAILABLE_AMOUNT_OFF),
        collateral_mint: read_pubkey(data, RES_COLL_MINT_OFF),
        collateral_supply: read_pubkey(data, RES_COLL_SUPPLY_OFF),
    })
}

/// True if the given pubkey is Solend's literal "nu11…" sentinel meaning
/// "no oracle configured for this slot." Recognition must happen here,
/// at the raw layer, before any RPC fetch is attempted against the slot.
pub fn is_null_oracle_sentinel(pk: &Pubkey) -> bool {
    pk.to_string() == SOLEND_NULL_ORACLE_SENTINEL_BS58
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic but fully-specified 1300-byte obligation. The
    /// result decodes back to the input exactly — this fixture doubles as
    /// a per-offset audit.
    pub(crate) fn synth_obligation(
        owner: Pubkey,
        lending_market: Pubkey,
        last_update_slot: u64,
        stale: bool,
        deposits: &[SolendObligationCollateralRaw],
        borrows: &[SolendObligationLiquidityRaw],
    ) -> Vec<u8> {
        let mut out = vec![0u8; OBLIGATION_LEN];
        out[OBL_VERSION_OFF] = 1;
        out[OBL_LAST_UPDATE_SLOT_OFF..OBL_LAST_UPDATE_SLOT_OFF + 8]
            .copy_from_slice(&last_update_slot.to_le_bytes());
        out[OBL_LAST_UPDATE_STALE_OFF] = stale as u8;
        out[OBL_LENDING_MARKET_OFF..OBL_LENDING_MARKET_OFF + 32]
            .copy_from_slice(&lending_market.to_bytes());
        out[OBL_OWNER_OFF..OBL_OWNER_OFF + 32].copy_from_slice(&owner.to_bytes());
        out[OBL_DEPOSITS_LEN_OFF] = deposits.len() as u8;
        out[OBL_BORROWS_LEN_OFF] = borrows.len() as u8;

        for (i, d) in deposits.iter().enumerate() {
            let base = OBL_DATA_FLAT_OFF + i * OBLIGATION_COLLATERAL_LEN;
            out[base + OBL_COLL_DEPOSIT_RESERVE_OFF..base + OBL_COLL_DEPOSIT_RESERVE_OFF + 32]
                .copy_from_slice(&d.deposit_reserve.to_bytes());
            out[base + OBL_COLL_DEPOSITED_AMOUNT_OFF..base + OBL_COLL_DEPOSITED_AMOUNT_OFF + 8]
                .copy_from_slice(&d.deposited_amount.to_le_bytes());
        }
        let borrows_start =
            OBL_DATA_FLAT_OFF + deposits.len() * OBLIGATION_COLLATERAL_LEN;
        for (i, b) in borrows.iter().enumerate() {
            let base = borrows_start + i * OBLIGATION_LIQUIDITY_LEN;
            out[base + OBL_LIQ_BORROW_RESERVE_OFF..base + OBL_LIQ_BORROW_RESERVE_OFF + 32]
                .copy_from_slice(&b.borrow_reserve.to_bytes());
            out[base + OBL_LIQ_BORROWED_AMOUNT_WADS_OFF
                ..base + OBL_LIQ_BORROWED_AMOUNT_WADS_OFF + 16]
                .copy_from_slice(&b.borrowed_amount_wads.to_le_bytes());
        }
        out
    }

    /// Build a synthetic 619-byte reserve.
    pub(crate) fn synth_reserve(
        lending_market: Pubkey,
        mint: Pubkey,
        decimals: u8,
        liquidity_supply: Pubkey,
        pyth_oracle: Pubkey,
        switchboard_oracle: Pubkey,
        available: u64,
        collateral_mint: Pubkey,
        collateral_supply: Pubkey,
        last_update_slot: u64,
        stale: bool,
    ) -> Vec<u8> {
        let mut out = vec![0u8; RESERVE_LEN];
        out[RES_VERSION_OFF] = 1;
        out[RES_LAST_UPDATE_SLOT_OFF..RES_LAST_UPDATE_SLOT_OFF + 8]
            .copy_from_slice(&last_update_slot.to_le_bytes());
        out[RES_LAST_UPDATE_STALE_OFF] = stale as u8;
        out[RES_LENDING_MARKET_OFF..RES_LENDING_MARKET_OFF + 32]
            .copy_from_slice(&lending_market.to_bytes());
        out[RES_LIQ_MINT_OFF..RES_LIQ_MINT_OFF + 32].copy_from_slice(&mint.to_bytes());
        out[RES_LIQ_MINT_DECIMALS_OFF] = decimals;
        out[RES_LIQ_SUPPLY_OFF..RES_LIQ_SUPPLY_OFF + 32]
            .copy_from_slice(&liquidity_supply.to_bytes());
        out[RES_LIQ_PYTH_ORACLE_OFF..RES_LIQ_PYTH_ORACLE_OFF + 32]
            .copy_from_slice(&pyth_oracle.to_bytes());
        out[RES_LIQ_SWITCHBOARD_ORACLE_OFF..RES_LIQ_SWITCHBOARD_ORACLE_OFF + 32]
            .copy_from_slice(&switchboard_oracle.to_bytes());
        out[RES_LIQ_AVAILABLE_AMOUNT_OFF..RES_LIQ_AVAILABLE_AMOUNT_OFF + 8]
            .copy_from_slice(&available.to_le_bytes());
        out[RES_COLL_MINT_OFF..RES_COLL_MINT_OFF + 32]
            .copy_from_slice(&collateral_mint.to_bytes());
        out[RES_COLL_SUPPLY_OFF..RES_COLL_SUPPLY_OFF + 32]
            .copy_from_slice(&collateral_supply.to_bytes());
        out
    }

    #[test]
    fn obligation_wrong_size_rejected() {
        let short = vec![0u8; OBLIGATION_LEN - 1];
        assert_eq!(
            decode_obligation(&short),
            Err(DecodeError::ObligationWrongSize(OBLIGATION_LEN - 1))
        );
    }

    #[test]
    fn reserve_wrong_size_rejected() {
        let long = vec![0u8; RESERVE_LEN + 1];
        assert_eq!(
            decode_reserve(&long),
            Err(DecodeError::ReserveWrongSize(RESERVE_LEN + 1))
        );
    }

    #[test]
    fn obligation_padding_nonzero_rejected() {
        let owner = Pubkey::new_unique();
        let mkt = Pubkey::new_unique();
        let mut bytes = synth_obligation(owner, mkt, 100, false, &[], &[]);
        bytes[OBL_PADDING_OFF + 10] = 0xFF; // corrupt middle of padding
        assert_eq!(
            decode_obligation(&bytes),
            Err(DecodeError::ObligationPaddingNonZero)
        );
    }

    #[test]
    fn obligation_array_overflow_rejected() {
        let owner = Pubkey::new_unique();
        let mkt = Pubkey::new_unique();
        let mut bytes = synth_obligation(owner, mkt, 100, false, &[], &[]);
        // Claim far more deposits than data_flat can hold:
        //   (15 * 88) + (5 * 112) = 1320 + 560 = 1880 > 1096
        bytes[OBL_DEPOSITS_LEN_OFF] = 15;
        bytes[OBL_BORROWS_LEN_OFF] = 5;
        assert_eq!(
            decode_obligation(&bytes),
            Err(DecodeError::ObligationArrayOverflow {
                deposits: 15,
                borrows: 5
            })
        );
    }

    #[test]
    fn obligation_stale_bit_invalid_rejected() {
        let owner = Pubkey::new_unique();
        let mkt = Pubkey::new_unique();
        let mut bytes = synth_obligation(owner, mkt, 100, false, &[], &[]);
        bytes[OBL_LAST_UPDATE_STALE_OFF] = 2;
        assert_eq!(
            decode_obligation(&bytes),
            Err(DecodeError::ObligationStaleBitInvalid(2))
        );
    }

    #[test]
    fn obligation_header_roundtrip() {
        let owner = Pubkey::new_unique();
        let mkt = Pubkey::new_unique();
        let bytes = synth_obligation(owner, mkt, 414_000_000, true, &[], &[]);
        let out = decode_obligation(&bytes).expect("decode");
        assert_eq!(out.owner, owner);
        assert_eq!(out.lending_market, mkt);
        assert_eq!(out.last_update_slot, 414_000_000);
        assert!(out.last_update_stale);
        assert_eq!(out.deposits.len(), 0);
        assert_eq!(out.borrows.len(), 0);
    }

    #[test]
    fn obligation_deposits_and_borrows_roundtrip() {
        let owner = Pubkey::new_unique();
        let mkt = Pubkey::new_unique();
        let r1 = Pubkey::new_unique();
        let r2 = Pubkey::new_unique();
        let deposits = vec![
            SolendObligationCollateralRaw {
                deposit_reserve: r1,
                deposited_amount: 12_457,
            },
            SolendObligationCollateralRaw {
                deposit_reserve: r2,
                deposited_amount: 96_907,
            },
        ];
        let borrows = vec![SolendObligationLiquidityRaw {
            borrow_reserve: r1,
            borrowed_amount_wads: 56_641_326_427_023_044_032_452u128,
        }];
        let bytes = synth_obligation(owner, mkt, 100, false, &deposits, &borrows);
        let out = decode_obligation(&bytes).expect("decode");
        assert_eq!(out.deposits, deposits);
        assert_eq!(out.borrows, borrows);
    }

    #[test]
    fn reserve_roundtrip() {
        let mkt = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let supply = Pubkey::new_unique();
        let pyth = Pubkey::new_unique();
        let swb = Pubkey::new_unique();
        let c_mint = Pubkey::new_unique();
        let c_supply = Pubkey::new_unique();
        let bytes = synth_reserve(
            mkt,
            mint,
            6,
            supply,
            pyth,
            swb,
            1_782_132_281,
            c_mint,
            c_supply,
            397_756_108,
            true,
        );
        let out = decode_reserve(&bytes).expect("decode");
        assert_eq!(out.version, 1);
        assert_eq!(out.last_update_slot, 397_756_108);
        assert!(out.last_update_stale);
        assert_eq!(out.lending_market, mkt);
        assert_eq!(out.liquidity_mint, mint);
        assert_eq!(out.liquidity_mint_decimals, 6);
        assert_eq!(out.liquidity_supply, supply);
        assert_eq!(out.pyth_oracle, pyth);
        assert_eq!(out.switchboard_oracle, swb);
        assert_eq!(out.liquidity_available_amount, 1_782_132_281);
        assert_eq!(out.collateral_mint, c_mint);
        assert_eq!(out.collateral_supply, c_supply);
    }

    #[test]
    fn sentinel_recognition() {
        let sentinel: Pubkey = SOLEND_NULL_ORACLE_SENTINEL_BS58.parse().expect("parse");
        assert!(is_null_oracle_sentinel(&sentinel));
        assert!(!is_null_oracle_sentinel(&Pubkey::new_unique()));
    }
}

// Re-export synth helpers for the `mapping` tests.
#[cfg(test)]
pub(crate) use tests::{synth_obligation, synth_reserve};
