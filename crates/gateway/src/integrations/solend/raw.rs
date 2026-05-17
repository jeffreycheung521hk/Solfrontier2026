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
// Citation: solendprotocol/solana-program-library, `mainnet` branch,
//   commit d04ce00bbf4356c4fd32b3be38eb9760b696bb3e, file
//   `token-lending/sdk/src/state/obligation.rs`, `Pack` impl
//   (`mut_array_refs!` / `array_refs!` token lists verified end-to-end).
//
// IMPORTANT — mainnet / master drift (Slice 3H):
//   The master branch treats bytes 138..202 as one 64-byte zero-padding
//   block. Mainnet uses that region for five real fields (50 bytes) and
//   leaves a smaller true padding of 14 bytes at 188..202. A decoder
//   that enforces master's 64-byte zero invariant will falsely reject
//   any mainnet obligation whose values have become non-zero (e.g.,
//   after a borrow, post-interest-accrual, or when flagged closeable).
//   See below for the exact byte walk.
//
//   0     1    version
//   1     8    last_update.slot (u64 LE)
//   9     1    last_update.stale (bool, 0 or 1)
//   10    32   lending_market (Pubkey)
//   42    32   owner (Pubkey)
//   74    16   deposited_value (Decimal wad)              — SKIPPED
//   90    16   borrowed_value (Decimal wad)               — SKIPPED
//   106   16   allowed_borrow_value (Decimal wad)         — SKIPPED
//   122   16   unhealthy_borrow_value (Decimal wad)       — SKIPPED
//   138   16   borrowed_value_upper_bound (Decimal wad)   ← MAINNET EXTENSION
//   154   1    borrowing_isolated_asset (bool, 0 or 1)    ← MAINNET EXTENSION
//   155   16   super_unhealthy_borrow_value (Decimal wad) ← MAINNET EXTENSION
//   171   16   unweighted_borrowed_value (Decimal wad)    ← MAINNET EXTENSION
//   187   1    closeable (bool, 0 or 1)                   ← MAINNET EXTENSION
//   188   14   _padding [u8; 14] — must be all zero
//   202   1    deposits_len (u8)
//   203   1    borrows_len (u8)
//   204   1096 data_flat: deposits[] (88 bytes each) then borrows[] (112 bytes each)
//
// Byte math for the extension block (138..188):
//   16 (borrowed_value_upper_bound)
// +  1 (borrowing_isolated_asset)
// + 16 (super_unhealthy_borrow_value)
// + 16 (unweighted_borrowed_value)
// +  1 (closeable)
// = 50 bytes  →  ends at offset 188
// True padding: 188..202 = 14 bytes  →  ends at offset 202 (deposits_len)
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

// Slice 3H mainnet-extension offsets. Each comment states the 1- or
// 16-byte size explicitly so the byte walk visibly adds up.
const OBL_BORROWED_VALUE_UPPER_BOUND_OFF: usize = 138;        // 16 bytes
const OBL_BORROWING_ISOLATED_ASSET_OFF: usize = 154;          // 1 byte
const OBL_SUPER_UNHEALTHY_BORROW_VALUE_OFF: usize = 155;      // 16 bytes
const OBL_UNWEIGHTED_BORROWED_VALUE_OFF: usize = 171;         // 16 bytes
const OBL_CLOSEABLE_OFF: usize = 187;                         // 1 byte

// True padding (mainnet): 14 bytes at 188..202. Master branch's 64-byte
// range at 138..202 is NOT enforced here any more.
const OBL_PADDING_OFF: usize = 188;
const OBL_PADDING_LEN: usize = 14;

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
// Citation: solendprotocol/solana-program-library, `mainnet` branch
//   (commit d04ce00bbf4356c4fd32b3be38eb9760b696bb3e). The deployed
//   mainnet program is built from this branch, not `master`. Offsets
//   below were re-verified against `token-lending/sdk/src/state/reserve.rs`
//   at that commit. Any field visible at an offset named here was confirmed
//   by walking the `array_mut_refs!` macro size list end-to-end.
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
//   179   16   liquidity.borrowed_amount_wads (u128 LE; Decimal scaled_val)
//   195   16   liquidity.cumulative_borrow_rate_wads     — SKIPPED
//   211   16   liquidity.market_price                    — SKIPPED
//   227   32   collateral.mint_pubkey (Pubkey)
//   259   8    collateral.mint_total_supply              — SKIPPED
//   267   32   collateral.supply_pubkey (Pubkey)
//   299   1    config.optimal_utilization_rate (u8)      — READ (B-O1)
//   300   1    config.loan_to_value_ratio                — SKIPPED
//   301   1    config.liquidation_bonus                  — SKIPPED
//   302   1    config.liquidation_threshold              — SKIPPED
//   303   1    config.min_borrow_rate (u8)               — READ (B-O1)
//   304   1    config.optimal_borrow_rate (u8)           — READ (B-O1)
//   305   1    config.max_borrow_rate (u8)               — READ (B-O1)
//   306   8    config.fees.borrow_fee_wad                — SKIPPED
//   314   8    config.fees.flash_loan_fee_wad            — SKIPPED
//   322   1    config.fees.host_fee_percentage           — SKIPPED
//   323   8    config.deposit_limit (u64 LE)             — READ (Slice 3D(iii))
//   331   8    config.borrow_limit                       — SKIPPED
//   339   32   config.fee_receiver                       — SKIPPED
//   371   1    config.protocol_liquidation_fee           — SKIPPED
//   372   1    config.protocol_take_rate (u8)            — READ (B-O1)
//   373   16   liquidity.accumulated_protocol_fees_wads  — READ (Slice 3D(iii))
//   389   56   rate_limiter (RATE_LIMITER_LEN = 56)      — SKIPPED
//   445   8    config.added_borrow_weight_bps            — SKIPPED
//   453   16   liquidity.smoothed_market_price           — SKIPPED
//   469   1    config.asset_type / reserve_type          — SKIPPED
//   470   1    config.max_utilization_rate (u8)          — READ (B-O1)
//   471   8    config.super_max_borrow_rate (u64 LE)     — READ (B-O1)
//   479   1    config.max_liquidation_bonus              — SKIPPED
//   480   1    config.max_liquidation_threshold          — SKIPPED
//   481   8    config.scaled_price_offset_bps            — SKIPPED
//   489   32   config.extra_oracle_pubkey                — SKIPPED
//   521   1    liquidity.extra_market_price_flag         — SKIPPED
//   522   16   liquidity.extra_market_price              — SKIPPED
//   538   16   attributed_borrow_value                   — SKIPPED
//   554   8    config.attributed_borrow_limit_open       — SKIPPED
//   562   8    config.attributed_borrow_limit_close      — SKIPPED
//   570   49   _padding                                  — SKIPPED
//   619 total  (RESERVE_LEN)
//
// **B-O1 verification (2026-05-11)** — the 7 rate-config fields
// flagged READ above were re-verified against the upstream
// `solendprotocol/solana-program-library` mainnet branch source at
// `token-lending/sdk/src/state/reserve.rs` (line 1245-onwards). The
// `mut_array_refs!` field order in `impl Pack for Reserve` and the
// `RATE_LIMITER_LEN = 56` constant from
// `token-lending/sdk/src/state/rate_limiter.rs` give the exact
// offsets pinned in the constants below. The cumulative sum
// (1+8+1+32+32+1+32+32+32+8+16+16+16+32+8+32+1+1+1+1+1+1+1+8+8+1+8+8+
// 32+1+1+16+56+8+16+1+1+8+1+1+8+32+1+16+16+8+8+49) equals 619 ==
// RESERVE_LEN; a divergence here would break Pack invariance and is
// caught by `reserve_rate_config_fields_roundtrip`.

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
const RES_LIQ_BORROWED_AMOUNT_WADS_OFF: usize = 179;
const RES_COLL_MINT_OFF: usize = 227;
const RES_COLL_SUPPLY_OFF: usize = 267;
const RES_CONFIG_DEPOSIT_LIMIT_OFF: usize = 323;
const RES_LIQ_ACCUMULATED_PROTOCOL_FEES_WADS_OFF: usize = 373;

// Stage 2 B-O1 — rate-config offsets verified against
// solendprotocol/solana-program-library, `mainnet` branch,
// `token-lending/sdk/src/state/reserve.rs::pack_into_slice` field
// order at the `mut_array_refs!` macro call. The first 5 offsets fall
// within the existing decoder's read window (≤ 372) and were already
// documented in the byte-layout comment above; the last 2 fall in the
// "remaining extension fields" range (offsets 470-471) that the
// research doc `STAGE2_SOLEND_APY_RESEARCH.md` § B-O1 flagged as
// open. They are derived deterministically from `RATE_LIMITER_LEN =
// 56` (`token-lending/sdk/src/state/rate_limiter.rs`) plus the
// post-rate-limiter field widths (added_borrow_weight_bps:u64=8,
// liquidity_smoothed_market_price:u128=16, asset_type:u8=1) which
// puts `config_max_utilization_rate` at 389+56+8+16+1 = 470 and
// `config_super_max_borrow_rate` at 471.
const RES_CONFIG_OPTIMAL_UTILIZATION_RATE_OFF: usize = 299;
const RES_CONFIG_MIN_BORROW_RATE_OFF: usize = 303;
const RES_CONFIG_OPTIMAL_BORROW_RATE_OFF: usize = 304;
const RES_CONFIG_MAX_BORROW_RATE_OFF: usize = 305;
const RES_CONFIG_PROTOCOL_TAKE_RATE_OFF: usize = 372;
const RES_CONFIG_MAX_UTILIZATION_RATE_OFF: usize = 470;
const RES_CONFIG_SUPER_MAX_BORROW_RATE_OFF: usize = 471;

/// Decimal wad scale used by Solend (`10^18`). The packed representation
/// of any Solend `Decimal` field is the `u128` scaled-value where one wad
/// equals `10^18`. See `math/decimal.rs` in the mainnet branch.
pub const SOLEND_WAD: u128 = 1_000_000_000_000_000_000;

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

    // ── Slice 3H mainnet extension fields (138..188) ────────────────────
    //
    // All Decimal/wad fields are stored as the on-chain u128
    // scaled-value little-endian exactly as they appear on chain. NO
    // scaling, division, or floating-point conversion happens in the
    // raw layer — any downstream consumer that needs a UI value must
    // apply its own divisor (typically `SOLEND_WAD`).
    //
    // Boolean fields mirror Solend's `unpack_bool` strictness: only
    // bytes `0` or `1` decode; any other value causes the decoder to
    // fail closed with `DecodeError::ObligationBoolInvalid`. This
    // matches the deployed program's behavior (Err on non-canonical
    // bool bytes), and is stricter than needed for a read-model but
    // safer for an anti-corruption boundary.
    pub borrowed_value_upper_bound_wads: u128,
    pub borrowing_isolated_asset: bool,
    pub super_unhealthy_borrow_value_wads: u128,
    pub unweighted_borrowed_value_wads: u128,
    pub closeable: bool,
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
    /// Scaled-value (wad) form of `Decimal borrowed_amount_wads`. To
    /// interpret as "tokens", divide by `SOLEND_WAD`. Used by the deployed
    /// `_deposit_reserve_liquidity` cap check.
    pub liquidity_borrowed_amount_wads: u128,
    /// Scaled-value (wad) form of `Decimal accumulated_protocol_fees_wads`.
    /// Subtracted inside `ReserveLiquidity::total_supply` on the mainnet
    /// branch. Required for any accurate headroom computation.
    pub liquidity_accumulated_protocol_fees_wads: u128,
    pub collateral_mint: Pubkey,
    pub collateral_supply: Pubkey,
    /// `ReserveConfig::deposit_limit`: maximum floor-of-total-supply in
    /// underlying base units. Zero here means deposits are disabled on
    /// this reserve via config, not that the reserve is "empty".
    pub config_deposit_limit: u64,

    // ── Stage 2 B-O1 — rate-config fields used by W3 evaluator's
    // `supply_apr_wad` path. All offsets pinned against the upstream
    // `mainnet` branch `pack_into_slice` field order; see the byte-layout
    // comment block at the top of this file. Without these, a live
    // SolendReserveRaw cannot be mapped to a `Stage2SnapshotValue` and
    // the APR comparator cannot be evaluated against live state.
    /// `ReserveConfig::optimal_utilization_rate` — percent (`u8`), 0..=100.
    /// Read from byte offset 299. Used as the breakpoint between the
    /// linear and kinked regions of the borrow-rate curve.
    pub config_optimal_utilization_rate_pct: u8,
    /// `ReserveConfig::min_borrow_rate` — percent (`u8`), 0..=100.
    /// Read from byte offset 303. Floor of the kinked borrow-rate curve.
    pub config_min_borrow_rate_pct: u8,
    /// `ReserveConfig::optimal_borrow_rate` — percent (`u8`), 0..=100.
    /// Read from byte offset 304. Rate at `optimal_utilization_rate`.
    pub config_optimal_borrow_rate_pct: u8,
    /// `ReserveConfig::max_borrow_rate` — percent (`u8`), 0..=100.
    /// Read from byte offset 305. Rate at `max_utilization_rate`.
    pub config_max_borrow_rate_pct: u8,
    /// `ReserveConfig::protocol_take_rate` — percent (`u8`), 0..=100.
    /// Read from byte offset 372. Fraction of supply yield that goes
    /// to the protocol; `supply_apr = borrow_apr × util × (1 − take)`.
    pub config_protocol_take_rate_pct: u8,
    /// `ReserveConfig::max_utilization_rate` — percent (`u8`), 0..=100.
    /// Read from byte offset 470 (mainnet extension field). Breakpoint
    /// between the kinked and super-kinked regions of the curve.
    pub config_max_utilization_rate_pct: u8,
    /// `ReserveConfig::super_max_borrow_rate` — percent (`u64`).
    /// Read from byte offset 471 (mainnet extension field). Width is
    /// `u64` not `u8` — the deployed mainnet program uses values that
    /// can exceed 255% in stress scenarios; truncating to `u8` would
    /// silently break the third region of the kinked rate model (see
    /// audit C-3 in `stage2_evaluator.rs`).
    pub config_super_max_borrow_rate_pct: u64,
}

impl SolendReserveRaw {
    /// Reconstruct Solend's `ReserveLiquidity::total_supply`, in underlying
    /// base units, exactly as the deployed deposit-limit check evaluates
    /// it at `processor.rs` (mainnet branch commit
    /// `d04ce00bbf4356c4fd32b3be38eb9760b696bb3e`):
    ///
    /// ```text
    /// floor( available_amount * WAD
    ///      + borrowed_amount_wads
    ///      - accumulated_protocol_fees_wads ) / WAD
    /// ```
    ///
    /// Returns `None` if the subtraction would underflow (on-chain
    /// invariant: accumulated fees never exceed available + borrowed,
    /// so underflow would indicate a decode error or state corruption).
    pub fn liquidity_total_supply_raw(&self) -> Option<u64> {
        let available_wads = (self.liquidity_available_amount as u128).checked_mul(SOLEND_WAD)?;
        let summed = available_wads.checked_add(self.liquidity_borrowed_amount_wads)?;
        let after_fees = summed.checked_sub(self.liquidity_accumulated_protocol_fees_wads)?;
        let floored = after_fees / SOLEND_WAD;
        u64::try_from(floored).ok()
    }

    /// Maximum additional underlying amount that would still satisfy the
    /// deployed deposit-limit check, in base units. Returns `0` if the
    /// cap is at or below current `total_supply`.
    ///
    /// The deployed check is:
    /// `floor( Decimal::from(liquidity_amount) + total_supply ) > deposit_limit → reject`
    /// so the largest accepted amount is `deposit_limit - total_supply`
    /// (when `total_supply <= deposit_limit`). This helper returns that
    /// saturating difference.
    pub fn deposit_headroom_raw(&self) -> u64 {
        let Some(total) = self.liquidity_total_supply_raw() else {
            return 0;
        };
        self.config_deposit_limit.saturating_sub(total)
    }
}

// ── Decode errors ────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error("obligation bytes wrong length: expected {OBLIGATION_LEN}, got {0}")]
    ObligationWrongSize(usize),
    #[error("reserve bytes wrong length: expected {RESERVE_LEN}, got {0}")]
    ReserveWrongSize(usize),
    #[error("obligation padding at bytes 188..202 is not all zero")]
    ObligationPaddingNonZero,
    #[error("obligation arrays overflow data_flat: deposits_len={deposits}, borrows_len={borrows}")]
    ObligationArrayOverflow { deposits: u8, borrows: u8 },
    #[error("obligation stale bit at offset 9 is {0}, expected 0 or 1")]
    ObligationStaleBitInvalid(u8),
    #[error("reserve stale bit at offset 9 is {0}, expected 0 or 1")]
    ReserveStaleBitInvalid(u8),
    /// Slice 3H: mainnet extension bools (`borrowing_isolated_asset` at
    /// offset 154, `closeable` at offset 187) MUST be `0` or `1`.
    /// Mirrors Solend's `unpack_bool` strictness on the deployed
    /// program; a non-canonical byte here means the account data does
    /// not round-trip through `Pack`.
    #[error("obligation bool at offset {offset} is {value}, expected 0 or 1 (field: {field})")]
    ObligationBoolInvalid {
        offset: usize,
        value: u8,
        field: &'static str,
    },
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
/// Slice 3H (2026-04-23): the decoder now matches the deployed **mainnet**
/// `Pack` layout, commit `d04ce00bbf4356c4fd32b3be38eb9760b696bb3e`.
/// Bytes 138..188 carry five real extension fields (previously treated
/// as padding); the true padding range is 188..202. `deposited_value` /
/// `borrowed_value` / `allowed_borrow_value` / `unhealthy_borrow_value`
/// at offsets 74..138 remain intentionally SKIPPED because V1 policy
/// does not consume them.
pub fn decode_obligation(data: &[u8]) -> Result<SolendObligationRaw, DecodeError> {
    if data.len() != OBLIGATION_LEN {
        return Err(DecodeError::ObligationWrongSize(data.len()));
    }

    // Integrity check: the 14-byte mainnet padding block 188..202 must
    // be all zero. A non-zero padding block signals either layout drift
    // or a different program version. Note: this range is SMALLER than
    // the old master-branch assumption (138..202) because the mainnet
    // program uses 138..188 for the five extension fields decoded
    // below.
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

    // ── Mainnet extension fields (138..188) ─────────────────────────────
    //
    // Wad-scaled Decimal fields are stored as the raw `u128` little-
    // endian scaled value. No scaling, no f64, no division in the raw
    // layer. Booleans are strict: only `0` or `1` accepted.
    let borrowed_value_upper_bound_wads =
        read_u128_le(data, OBL_BORROWED_VALUE_UPPER_BOUND_OFF);
    let borrowing_isolated_asset = read_bool(data, OBL_BORROWING_ISOLATED_ASSET_OFF)
        .ok_or(DecodeError::ObligationBoolInvalid {
            offset: OBL_BORROWING_ISOLATED_ASSET_OFF,
            value: data[OBL_BORROWING_ISOLATED_ASSET_OFF],
            field: "borrowing_isolated_asset",
        })?;
    let super_unhealthy_borrow_value_wads =
        read_u128_le(data, OBL_SUPER_UNHEALTHY_BORROW_VALUE_OFF);
    let unweighted_borrowed_value_wads =
        read_u128_le(data, OBL_UNWEIGHTED_BORROWED_VALUE_OFF);
    let closeable = read_bool(data, OBL_CLOSEABLE_OFF)
        .ok_or(DecodeError::ObligationBoolInvalid {
            offset: OBL_CLOSEABLE_OFF,
            value: data[OBL_CLOSEABLE_OFF],
            field: "closeable",
        })?;

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
        borrowed_value_upper_bound_wads,
        borrowing_isolated_asset,
        super_unhealthy_borrow_value_wads,
        unweighted_borrowed_value_wads,
        closeable,
    })
}

/// Decode the read-model-minimum subset of a Solend Reserve account.
///
/// Reads the Deposit-only Part 6B §64.2 fields **plus** the 7 rate-config
/// fields the Stage 2 W3 evaluator's `supply_apr_wad` path needs (Stage 2
/// B-O1, May 2026). The borrow-fee config, fee-receiver, rate-limiter
/// state, and most extension-padding fields remain skipped because no
/// current evaluation surface reads them.
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
        liquidity_borrowed_amount_wads: read_u128_le(
            data,
            RES_LIQ_BORROWED_AMOUNT_WADS_OFF,
        ),
        liquidity_accumulated_protocol_fees_wads: read_u128_le(
            data,
            RES_LIQ_ACCUMULATED_PROTOCOL_FEES_WADS_OFF,
        ),
        collateral_mint: read_pubkey(data, RES_COLL_MINT_OFF),
        collateral_supply: read_pubkey(data, RES_COLL_SUPPLY_OFF),
        config_deposit_limit: read_u64_le(data, RES_CONFIG_DEPOSIT_LIMIT_OFF),
        config_optimal_utilization_rate_pct: data
            [RES_CONFIG_OPTIMAL_UTILIZATION_RATE_OFF],
        config_min_borrow_rate_pct: data[RES_CONFIG_MIN_BORROW_RATE_OFF],
        config_optimal_borrow_rate_pct: data[RES_CONFIG_OPTIMAL_BORROW_RATE_OFF],
        config_max_borrow_rate_pct: data[RES_CONFIG_MAX_BORROW_RATE_OFF],
        config_protocol_take_rate_pct: data[RES_CONFIG_PROTOCOL_TAKE_RATE_OFF],
        config_max_utilization_rate_pct: data[RES_CONFIG_MAX_UTILIZATION_RATE_OFF],
        config_super_max_borrow_rate_pct: read_u64_le(
            data,
            RES_CONFIG_SUPER_MAX_BORROW_RATE_OFF,
        ),
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

    /// Build a synthetic but fully-specified 1300-byte obligation with
    /// all mainnet extension fields at zero. Result decodes back to the
    /// input exactly — this fixture doubles as a per-offset audit.
    /// Existing downstream callers in `mapping.rs` / `policy.rs` use
    /// this signature; Slice 3H tests that need non-zero extension
    /// fields should use
    /// [`synth_obligation_with_extension_fields`] instead.
    pub(crate) fn synth_obligation(
        owner: Pubkey,
        lending_market: Pubkey,
        last_update_slot: u64,
        stale: bool,
        deposits: &[SolendObligationCollateralRaw],
        borrows: &[SolendObligationLiquidityRaw],
    ) -> Vec<u8> {
        synth_obligation_with_extension_fields(
            owner,
            lending_market,
            last_update_slot,
            stale,
            deposits,
            borrows,
            ObligationExtensionFields::default(),
        )
    }

    /// Slice 3H extension helper: lets tests set the mainnet-layout
    /// extension fields at their exact offsets. Defaults are all zero
    /// (via `ObligationExtensionFields::default()`), matching a
    /// freshly-initialized obligation (like the Slice 3G live one).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn synth_obligation_with_extension_fields(
        owner: Pubkey,
        lending_market: Pubkey,
        last_update_slot: u64,
        stale: bool,
        deposits: &[SolendObligationCollateralRaw],
        borrows: &[SolendObligationLiquidityRaw],
        ext: ObligationExtensionFields,
    ) -> Vec<u8> {
        let mut out = vec![0u8; OBLIGATION_LEN];
        out[OBL_VERSION_OFF] = 1;
        out[OBL_LAST_UPDATE_SLOT_OFF..OBL_LAST_UPDATE_SLOT_OFF + 8]
            .copy_from_slice(&last_update_slot.to_le_bytes());
        out[OBL_LAST_UPDATE_STALE_OFF] = stale as u8;
        out[OBL_LENDING_MARKET_OFF..OBL_LENDING_MARKET_OFF + 32]
            .copy_from_slice(&lending_market.to_bytes());
        out[OBL_OWNER_OFF..OBL_OWNER_OFF + 32].copy_from_slice(&owner.to_bytes());
        // Extension fields at 138..188. Byte walk:
        //   16 (borrowed_value_upper_bound)
        // +  1 (borrowing_isolated_asset)
        // + 16 (super_unhealthy_borrow_value)
        // + 16 (unweighted_borrowed_value)
        // +  1 (closeable)
        // = 50 bytes → ends at 188
        out[OBL_BORROWED_VALUE_UPPER_BOUND_OFF..OBL_BORROWED_VALUE_UPPER_BOUND_OFF + 16]
            .copy_from_slice(&ext.borrowed_value_upper_bound_wads.to_le_bytes());
        out[OBL_BORROWING_ISOLATED_ASSET_OFF] = ext.borrowing_isolated_asset as u8;
        out[OBL_SUPER_UNHEALTHY_BORROW_VALUE_OFF..OBL_SUPER_UNHEALTHY_BORROW_VALUE_OFF + 16]
            .copy_from_slice(&ext.super_unhealthy_borrow_value_wads.to_le_bytes());
        out[OBL_UNWEIGHTED_BORROWED_VALUE_OFF..OBL_UNWEIGHTED_BORROWED_VALUE_OFF + 16]
            .copy_from_slice(&ext.unweighted_borrowed_value_wads.to_le_bytes());
        out[OBL_CLOSEABLE_OFF] = ext.closeable as u8;
        // True padding 188..202 stays zero from the vec! initialization.
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

    /// Slice 3H: bundle of the five mainnet extension fields.
    #[derive(Debug, Clone, Copy, Default)]
    pub(crate) struct ObligationExtensionFields {
        pub borrowed_value_upper_bound_wads: u128,
        pub borrowing_isolated_asset: bool,
        pub super_unhealthy_borrow_value_wads: u128,
        pub unweighted_borrowed_value_wads: u128,
        pub closeable: bool,
    }

    /// Build a synthetic 619-byte reserve. The deposit-limit-related
    /// fields (`borrowed_amount_wads`, `accumulated_protocol_fees_wads`,
    /// `config.deposit_limit`) are left zeroed so that the many existing
    /// callers in `mapping.rs` / `policy.rs` do not need to change. Tests
    /// that care about those fields should use
    /// [`synth_reserve_with_deposit_limit_fields`] instead.
    #[allow(clippy::too_many_arguments)]
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
        synth_reserve_with_deposit_limit_fields(
            lending_market,
            mint,
            decimals,
            liquidity_supply,
            pyth_oracle,
            switchboard_oracle,
            available,
            collateral_mint,
            collateral_supply,
            last_update_slot,
            stale,
            0,
            0,
            0,
        )
    }

    /// Slice 3D(iii) extension: build a synthetic reserve with the three
    /// deposit-limit-related fields populated. Kept in the raw layer so
    /// the offset bytes stay in one place and cannot drift against the
    /// production decoder.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn synth_reserve_with_deposit_limit_fields(
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
        borrowed_wads: u128,
        accumulated_fees_wads: u128,
        deposit_limit: u64,
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
        out[RES_LIQ_BORROWED_AMOUNT_WADS_OFF..RES_LIQ_BORROWED_AMOUNT_WADS_OFF + 16]
            .copy_from_slice(&borrowed_wads.to_le_bytes());
        out[RES_COLL_MINT_OFF..RES_COLL_MINT_OFF + 32]
            .copy_from_slice(&collateral_mint.to_bytes());
        out[RES_COLL_SUPPLY_OFF..RES_COLL_SUPPLY_OFF + 32]
            .copy_from_slice(&collateral_supply.to_bytes());
        out[RES_CONFIG_DEPOSIT_LIMIT_OFF..RES_CONFIG_DEPOSIT_LIMIT_OFF + 8]
            .copy_from_slice(&deposit_limit.to_le_bytes());
        out[RES_LIQ_ACCUMULATED_PROTOCOL_FEES_WADS_OFF
            ..RES_LIQ_ACCUMULATED_PROTOCOL_FEES_WADS_OFF + 16]
            .copy_from_slice(&accumulated_fees_wads.to_le_bytes());
        out
    }

    /// Stage 2 B-O1 — overlay the 7 rate-config bytes onto a reserve
    /// fixture already produced by [`synth_reserve_with_deposit_limit_fields`].
    /// Kept as a separate helper so existing deposit-limit tests are
    /// untouched.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn overlay_reserve_rate_config_fields(
        bytes: &mut [u8],
        optimal_utilization_rate_pct: u8,
        min_borrow_rate_pct: u8,
        optimal_borrow_rate_pct: u8,
        max_borrow_rate_pct: u8,
        protocol_take_rate_pct: u8,
        max_utilization_rate_pct: u8,
        super_max_borrow_rate_pct: u64,
    ) {
        assert_eq!(bytes.len(), RESERVE_LEN);
        bytes[RES_CONFIG_OPTIMAL_UTILIZATION_RATE_OFF] = optimal_utilization_rate_pct;
        bytes[RES_CONFIG_MIN_BORROW_RATE_OFF] = min_borrow_rate_pct;
        bytes[RES_CONFIG_OPTIMAL_BORROW_RATE_OFF] = optimal_borrow_rate_pct;
        bytes[RES_CONFIG_MAX_BORROW_RATE_OFF] = max_borrow_rate_pct;
        bytes[RES_CONFIG_PROTOCOL_TAKE_RATE_OFF] = protocol_take_rate_pct;
        bytes[RES_CONFIG_MAX_UTILIZATION_RATE_OFF] = max_utilization_rate_pct;
        bytes[RES_CONFIG_SUPER_MAX_BORROW_RATE_OFF
            ..RES_CONFIG_SUPER_MAX_BORROW_RATE_OFF + 8]
            .copy_from_slice(&super_max_borrow_rate_pct.to_le_bytes());
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

    // ── Slice 3H: mainnet Obligation extension-field regression tests ───

    /// (A) Zero-extension obligation still decodes. Mirrors the Slice 3G
    /// live post-deposit obligation state: a freshly-initialized
    /// obligation with all extension fields zero, 1 deposit, 0 borrows.
    /// Decoder must accept this unchanged.
    #[test]
    fn obligation_zero_extension_still_decodes() {
        let owner = Pubkey::new_unique();
        let mkt = Pubkey::new_unique();
        let reserve = Pubkey::new_unique();
        let deposits = vec![SolendObligationCollateralRaw {
            deposit_reserve: reserve,
            deposited_amount: 772,
        }];
        let bytes = synth_obligation_with_extension_fields(
            owner,
            mkt,
            415_083_795,
            true,
            &deposits,
            &[],
            ObligationExtensionFields::default(),
        );
        let out = decode_obligation(&bytes).expect("zero-extension must decode");
        assert_eq!(out.deposits.len(), 1);
        assert_eq!(out.borrows.len(), 0);
        assert_eq!(out.borrowed_value_upper_bound_wads, 0);
        assert!(!out.borrowing_isolated_asset);
        assert_eq!(out.super_unhealthy_borrow_value_wads, 0);
        assert_eq!(out.unweighted_borrowed_value_wads, 0);
        assert!(!out.closeable);
    }

    /// (B) CRITICAL regression: an obligation with non-zero values in
    /// ALL five mainnet extension fields decodes successfully, and each
    /// field round-trips. Before Slice 3H, this would have been
    /// rejected with `ObligationPaddingNonZero` because the old
    /// decoder required 138..202 to be all zero.
    #[test]
    fn obligation_nonzero_mainnet_extension_fields_decode_and_roundtrip() {
        let owner = Pubkey::new_unique();
        let mkt = Pubkey::new_unique();
        let ext = ObligationExtensionFields {
            borrowed_value_upper_bound_wads: 3_141_592_653_589_793_238u128,
            borrowing_isolated_asset: true,
            super_unhealthy_borrow_value_wads: 2_718_281_828_459_045_235u128,
            unweighted_borrowed_value_wads: 1_618_033_988_749_894_848u128,
            closeable: true,
        };
        let bytes =
            synth_obligation_with_extension_fields(owner, mkt, 100, false, &[], &[], ext);
        let out = decode_obligation(&bytes).expect("non-zero extension must decode");
        assert_eq!(
            out.borrowed_value_upper_bound_wads, ext.borrowed_value_upper_bound_wads,
            "borrowed_value_upper_bound_wads should round-trip"
        );
        assert!(out.borrowing_isolated_asset);
        assert_eq!(
            out.super_unhealthy_borrow_value_wads, ext.super_unhealthy_borrow_value_wads,
        );
        assert_eq!(out.unweighted_borrowed_value_wads, ext.unweighted_borrowed_value_wads);
        assert!(out.closeable);
    }

    /// (C) True padding (188..202) still fail-closes. A single non-zero
    /// byte anywhere in this 14-byte range must be rejected.
    #[test]
    fn obligation_true_padding_nonzero_still_rejected() {
        let owner = Pubkey::new_unique();
        let mkt = Pubkey::new_unique();
        let mut bytes = synth_obligation(owner, mkt, 100, false, &[], &[]);
        // Pick the first byte of true padding. Any byte in 188..202 works.
        bytes[OBL_PADDING_OFF] = 0x7F;
        assert_eq!(
            decode_obligation(&bytes),
            Err(DecodeError::ObligationPaddingNonZero)
        );
        // And the last byte of the true padding range (201) must also trip.
        let mut bytes2 = synth_obligation(owner, mkt, 100, false, &[], &[]);
        bytes2[OBL_PADDING_OFF + OBL_PADDING_LEN - 1] = 0x01;
        assert_eq!(
            decode_obligation(&bytes2),
            Err(DecodeError::ObligationPaddingNonZero)
        );
    }

    /// (D) The old false-padding assumption is gone: a non-zero byte at
    /// the START of the former-padding range (offset 138, now
    /// `borrowed_value_upper_bound`) MUST NOT trip a padding-violation
    /// error — it is a real decodable field. This test protects against
    /// accidentally reintroducing the old `138..202` zero-padding check.
    #[test]
    fn obligation_nonzero_inside_old_padding_range_still_decodes() {
        let owner = Pubkey::new_unique();
        let mkt = Pubkey::new_unique();
        // Write a non-zero byte at offset 138 (the first byte of
        // `borrowed_value_upper_bound`). Previously: padding violation.
        // Now: legitimate extension-field data.
        let mut bytes = synth_obligation(owner, mkt, 100, false, &[], &[]);
        bytes[OBL_BORROWED_VALUE_UPPER_BOUND_OFF] = 0xAB;
        let out = decode_obligation(&bytes)
            .expect("non-zero at old-padding-range start must decode (not padding any more)");
        // The low byte of `borrowed_value_upper_bound_wads` should now be 0xAB.
        assert_eq!(
            out.borrowed_value_upper_bound_wads & 0xFF,
            0xAB,
            "low byte should reflect the non-zero byte we wrote"
        );
        // And a non-zero byte somewhere inside the former-padding range
        // but BEFORE 188 must also decode without padding violation.
        let mut bytes2 = synth_obligation(owner, mkt, 100, false, &[], &[]);
        bytes2[OBL_UNWEIGHTED_BORROWED_VALUE_OFF + 5] = 0xCD;
        let out2 = decode_obligation(&bytes2)
            .expect("non-zero inside ex-padding interior must still decode");
        assert!(
            out2.unweighted_borrowed_value_wads != 0,
            "interior byte should flow into the field"
        );
    }

    /// (E) Account length remains strict (redundant with existing test,
    /// included explicitly per Slice 3H spec).
    #[test]
    fn obligation_wrong_length_still_rejected_slice3h() {
        let too_long = vec![0u8; OBLIGATION_LEN + 1];
        assert_eq!(
            decode_obligation(&too_long),
            Err(DecodeError::ObligationWrongSize(OBLIGATION_LEN + 1))
        );
        let too_short = vec![0u8; OBLIGATION_LEN - 2];
        assert_eq!(
            decode_obligation(&too_short),
            Err(DecodeError::ObligationWrongSize(OBLIGATION_LEN - 2))
        );
    }

    /// (F) Invalid boolean bytes for the extension bools fail closed,
    /// mirroring Solend's `unpack_bool` strictness on the deployed
    /// program.
    #[test]
    fn obligation_invalid_extension_bool_bytes_fail_closed() {
        let owner = Pubkey::new_unique();
        let mkt = Pubkey::new_unique();

        // borrowing_isolated_asset = 2 (invalid).
        let mut bytes = synth_obligation(owner, mkt, 100, false, &[], &[]);
        bytes[OBL_BORROWING_ISOLATED_ASSET_OFF] = 2;
        assert_eq!(
            decode_obligation(&bytes),
            Err(DecodeError::ObligationBoolInvalid {
                offset: OBL_BORROWING_ISOLATED_ASSET_OFF,
                value: 2,
                field: "borrowing_isolated_asset",
            })
        );

        // closeable = 0xFF (invalid).
        let mut bytes2 = synth_obligation(owner, mkt, 100, false, &[], &[]);
        bytes2[OBL_CLOSEABLE_OFF] = 0xFF;
        assert_eq!(
            decode_obligation(&bytes2),
            Err(DecodeError::ObligationBoolInvalid {
                offset: OBL_CLOSEABLE_OFF,
                value: 0xFF,
                field: "closeable",
            })
        );
    }

    /// Slice 3G-shape sanity: the live deposit obligation (1 deposit, 0
    /// borrows, all extension fields zero) must still decode. This
    /// duplicates part of (A) with the exact live shape.
    #[test]
    fn obligation_slice3g_live_shape_still_decodes() {
        let owner = Pubkey::new_unique();
        let mkt = Pubkey::new_unique();
        let reserve = Pubkey::new_unique();
        let bytes = synth_obligation(
            owner,
            mkt,
            415_083_795,
            true,
            &[SolendObligationCollateralRaw {
                deposit_reserve: reserve,
                deposited_amount: 772,
            }],
            &[],
        );
        let out = decode_obligation(&bytes).expect("Slice 3G live shape must decode");
        assert_eq!(out.deposits[0].deposit_reserve, reserve);
        assert_eq!(out.deposits[0].deposited_amount, 772);
        assert_eq!(out.borrows.len(), 0);
        assert!(out.last_update_stale);
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
        assert_eq!(out.liquidity_borrowed_amount_wads, 0);
        assert_eq!(out.liquidity_accumulated_protocol_fees_wads, 0);
        assert_eq!(out.config_deposit_limit, 0);
    }

    /// Slice 3D(iii): confirm `borrowed_amount_wads`, `accumulated_fees_wads`,
    /// and `deposit_limit` round-trip through the Pack-layout offsets we
    /// claim. Uses a non-zero, non-trivial value for each so a silently
    /// swapped offset would break the assertion.
    #[test]
    fn reserve_deposit_limit_fields_roundtrip() {
        let pk = Pubkey::new_unique();
        let borrowed_wads: u128 = 3_141_592_653_589_793_238u128; // ≈ 3.14 units @ wad
        let acc_fees_wads: u128 = 271_828_182_845_904_523u128;   // ≈ 0.27 units @ wad
        let deposit_limit: u64 = 123_456_789_000;
        let bytes = synth_reserve_with_deposit_limit_fields(
            pk, pk, 6, pk, pk, pk, 999_888_777, pk, pk, 100, false,
            borrowed_wads, acc_fees_wads, deposit_limit,
        );
        let out = decode_reserve(&bytes).expect("decode");
        assert_eq!(out.liquidity_borrowed_amount_wads, borrowed_wads);
        assert_eq!(out.liquidity_accumulated_protocol_fees_wads, acc_fees_wads);
        assert_eq!(out.config_deposit_limit, deposit_limit);
    }

    /// Slice 3D(iii): headroom helper matches the deployed
    /// `_deposit_reserve_liquidity` formula. Scenarios cover:
    /// * plenty of headroom (simple case),
    /// * exact cap (headroom == 0),
    /// * saturated past cap (saturating_sub clamps to 0),
    /// * fees-subtract effect reducing total_supply below raw sum.
    #[test]
    fn liquidity_total_supply_and_headroom_math() {
        let pk = Pubkey::new_unique();
        // Plenty of headroom: available=100, no borrow, no fees, limit=1000
        let bytes = synth_reserve_with_deposit_limit_fields(
            pk, pk, 6, pk, pk, pk, 100, pk, pk, 0, false, 0, 0, 1_000,
        );
        let r = decode_reserve(&bytes).unwrap();
        assert_eq!(r.liquidity_total_supply_raw(), Some(100));
        assert_eq!(r.deposit_headroom_raw(), 900);

        // Exact cap: total_supply == deposit_limit
        let bytes = synth_reserve_with_deposit_limit_fields(
            pk, pk, 6, pk, pk, pk, 500, pk, pk, 0, false, 0, 0, 500,
        );
        let r = decode_reserve(&bytes).unwrap();
        assert_eq!(r.liquidity_total_supply_raw(), Some(500));
        assert_eq!(r.deposit_headroom_raw(), 0);

        // Saturated past cap: available=1000, limit=500
        let bytes = synth_reserve_with_deposit_limit_fields(
            pk, pk, 6, pk, pk, pk, 1_000, pk, pk, 0, false, 0, 0, 500,
        );
        let r = decode_reserve(&bytes).unwrap();
        assert_eq!(r.liquidity_total_supply_raw(), Some(1_000));
        assert_eq!(r.deposit_headroom_raw(), 0);

        // Fees subtraction: available=10, borrowed=5 wads (tokens),
        // accumulated_fees=3 wads (tokens). total_supply = floor(10+5-3)=12.
        let bytes = synth_reserve_with_deposit_limit_fields(
            pk, pk, 6, pk, pk, pk, 10, pk, pk, 0, false,
            5 * SOLEND_WAD,
            3 * SOLEND_WAD,
            100,
        );
        let r = decode_reserve(&bytes).unwrap();
        assert_eq!(r.liquidity_total_supply_raw(), Some(12));
        assert_eq!(r.deposit_headroom_raw(), 88);

        // Sub-WAD wad fractions still floor correctly: borrowed = 0.5
        // tokens, fees = 0.3 tokens; net add = 0.2 (< 1) floors out.
        let bytes = synth_reserve_with_deposit_limit_fields(
            pk, pk, 6, pk, pk, pk, 0, pk, pk, 0, false,
            SOLEND_WAD / 2,
            (SOLEND_WAD * 3) / 10,
            100,
        );
        let r = decode_reserve(&bytes).unwrap();
        assert_eq!(r.liquidity_total_supply_raw(), Some(0));
        assert_eq!(r.deposit_headroom_raw(), 100);
    }

    #[test]
    fn sentinel_recognition() {
        let sentinel: Pubkey = SOLEND_NULL_ORACLE_SENTINEL_BS58.parse().expect("parse");
        assert!(is_null_oracle_sentinel(&sentinel));
        assert!(!is_null_oracle_sentinel(&Pubkey::new_unique()));
    }

    /// Stage 2 B-O1 — round-trip the 7 rate-config fields through the
    /// pinned offsets. Each field is given a distinct, recognizable
    /// non-zero value so a silently swapped offset would break the
    /// per-field assertion. The values are chosen to satisfy the
    /// evaluator's `is_reserve_config_valid` ordering check
    /// (`min ≤ optimal ≤ max ≤ super_max`,
    ///  `optimal_util ≤ max_util ≤ 100`,
    ///  `protocol_take ≤ 100`).
    #[test]
    fn reserve_rate_config_fields_roundtrip() {
        let pk = Pubkey::new_unique();
        let mut bytes = synth_reserve_with_deposit_limit_fields(
            pk, pk, 6, pk, pk, pk, 0, pk, pk, 0, false, 0, 0, 0,
        );
        // Distinct values per field, all under 100 except super_max.
        const OPTIMAL_UTIL: u8 = 80;
        const MIN_BORROW: u8 = 1;
        const OPTIMAL_BORROW: u8 = 4;
        const MAX_BORROW: u8 = 50;
        const PROTOCOL_TAKE: u8 = 20;
        const MAX_UTIL: u8 = 95;
        const SUPER_MAX_BORROW: u64 = 300; // 300% — must fit u64, not u8.
        overlay_reserve_rate_config_fields(
            &mut bytes,
            OPTIMAL_UTIL,
            MIN_BORROW,
            OPTIMAL_BORROW,
            MAX_BORROW,
            PROTOCOL_TAKE,
            MAX_UTIL,
            SUPER_MAX_BORROW,
        );

        let out = decode_reserve(&bytes).expect("decode");
        assert_eq!(out.config_optimal_utilization_rate_pct, OPTIMAL_UTIL);
        assert_eq!(out.config_min_borrow_rate_pct, MIN_BORROW);
        assert_eq!(out.config_optimal_borrow_rate_pct, OPTIMAL_BORROW);
        assert_eq!(out.config_max_borrow_rate_pct, MAX_BORROW);
        assert_eq!(out.config_protocol_take_rate_pct, PROTOCOL_TAKE);
        assert_eq!(out.config_max_utilization_rate_pct, MAX_UTIL);
        assert_eq!(out.config_super_max_borrow_rate_pct, SUPER_MAX_BORROW);

        // Defence in depth: total bytes is still RESERVE_LEN — the new
        // offsets did NOT push the total past 619.
        assert_eq!(bytes.len(), RESERVE_LEN);
    }

    /// Stage 2 B-O1 — explicit byte-cursor sanity: the cumulative widths
    /// of every field documented in the layout comment (whether READ or
    /// SKIPPED) must equal `RESERVE_LEN`. Catches any future field-list
    /// drift against the upstream Pack layout.
    #[test]
    fn reserve_layout_width_sum_equals_reserve_len() {
        let widths: &[usize] = &[
            1, 8, 1, 32, 32, 1, 32, 32, 32, 8, 16, 16, 16, 32, 8, 32, 1, 1, 1,
            1, 1, 1, 1, 8, 8, 1, 8, 8, 32, 1, 1, 16, 56, // rate_limiter
            8, 16, 1, 1, 8, 1, 1, 8, 32, 1, 16, 16, 8, 8, 49,
        ];
        let total: usize = widths.iter().sum();
        assert_eq!(total, RESERVE_LEN, "field-width sum must equal RESERVE_LEN");
        // Spot-checks: the two B-O1 extension offsets fall in the
        // post-rate-limiter window the layout comment claims.
        assert_eq!(RES_CONFIG_MAX_UTILIZATION_RATE_OFF, 470);
        assert_eq!(RES_CONFIG_SUPER_MAX_BORROW_RATE_OFF, 471);
        // And the pre-rate-limiter offsets are unchanged.
        assert_eq!(RES_CONFIG_OPTIMAL_UTILIZATION_RATE_OFF, 299);
        assert_eq!(RES_CONFIG_PROTOCOL_TAKE_RATE_OFF, 372);
    }

    /// Stage 2 B-O1 — defence in depth: the 7 new fields fall in
    /// disjoint, non-overlapping byte ranges. Catches a swapped offset
    /// constant that happens to round-trip a single test value by luck.
    #[test]
    fn reserve_rate_config_offsets_are_disjoint() {
        let ranges: &[(usize, usize, &'static str)] = &[
            (RES_CONFIG_OPTIMAL_UTILIZATION_RATE_OFF, 1, "optimal_utilization"),
            (RES_CONFIG_MIN_BORROW_RATE_OFF, 1, "min_borrow"),
            (RES_CONFIG_OPTIMAL_BORROW_RATE_OFF, 1, "optimal_borrow"),
            (RES_CONFIG_MAX_BORROW_RATE_OFF, 1, "max_borrow"),
            (RES_CONFIG_PROTOCOL_TAKE_RATE_OFF, 1, "protocol_take"),
            (RES_CONFIG_MAX_UTILIZATION_RATE_OFF, 1, "max_utilization"),
            (RES_CONFIG_SUPER_MAX_BORROW_RATE_OFF, 8, "super_max_borrow"),
        ];
        for (i, (o1, l1, n1)) in ranges.iter().enumerate() {
            let end1 = o1 + l1;
            assert!(end1 <= RESERVE_LEN, "{n1} runs past RESERVE_LEN");
            for (o2, l2, n2) in &ranges[i + 1..] {
                let end2 = o2 + l2;
                // disjoint: [o1, end1) ∩ [o2, end2) == ∅
                let disjoint = end1 <= *o2 || end2 <= *o1;
                assert!(disjoint, "{n1}@{o1}..{end1} overlaps {n2}@{o2}..{end2}");
            }
        }
    }
}

// Re-export synth helpers for the `mapping` tests.
#[cfg(test)]
pub(crate) use tests::{synth_obligation, synth_reserve};
