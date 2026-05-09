# Stage 2 — Solend Supply APR/APY Calculator Research & Spec Substrate

**Status:** Research / spec substrate only. **No product code changes. No live RPC. No deploy. No push.**
**Date:** 2026-05-09.
**Audience:** Stage 2 implementation agents (B1 WatchRule, daemon condition evaluator, on-chain re-verifier).
**Scope:** Solend USDC supply-side rate calculator for the Stage 2 condition

> *"If Solend USDC supply APY < 10%, withdraw the delegated Solend position back to the user's main wallet."*

**Anchor docs (this file builds on, does not replace):**
- [`STAGE2_DELEGATED_CONDITIONAL_EXECUTION_SPEC.md`](STAGE2_DELEGATED_CONDITIONAL_EXECUTION_SPEC.md) — § 5, § 7.2, § 10, § 11, § 14
- [`STAGE2_OFFICIAL_DOCS_AUDIT.md`](STAGE2_OFFICIAL_DOCS_AUDIT.md) — C-2, C-3, R-2, U-1
- Repo decoder: [`crates/gateway/src/integrations/solend/raw.rs`](../crates/gateway/src/integrations/solend/raw.rs)

---

## 0. TL;DR

1. **Solend exposes no on-chain `supply_apr` / `supply_apy` helper.** Both daemon and on-chain re-verifier MUST derive supply rate from primitives. (Confirms audit C-2.)
2. **Recommended primary metric: supply APR (not APY).** APR is one fixed-point multiply away from `current_borrow_rate`; APY needs a per-slot `(1 + r/SLOTS_PER_YEAR)^SLOTS_PER_YEAR` power that costs compute units, risks parity drift, and adds nothing the user can act on at a 10% threshold. The B1 WatchRule MUST allow `rate_kind = Apr`; APY is allowed only as an optional extension with explicit `rate_kind = Apy` and a higher compute budget.
3. **Same-tx is non-negotiable for the withdraw-side path** (`STALE_AFTER_SLOTS_ELAPSED = 1`). The condition-evaluation step (§ 5.2 step 5) and the `RefreshReserve + Withdraw` step (§ 5.2 step 6) MUST be in the same transaction the program sees.
4. **Three-region kinked model with `super_max_borrow_rate: u64`** (audit C-3). Naive `as u8` cast on the third region truncates and silently breaks the comparator above ~max-utilisation. Parity fixtures MUST cover the third region.
5. **Decimal/Rate fixed-point at WAD = 10¹⁸ in u128.** No floats. The on-chain computation reuses Solend's `Decimal`/`Rate` types via `solend-program` SDK; the daemon-side comparator MUST use a bit-equal pure-Rust `u128` reimplementation (not `f64`, not `rug`, not arbitrary precision).
6. **B1 WatchRule schema includes `formula_version`**, so a future rate-model change at Solend bumps `schema_version` in the canonical-intent hash and old authorisations stop being executable rather than silently misfiring.

---

## 1. Official Sources Consulted

All sources are mainnet-branch primary sources. No secondary / community write-ups.

| Source | Purpose | Key extracts |
|---|---|---|
| [`token-lending/sdk/src/state/reserve.rs`](https://raw.githubusercontent.com/solendprotocol/solana-program-library/mainnet/token-lending/sdk/src/state/reserve.rs) | Reserve / ReserveConfig / ReserveLiquidity layouts; `current_borrow_rate`; `utilization_rate`; `compound_interest`; `accrue_interest`; collateral↔liquidity rounding | Three-region rate model verbatim; `protocol_take_rate: u8`; `super_max_borrow_rate: u64`; `try_floor_u64` both directions |
| [`token-lending/sdk/src/state/last_update.rs`](https://raw.githubusercontent.com/solendprotocol/solana-program-library/mainnet/token-lending/sdk/src/state/last_update.rs) | Staleness window | `STALE_AFTER_SLOTS_ELAPSED: u64 = 1` |
| [`token-lending/sdk/src/state/mod.rs`](https://raw.githubusercontent.com/solendprotocol/solana-program-library/mainnet/token-lending/sdk/src/state/mod.rs) | Time base | `SLOTS_PER_YEAR: u64 = 63_072_000` (2 slots/sec × 60 × 60 × 24 × 365); `INITIAL_COLLATERAL_RATIO: u64 = 1`; `PROGRAM_VERSION: u8 = 1` |
| [`token-lending/sdk/src/instruction.rs`](https://raw.githubusercontent.com/solendprotocol/solana-program-library/mainnet/token-lending/sdk/src/instruction.rs) | Account orderings for `RefreshReserve` (variant 3), `RefreshObligation` (variant 7), `WithdrawObligationCollateralAndRedeemReserveCollateral` (variant 15) | Variant 15: 13 accounts, two signer slots at indices 9 (`obligation_owner`) and 10 (`user_transfer_authority`) |
| [`token-lending/sdk/src/math/common.rs`](https://raw.githubusercontent.com/solendprotocol/solana-program-library/mainnet/token-lending/sdk/src/math/common.rs) | Fixed-point scale | `SCALE: usize = 18`; `WAD: u64 = 1_000_000_000_000_000_000`; `HALF_WAD`; `PERCENT_SCALER`; `BPS_SCALER` |
| [`crates/gateway/src/integrations/solend/raw.rs`](../crates/gateway/src/integrations/solend/raw.rs) | Existing repo decoder (offsets, `SOLEND_WAD`) | Reserve `protocol_take_rate` at byte offset **372** (currently SKIPPED in the read-model); `RESERVE_LEN = 619`; SOLEND_WAD = 10¹⁸ |
| [`docs/proofs/SOLEND_USDC_DEPOSIT_PHASE4C_E2E.md`](proofs/SOLEND_USDC_DEPOSIT_PHASE4C_E2E.md) | Live mainnet reserve pubkey for fixture capture | USDC reserve `BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw`, lending market `Main Pool`, slot 415475589, mint `EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v` |

---

## 2. Reserve Account Layout (Stage 2 read-model superset)

Stage 2's APR derivation needs **strictly more** reserve fields than the existing repo decoder reads. The existing decoder ([`raw.rs`](../crates/gateway/src/integrations/solend/raw.rs)) targets the deposit-only read-model and explicitly SKIPs all rate-config fields. Below is the **superset Stage 2 must additionally read**, with byte offsets cited from the existing decoder where available, and flagged as **TO RE-VERIFY** where they fall in the SKIPPED region.

### 2.1 Fields already decoded by `raw.rs` (re-used as-is)

| Field | Offset | Type | Source line in `raw.rs` |
|---|---|---|---|
| `version` | 0 | u8 | `RES_VERSION_OFF` |
| `last_update.slot` | 1 | u64 LE | `RES_LAST_UPDATE_SLOT_OFF` |
| `last_update.stale` | 9 | bool (0/1) | `RES_LAST_UPDATE_STALE_OFF` |
| `lending_market` | 10 | Pubkey | `RES_LENDING_MARKET_OFF` |
| `liquidity.mint_pubkey` | 42 | Pubkey | `RES_LIQ_MINT_OFF` |
| `liquidity.mint_decimals` | 74 | u8 | `RES_LIQ_MINT_DECIMALS_OFF` |
| `liquidity.available_amount` | 171 | u64 LE | `RES_LIQ_AVAILABLE_AMOUNT_OFF` |
| `liquidity.borrowed_amount_wads` | 179 | u128 LE (Decimal scaled-val) | `RES_LIQ_BORROWED_AMOUNT_WADS_OFF` |
| `liquidity.accumulated_protocol_fees_wads` | 373 | u128 LE | `RES_LIQ_ACCUMULATED_PROTOCOL_FEES_WADS_OFF` |

### 2.2 Fields the APR calculator needs in addition (currently SKIPPED — TO RE-VERIFY before implementation)

The existing decoder's offset comments (`raw.rs` lines 141..159) name these fields and the SKIPPED status, but the offset list is incomplete versus the mainnet ReserveConfig (specifically: `max_utilization_rate`, `max_liquidation_bonus`, `max_liquidation_threshold` and `super_max_borrow_rate` are present in the live struct but not in the offset table). **Stage 2 implementation MUST re-walk the `Pack::pack_into_slice` macro in the mainnet branch source to nail down each offset before adding them to the decoder.**

| Field | Type | Approximate offset (from existing comments) | Status |
|---|---|---|---|
| `config.optimal_utilization_rate` | u8 (percent) | ~299 | SKIPPED — re-verify |
| `config.max_utilization_rate` | u8 (percent) | (after `optimal_utilization_rate`) | **NOT IN raw.rs OFFSET TABLE** — re-verify against mainnet `Pack` |
| `config.min_borrow_rate` | u8 (percent) | ~303 | SKIPPED — re-verify |
| `config.optimal_borrow_rate` | u8 (percent) | ~304 | SKIPPED — re-verify |
| `config.max_borrow_rate` | u8 (percent) | ~305 | SKIPPED — re-verify |
| `config.super_max_borrow_rate` | **u64 LE** (percent stored in u64) | (after `max_borrow_rate`) | **NOT IN raw.rs OFFSET TABLE** — re-verify; **width matters** (audit C-3) |
| `config.protocol_take_rate` | u8 (percent) | 372 | SKIPPED but offset known — re-verify |

**Open question O-1.** The `raw.rs` offset table predates the mainnet extension fields (`max_utilization_rate`, `max_liquidation_bonus`, `max_liquidation_threshold`, `super_max_borrow_rate`). Stage 2 implementation MUST re-verify these byte offsets by walking `pack_into_slice` in `token-lending/sdk/src/state/reserve.rs` end-to-end, exactly the way `raw.rs` documents its existing walk. A live-fixture round-trip test (§ 6.6 below) catches any drift.

### 2.3 ReserveConfig declaration order (verbatim, per § 1 source fetch)

```rust
pub struct ReserveConfig {
    pub optimal_utilization_rate: u8,
    pub max_utilization_rate: u8,
    pub loan_to_value_ratio: u8,
    pub liquidation_bonus: u8,
    pub max_liquidation_bonus: u8,
    pub liquidation_threshold: u8,
    pub max_liquidation_threshold: u8,
    pub min_borrow_rate: u8,
    pub optimal_borrow_rate: u8,
    pub max_borrow_rate: u8,
    pub super_max_borrow_rate: u64,        // ← wider type
    pub fees: ReserveFees,                  // borrow_fee_wad u64 + flash_loan_fee_wad u64 + host_fee_percentage u8
    pub deposit_limit: u64,
    pub borrow_limit: u64,
    pub fee_receiver: Pubkey,
    pub protocol_liquidation_fee: u8,
    pub protocol_take_rate: u8,
    pub added_borrow_weight_bps: u64,
    pub reserve_type: ReserveType,
    pub scaled_price_offset_bps: i64,
    pub extra_oracle_pubkey: Option<Pubkey>,
    pub attributed_borrow_limit_open: u64,
    pub attributed_borrow_limit_close: u64,
}
```

Declaration order is **not** the same as `Pack` byte order in general — see O-1.

---

## 3. Rate Model — Source Verbatim

### 3.1 `current_borrow_rate` (three regions, two kinks)

Verbatim from `token-lending/sdk/src/state/reserve.rs` (mainnet branch):

```rust
pub fn current_borrow_rate(&self) -> Result<Rate, ProgramError> {
    let utilization_rate = self.liquidity.utilization_rate()?;
    let optimal_utilization_rate = Rate::from_percent(self.config.optimal_utilization_rate);
    let max_utilization_rate = Rate::from_percent(self.config.max_utilization_rate);

    if utilization_rate <= optimal_utilization_rate {
        // Region 1: u in [0, u_opt] — linear from min to optimal
        let min_rate = Rate::from_percent(self.config.min_borrow_rate);
        if optimal_utilization_rate == Rate::zero() { return Ok(min_rate); }
        let normalized_rate = utilization_rate.try_div(optimal_utilization_rate)?;
        let rate_range = Rate::from_percent(
            self.config.optimal_borrow_rate.checked_sub(self.config.min_borrow_rate)?
        );
        Ok(normalized_rate.try_mul(rate_range)?.try_add(min_rate)?)

    } else if utilization_rate <= max_utilization_rate {
        // Region 2: u in (u_opt, u_max] — linear from optimal to max
        let weight = utilization_rate.try_sub(optimal_utilization_rate)?
            .try_div(max_utilization_rate.try_sub(optimal_utilization_rate)?)?;
        let optimal_borrow_rate = Rate::from_percent(self.config.optimal_borrow_rate);
        let max_borrow_rate = Rate::from_percent(self.config.max_borrow_rate);
        let rate_range = max_borrow_rate.try_sub(optimal_borrow_rate)?;
        weight.try_mul(rate_range)?.try_add(optimal_borrow_rate)

    } else {
        // Region 3: u in (u_max, 100%] — linear from max to super_max
        let weight: Decimal = utilization_rate.try_sub(max_utilization_rate)?
            .try_div(Rate::from_percent(100u8.checked_sub(self.config.max_utilization_rate)?))?
            .into();
        let max_borrow_rate = Rate::from_percent(self.config.max_borrow_rate);
        let super_max_borrow_rate = Rate::from_percent_u64(self.config.super_max_borrow_rate);
        let rate_range: Decimal = super_max_borrow_rate.try_sub(max_borrow_rate)?.into();
        weight.try_mul(rate_range)?.try_add(max_borrow_rate.into())?.try_into()
    }
}
```

**Key observations:**
- Region 3 uses `Rate::from_percent_u64(super_max_borrow_rate)` — **note the `_u64` suffix.** A naive port to a hand-rolled comparator that does `Rate::from_percent(u8)` here truncates `super_max_borrow_rate` to u8 and silently caps at 255%. This is the audit-C-3 footgun.
- All three regions are **piecewise linear in utilisation** — there is no exponentiation in `current_borrow_rate`. APR is therefore cheap; the cost lives in APY (§ 4.4).

### 3.2 `utilization_rate`

```rust
pub fn utilization_rate(&self) -> Result<Rate, ProgramError> {
    let total_supply = self.total_supply()?;
    if total_supply == Decimal::zero() || self.borrowed_amount_wads == Decimal::zero() {
        return Ok(Rate::zero());
    }
    let denominator = self.borrowed_amount_wads.try_add(Decimal::from(self.available_amount))?;
    self.borrowed_amount_wads.try_div(denominator)?.try_into()
}
```

`total_supply` (per existing repo decoder citation, `raw.rs::liquidity_total_supply_raw`):
`floor( available_amount * WAD + borrowed_amount_wads − accumulated_protocol_fees_wads ) / WAD`

For the **utilisation denominator** Solend does NOT subtract `accumulated_protocol_fees_wads`. The denominator is `available_amount + borrowed_amount_wads`. The fees subtraction lives only in the `total_supply` early-zero check.

### 3.3 `accrue_interest` and `compound_interest`

```rust
pub fn accrue_interest(&mut self, current_slot: Slot) -> ProgramResult {
    let slots_elapsed = self.last_update.slots_elapsed(current_slot)?;
    if slots_elapsed > 0 {
        let current_borrow_rate = self.current_borrow_rate()?;
        let take_rate = Rate::from_percent(self.config.protocol_take_rate);
        self.liquidity.compound_interest(current_borrow_rate, slots_elapsed, take_rate)?;
    }
    Ok(())
}

fn compound_interest(&mut self, current_borrow_rate: Rate, slots_elapsed: u64, take_rate: Rate) -> ProgramResult {
    let slot_interest_rate = current_borrow_rate.try_div(SLOTS_PER_YEAR)?;
    let compounded_interest_rate = Rate::one()
        .try_add(slot_interest_rate)?
        .try_pow(slots_elapsed)?;
    self.cumulative_borrow_rate_wads = self.cumulative_borrow_rate_wads.try_mul(compounded_interest_rate)?;
    let net_new_debt = self.borrowed_amount_wads
        .try_mul(compounded_interest_rate)?
        .try_sub(self.borrowed_amount_wads)?;
    self.accumulated_protocol_fees_wads = net_new_debt
        .try_mul(take_rate)?
        .try_add(self.accumulated_protocol_fees_wads)?;
    self.borrowed_amount_wads = self.borrowed_amount_wads.try_add(net_new_debt)?;
    Ok(())
}
```

The protocol take rate appears **only** as `net_new_debt × take_rate → accumulated_protocol_fees_wads`. There is no separate "supply rate" in Solend's accounting — the supply side accrues passively as `borrowed_amount_wads` grows minus the fee skim that goes to the protocol.

### 3.4 `STALE_AFTER_SLOTS_ELAPSED`

```rust
pub const STALE_AFTER_SLOTS_ELAPSED: u64 = 1;

pub fn is_stale(&self, slot: Slot) -> Result<bool, ProgramError> {
    Ok(self.stale || self.slots_elapsed(slot)? >= STALE_AFTER_SLOTS_ELAPSED)
}
```

**Implication for Stage 2.** `RefreshReserve` and `WithdrawObligationCollateralAndRedeemReserveCollateral` MUST be in the same transaction; even one slot of separation flips `is_stale → true` and `_withdraw_obligation_collateral` rejects with `ReserveStale` for any obligation with non-empty borrows. (Stage 2's deposit-only obligations skip this check on the borrow-empty path — but parity with the borrow-eligible path and accurate cToken→liquidity conversion both require same-tx refresh anyway, so the daemon MUST package them together unconditionally.)

---

## 4. Calculator Design

### 4.1 Why APR (not APY) is the recommended primary metric

| Property | APR (per-year, no compounding) | APY (per-year, compounded slot-wise) |
|---|---|---|
| Computation | One fixed-point multiply: `borrow_rate × utilisation × (1 − take_rate)` | `((1 + r/SLOTS_PER_YEAR)^SLOTS_PER_YEAR) − 1` — `try_pow(63_072_000)` is **not** what `try_pow` does in Solend's `Rate` (it's slot-scoped); a yearly compound would need a dedicated implementation |
| Compute units (on-chain) | ~10 multiplies / divs at WAD; cheap | Power-of-large-exponent — measurably more expensive; pessimistically may need to be split or pre-computed |
| Parity surface | Few operations; bit-equal between daemon and program is straightforward | Each multiply/round in the power loop is a chance for parity drift (different intermediate rounding between off-chain and on-chain) |
| User actionability at the demo threshold | `< 10% APR` and `< 10% APY` differ by a fraction of a percent at typical Solend USDC rates; the user-visible decision (withdraw vs not) is unchanged | Same — APY framing does not give the user better information at this threshold |
| Audit C-2 framing risk | "We compute supply APR ourselves; here is the formula" is honest | "We compute supply APY" risks being read as "we use Solend's APY function" — which doesn't exist |

**Recommendation.** B1 WatchRule MUST allow `rate_kind = Apr` and SHOULD default to it for the v1 demo. `rate_kind = Apy` MAY be added in a follow-up phase with a separate compute-budget allocation and a separate parity fixture set; the demo is **not** gated on APY support.

**Naming hygiene.** The demo script and UI MUST say *"supply APR < 10%"*, not *"supply APY < 10%"*, when the actual computation is APR. Any conflation in the user-facing copy is a no-overclaim violation. (See [`feedback_no_overclaim.md`](../.claude/projects/c--Users-jeffr-SolFrontier/memory/feedback_no_overclaim.md) for the policy this honours.)

### 4.2 Supply APR formula

```text
borrow_rate_wad   = current_borrow_rate(reserve)        # WAD-scaled (10^18)
utilisation_wad   = utilization_rate(reserve)           # WAD-scaled
take_rate_wad     = protocol_take_rate (u8 percent) → WAD = pct * 10^16
supply_apr_wad    = mul_wad( borrow_rate_wad,
                       mul_wad( utilisation_wad,
                                sub_wad( WAD, take_rate_wad ) ) )
```

Where `mul_wad(a, b) = (a as u256 * b as u256) / WAD as u256`, floored. (`u256` here is conceptual — the implementation widens to `u128 → u256` via Solend's `Decimal` type, which already handles overflow check.)

**Equivalent algebra:** `supply_apr = borrow_apr × utilisation × (1 − protocol_take_rate)`. This is the standard kinked-IRM supply-side identity; Solend's documentation expresses it the same way. The daemon MUST compute it via the same WAD-fixed-point ops the program uses (no `f64` shortcut).

### 4.3 Threshold comparison

The B1 rule's threshold is also stored as WAD-fixed-point (or as basis points, see § 5). Comparison is a single `<` between `u128` scaled values — no float, no rounding mode ambiguity. Direction (`LessThan`) maps to `supply_apr_wad < threshold_wad`.

### 4.4 If APY ever ships (deferred): cost accounting

The annual supply APY is `(1 + supply_apr/SLOTS_PER_YEAR)^SLOTS_PER_YEAR − 1`. Solend's `Rate::try_pow` is exponentiation by squaring in u128/WAD; with exponent ≈ 63 million, that's ~26 squarings and ~13 multiplies. Each squaring is one `u128 → u256 → u128` mul-then-div-by-WAD with overflow check. **Off-chain this is microseconds; on-chain it's measurable compute units that must be budgeted in the program's CU envelope.** APY support, if added, MUST publish a CU measurement in the parity-fixture report.

### 4.5 Off-chain daemon calculator API (sketch)

```rust
// crates/clawsol-solend-rate/src/lib.rs   (new crate, or sub-module)
//
// Pure-Rust, no_std-capable, no_float, no Solana deps. Daemon and program
// both depend on this crate to guarantee bit-equal results.

pub const SOLEND_WAD: u128 = 1_000_000_000_000_000_000;
pub const SOLEND_SLOTS_PER_YEAR: u64 = 63_072_000;        // Pinned: see § 1; CI parity test

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReserveRateInputs {
    pub available_amount:       u64,    // base units of underlying
    pub borrowed_amount_wads:   u128,   // Decimal scaled-val
    // protocol_take_rate is a percent in [0,100]; widen and store as u8 for type safety
    pub protocol_take_rate_pct: u8,
    pub min_borrow_rate_pct:    u8,
    pub optimal_borrow_rate_pct:u8,
    pub max_borrow_rate_pct:    u8,
    pub super_max_borrow_rate_pct: u64, // u64 width preserved (audit C-3)
    pub optimal_utilization_rate_pct: u8,
    pub max_utilization_rate_pct:     u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateError {
    MathOverflow,
    InvalidConfig,         // e.g. min > optimal > max ordering violated
    SuperMaxBelowMax,      // super_max_borrow_rate < max_borrow_rate
}

pub type Wad = u128;       // 18-decimal fixed-point

pub fn utilization_wad(r: &ReserveRateInputs) -> Result<Wad, RateError> { ... }
pub fn current_borrow_rate_wad(r: &ReserveRateInputs) -> Result<Wad, RateError> { ... }
pub fn supply_apr_wad(r: &ReserveRateInputs) -> Result<Wad, RateError> { ... }

// Optional, deferred:
// pub fn supply_apy_wad(r: &ReserveRateInputs) -> Result<Wad, RateError> { ... }

pub const FORMULA_VERSION: u8 = 1;  // bump if Solend rate model changes
```

The on-chain re-verifier depends on this crate via `solend_rate::supply_apr_wad(decoded_inputs)` and asserts the result is `<` the threshold from the canonical-intent-bound PDA.

### 4.6 On-chain program calculator API (sketch)

The program may either:
- **(a)** depend on `solend-program::state::Reserve` directly, call its real `current_borrow_rate()`, and reproduce only the `× utilisation × (1 − take_rate)` step on top — minimum surface, but couples ClawSol to Solend's SDK version; **or**
- **(b)** depend on the new `clawsol-solend-rate` crate (§ 4.5) that ships a verbatim port of Solend's three-region formula in pure-Rust u128 fixed-point — fully decoupled from Solend SDK version; parity asserted by fixtures.

**Recommendation: (b).** Coupling ClawSol's program build to the Solend SDK version drags `solend-program` into the on-chain crate's dependency tree, inflates compiled size, and makes the audit harder. A self-contained 200-line APR module with verbatim formulas and fixture parity tests is cleaner. (The fixtures themselves are the authoritative parity proof against the live program; we don't need the SDK in the build.)

### 4.7 No-float arithmetic strategy

- Daemon, program, and shared crate are `#![deny(clippy::float_arithmetic)]` (or equivalent) — float types compile-error.
- All inputs are integer (`u8`/`u64`/`u128`).
- All intermediate ops are WAD-fixed-point u128, with widening to u256 only inside the multiply (matches Solend's `Decimal::try_mul`).
- All comparisons are u128 `<` / `>` / `==` — no `epsilon`, no `approx_eq`.
- Tests use **bit-equal** assertions (`assert_eq!(a, b)`), never `assert!((a - b).abs() < eps)`.

---

## 5. B1 WatchRule Schema — Recommended Fields

Recommended subset for a Solend supply-rate condition. Field names are advisory; the actual schema landing in B1 may rename, but the **semantic content** below MUST be present.

```rust
// In B1 WatchRule schema (subject of a separate substrate doc):
pub struct WatchRuleSolendSupplyRate {
    /// Discriminant. Pinned u8 = N for forward compatibility with other source types.
    pub source_type: SourceType,                // = SourceType::SolendReserveSupplyRate

    /// The Solend reserve account being watched. Part of the canonical intent
    /// hash; an execute attempt against any other reserve fails on hash recompute.
    pub reserve_pubkey: Pubkey,

    /// The lending market the reserve belongs to. Recorded for sanity / decode
    /// safety; the program asserts `decoded_reserve.lending_market == lending_market`.
    pub lending_market: Pubkey,

    /// The Solend program id (mainnet `So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo`).
    /// Pinned in the rule so a future Solend program-id rotation forces re-authz.
    pub solend_program_id: Pubkey,

    /// Comparison direction.
    pub comparison: Comparison,                 // LessThan | LessOrEqual | GreaterThan | GreaterOrEqual

    /// Threshold as basis points (e.g. 1000 = 10.00%). Stored as u32; comparator
    /// converts to WAD before comparing to `supply_apr_wad`.
    pub threshold_bps: u32,

    /// Rate kind to compare against. v1 demo: Apr only. Apy is deferred (see § 4.4).
    pub rate_kind: RateKind,                    // Apr | Apy

    /// Formula version pinned in the rule. If Solend changes its rate model,
    /// `FORMULA_VERSION` in the calculator crate bumps; old rules with the old
    /// version stop being executable until the user re-authorises.
    pub formula_version: u8,                    // = solend_rate::FORMULA_VERSION at authz time

    /// Maximum reserve `last_update.slot` age, in slots, that the comparator
    /// will treat as fresh-enough for evaluation. For deposit-only obligations
    /// the on-chain staleness check is skipped, but the daemon MUST still
    /// apply this guard to avoid acting on a multi-slot-old reserve snapshot.
    /// Recommended default: 32 (≈ 16 s @ 2 slots/sec).
    pub max_reserve_staleness_slots: u32,

    /// True ⇒ require `RefreshReserve` to be in the same tx as the action ix.
    /// MUST be `true` for the Stage 2 withdraw path. Field exists to make the
    /// invariant visible in the rule, not to be flipped to `false` in v1.
    pub required_refresh_same_tx: bool,         // = true

    /// Borrow-rate-config snapshot at authz time, recorded for forensic
    /// "what config did we authorise against?" review. NOT used at evaluation
    /// time (the comparator always reads live reserve config). Optional; may
    /// be elided if the canonical intent hash size budget is tight.
    pub authz_time_config_snapshot: Option<ReserveConfigDigest>,
}

pub enum RateKind { Apr, Apy }
```

**Justification for each field:**

- `source_type` — discriminant lets the same WatchRule schema cover Pyth-price, Solend-rate, and future condition sources behind one Borsh enum.
- `reserve_pubkey` + `lending_market` + `solend_program_id` — three layers of identity. Pubkey alone is not enough: a malicious re-init could rotate ownership; the program-id pin defends against Solend program-id rotation; the lending-market pin defends against a reserve being moved to a sibling pool.
- `comparison` + `threshold_bps` — `bps` instead of percent gives 0.01% resolution at u32 (max 4.29e9 bps = 4.29e7%, plenty); WAD storage of the threshold is also acceptable but slightly larger.
- `rate_kind` — see § 4.4. Forces the demo to be honest about whether it computes APR or APY.
- `formula_version` — the **load-bearing field** for version safety. If Solend changes the rate model (utilisation regions, kink positions, max-rate parameters, take-rate semantics), our calculator crate's `FORMULA_VERSION` bumps and old WatchRules fail with `FormulaVersionMismatch` until re-authorised. Pins the on-chain canonical-intent hash to the formula version it was authorised against (per Stage 2 spec § 7.2 last bullet).
- `max_reserve_staleness_slots` — orthogonal to Solend's `STALE_AFTER_SLOTS_ELAPSED = 1` (which is about borrow-side correctness). This is a **rule-level liveness gate** — refuse to act on data that's, say, more than 16 s old at the daemon, regardless of what Solend's on-chain check says. Concretely defends against a "we read reserve at slot N, the daemon stalled, we executed at slot N+200" race.
- `required_refresh_same_tx` — see § 5.2 step 6 in the Stage 2 spec; making the constraint visible in the rule lets reviewers grep for the bool rather than re-discovering the invariant.
- `authz_time_config_snapshot` (optional) — purely forensic. Audit / post-mortem can answer "the rate model at authz time had `optimal_borrow_rate = X`; at execute it had `Y` — the user authorised against the older shape." Not used at evaluation. Drop it if the canonical-intent hash byte budget is tight.

---

## 6. Tests — Required Coverage

All tests live in `crates/clawsol-solend-rate/tests/` (or wherever the calculator crate ends up). CI green is a release blocker.

### 6.1 Reserve fixture decode

- Synthesised reserve bytes round-trip through the (extended) decoder for every field the calculator reads. Mirrors the existing pattern in [`raw.rs::reserve_roundtrip`](../crates/gateway/src/integrations/solend/raw.rs#L1003).
- Property tests: random valid configs decode without panic; invalid configs (e.g. `min > optimal`) are rejected at decode time, not silently treated as zero.

### 6.2 Utilisation regimes

| Test | `available` | `borrowed_amount_wads` | Expected `utilisation` |
|---|---|---|---|
| zero supply | 0 | 0 | 0 |
| zero borrow | 1_000_000 | 0 | 0 |
| low | 1_000_000 | 100_000 × WAD | ~9.09% |
| at-optimal | 100_000 | 200_000 × WAD (config: u_opt = 80%) | exactly at kink |
| between-kinks | 100_000 | 800_000 × WAD (u_opt=80%, u_max=90%) | ~88.89% (region 2) |
| at-max | 100_000 | 900_000 × WAD | exactly at second kink |
| super-max | 100_000 | 999_000 × WAD | ~99.9% (region 3) |
| pathological | 1 | u128::MAX | should not panic; should overflow-error or saturate cleanly |

### 6.3 `protocol_take_rate` effect

Pin a single utilisation regime (say 80%) and vary `protocol_take_rate ∈ {0, 10, 50, 100}`. Assert `supply_apr_wad` decreases monotonically and equals `borrow_apr_wad × utilisation × (WAD − take_rate_wad) / WAD` to bit precision.

### 6.4 Threshold boundary (`LessThan`)

For each (config, threshold) pair, assert:
- `supply_apr = threshold − 1` ⇒ rule fires.
- `supply_apr = threshold` ⇒ rule does **not** fire (strict inequality).
- `supply_apr = threshold + 1` ⇒ rule does not fire.

Repeats for `LessOrEqual`, `GreaterThan`, `GreaterOrEqual` to catch boundary direction bugs.

### 6.5 Daemon ↔ program parity (the load-bearing test)

- Both daemon (host build) and program (sBPF build) load the **same** fixture reserve bytes.
- Both compute `supply_apr_wad`.
- Assert **bit-equal** results (`assert_eq!`).
- Run for the full fixture matrix (low / mid / super-max / boundary).
- Failure here is a release blocker.

### 6.6 Live-reserve fixture capture

Stage 1 Tail's Solend live work captured the USDC main-pool reserve at finalised slot **415475589** (per [`SOLEND_USDC_DEPOSIT_PHASE4C_E2E.md`](proofs/SOLEND_USDC_DEPOSIT_PHASE4C_E2E.md)). For Stage 2 parity:
- **Capture step (one-shot, manual, **NOT** part of CI):** RPC `getAccountInfo` against `BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw` at a finalised slot. Save the raw 619-byte account data to `crates/clawsol-solend-rate/tests/fixtures/usdc_main_pool_<slot>.bin`. Record the slot, lending market, and capture timestamp in a sibling `.txt`.
- **CI use:** load the fixture, decode, compute APR, assert against a hand-computed expected value (also saved as a `.expected.toml` next to the bin).
- **Refresh policy:** capture a new fixture once per Stage 2 phase milestone; old fixtures are kept (renamed with date) so regressions can be reproduced against historical reserve states.
- **No live RPC in CI.** The fixture file is the authoritative input; CI never calls mainnet.

### 6.7 Stale-reserve rejection

- Reserve fixture with `last_update.slot = current_slot − (max_reserve_staleness_slots + 1)`.
- Daemon evaluation MUST refuse with `StaleReserve`.
- Mirror test on the program side: program receives an unrefreshed reserve and refuses with `ReserveStale` (Solend's own error path).

### 6.8 Same-tx refresh invariant

- Build a synthetic transaction that contains `RefreshReserve` and the withdraw ix in the same tx. Assert it passes the static layout check.
- Build a synthetic transaction that contains the withdraw without `RefreshReserve`. Assert the daemon's pre-flight refuses to submit; assert that — if it slipped through to the chain — the program would reject with `ReserveStale`.

### 6.9 No-floats lint

- `cargo clippy -- -D clippy::float_arithmetic -D clippy::lossy_float_literal` in CI for the calculator crate, the daemon comparator, and the on-chain program. Adding an `f32` / `f64` anywhere in those paths is a compile error.

### 6.10 `super_max_borrow_rate` width regression

Specifically test the audit-C-3 footgun: a fixture with `super_max_borrow_rate = 300` (i.e. 300%) MUST produce a borrow rate above the `max_borrow_rate` cap (cast to u8 would truncate to 44, silently passing as 44%). This is the canonical "naive cast broke region 3" detector.

---

## 7. Same-Tx Solend Execution Constraints (restated)

These are not new findings — they restate Stage 2 spec § 5.2 and the audit, in one place, for the calculator's reviewers.

### 7.1 `RefreshReserve` + `Withdraw` same tx (audit B-2 / C-1)

- `STALE_AFTER_SLOTS_ELAPSED = 1` (verbatim, [`last_update.rs`](https://raw.githubusercontent.com/solendprotocol/solana-program-library/mainnet/token-lending/sdk/src/state/last_update.rs)).
- `_withdraw_obligation_collateral` rejects with `ReserveStale` if `withdraw_reserve.last_update.is_stale(clock.slot)? && !obligation.borrows.is_empty()`.
- Stage 2's delegated obligation has `borrows.is_empty() == true`, so the stale check is technically skipped on the borrow path. BUT: same-tx refresh is still required for `cToken→liquidity` conversion accuracy, and Stage 2 spec § 5.2 mandates it unconditionally for parity with the borrow-eligible variant.

### 7.2 Variant 15 account ordering (verbatim per § 1 source fetch)

`WithdrawObligationCollateralAndRedeemReserveCollateral`, account list:

| Idx | Account | Signer | Writable |
|---|---|---|---|
| 0 | Source withdraw reserve collateral supply SPL Token account | — | ✓ |
| 1 | Destination collateral token account | — | ✓ |
| 2 | Withdraw reserve account — refreshed | — | ✓ |
| 3 | Obligation account — refreshed | — | ✓ |
| 4 | Lending market account | — | — |
| 5 | Derived lending market authority | — | — |
| 6 | User liquidity token account | — | ✓ |
| 7 | Reserve collateral SPL Token mint | — | ✓ |
| 8 | Reserve liquidity supply SPL Token account | — | ✓ |
| 9 | **Obligation owner** | **✓** | — |
| 10 | **User transfer authority** | **✓** | — |
| 11 | Clock sysvar (optional, will be removed) | — | — |
| 12 | Token program id | — | — |

Data: `collateral_amount: u64` (use `u64::MAX` for "withdraw all").

**Both signer slots** (idx 9 and idx 10) MUST be marked as signers in the tx builder. Both can be the **same** delegated wallet pubkey (it owns the obligation AND is the transfer authority on its own collateral ATA), but the tx builder cannot omit either signer flag — the Solend program treats them as two independent `is_signer` checks.

### 7.3 Delegated wallet ownership boundary

- The Solend obligation MUST be the one created and owned by the **delegated** wallet at authorisation time (Stage 2 spec § 5.1).
- The user's main-wallet obligation (if any) is NOT touched, NOT referenced, NOT readable by Stage 2 code.
- Solend's `_withdraw_obligation_collateral` enforces `obligation.owner == signer` at the program level; the delegated wallet cannot withdraw from another wallet's obligation even if it is somehow tricked into supplying that obligation as input.

---

## 8. Top Risks & Blockers

### Blockers (must be resolved before Stage 2 calculator code is written)

1. **B-O1.** Reserve byte offsets for the rate-config fields (`max_utilization_rate`, `super_max_borrow_rate`, plus re-verification of `optimal_*`/`min_*`/`max_borrow_rate` and `protocol_take_rate`) are NOT in the existing `raw.rs` decoder. **Stage 2 implementation MUST walk the mainnet `Pack::pack_into_slice` for `Reserve` end-to-end, exactly as `raw.rs` already documents for the deposit-only fields, and add the missing offsets with citation comments before writing the calculator.** The live-fixture round-trip test (§ 6.6) is the safety net.

2. **B-O2.** Daemon and program MUST compile against the **same** `SLOTS_PER_YEAR = 63_072_000` constant — pinned in the new `clawsol-solend-rate` crate, asserted in a parity test that imports the constant from both daemon-side and program-side build configs. Audit U-1.

3. **B-O3.** Choice between calculator design (a) (depend on `solend-program` SDK on-chain) vs (b) (port to standalone crate) MUST be made before any code is written. § 4.6 recommends (b); decision documented either way.

### Risks (manageable; not blockers)

- **R-O1. Decimal/Rate parity drift.** Solend's `Decimal` (u128 WAD) and `Rate` (u128, bounded) types have specific overflow / rounding behaviour. The standalone crate (option b) must reproduce **every** intermediate `try_floor` / `try_round` decision exactly. Mitigation: § 6.5 fixture parity tests are bit-equal.
- **R-O2. `ReserveConfig` validity invariants.** The on-chain program enforces `min_borrow_rate ≤ optimal_borrow_rate ≤ max_borrow_rate ≤ super_max_borrow_rate`, `optimal_utilization_rate ≤ max_utilization_rate ≤ 100`, and `protocol_take_rate ∈ [0, 100]`. Our calculator MUST also enforce these on decode and refuse to compute on violations (rather than silently producing nonsense). Mitigation: § 6.1 decode tests.
- **R-O3. Demo-script wording risk.** The current demo script ([`docs/DEMO_SCRIPT_NL_TO_DEFI.md`](DEMO_SCRIPT_NL_TO_DEFI.md)) references "APY < 10%". If the calculator ships APR-only, the demo wording MUST be reconciled to "APR" — or the threshold restated against APY semantics. Mitigation: choose APR; update demo copy in the same PR that lands the calculator.
- **R-O4. Reserve-config drift mid-authz.** A user authorises against `optimal_borrow_rate = 5`; Solend's market admin updates it to `8` mid-authz; the rule's threshold meaning shifts. We treat this as **acceptable**: the user authorised an APR threshold, not a config snapshot, and the threshold continues to map to the live config. The optional `authz_time_config_snapshot` field (§ 5) gives a forensic record. (If we ever want to *fail closed* on config drift, that's a follow-up policy choice — not a v1 invariant.)
- **R-O5. APY ambiguity.** If the demo script says "APY", a reviewer reading the spec will look for an APY computation and find APR. Mitigation: § 4.1 wording hygiene + R-O3 fix.

---

## 9. Open Questions

- **O-1.** Exact byte offsets for `max_utilization_rate`, `super_max_borrow_rate`, `protocol_take_rate` (re-verified against current mainnet `Pack`). § 2.2 / B-O1.
- **O-2.** Should the threshold be stored as `bps` (u32, current recommendation) or as a WAD `u128`? Bps is cheaper and human-readable; WAD avoids any conversion at compare time. Trade-off favours bps for the demo.
- **O-3.** Should the WatchRule include `pyth_oracle_pubkey` / `switchboard_oracle_pubkey` for the reserve, since Solend reads them on `RefreshReserve`? If yes, they become part of canonical-intent hash and rotate-on-Solend-side ⇒ user must re-authorise. Recommendation: **no** — the rule pins `reserve_pubkey + lending_market`, and the oracle pubkeys are reserve-internal config; pinning them adds churn without measurable safety.
- **O-4.** Compute-budget envelope on the on-chain re-verifier. APR alone is cheap; if the program also re-runs `RefreshReserve`-equivalent math (it should NOT — Solend already did), CU could spike. Confirm: program reads the **already-refreshed** reserve account and only does the supply-APR derivation on top. Recommended.
- **O-5.** Multi-reserve future. The B1 schema is single-reserve. If Stage 2.x ever monitors a basket (e.g. "if **any** USDC reserve across pools < 5%"), the schema needs a `reserves: Vec<Pubkey>` and a `comparison_logic: Any|All`. Out of v1 scope; flag for follow-up.
- **O-6.** Daemon poll interval. § 5.2 step 3 says "daemon polls"; cadence not specified. Recommended: every 4 slots (~2 s) for the demo, with a "debounce" to avoid a flapping condition triggering repeatedly. The one-shot completed flag (Stage 2 spec § 3.1.1) bounds the worst-case to one execute per authorisation regardless of poll cadence.

---

## 10. Net-Effect Summary for Implementation Agents

| Item | Recommendation |
|---|---|
| Rate kind to ship in v1 demo | **APR** (not APY). § 4.1. |
| Calculator surface | New crate `clawsol-solend-rate` with `supply_apr_wad(...)`. Pure-Rust u128 fixed-point. Daemon and program both depend on it. § 4.5 / § 4.6. |
| `SLOTS_PER_YEAR` | Pinned `63_072_000` (verbatim Solend mainnet). § 1, § 4.5. |
| Same-tx invariant | `RefreshReserve + WithdrawObligationCollateralAndRedeemReserveCollateral` in one tx. Both signer slots flagged. § 7. |
| WatchRule schema | 11 fields; `formula_version: u8` is the load-bearing version-safety field. § 5. |
| Test coverage | Three-region matrix + parity (daemon vs program, bit-equal) + live-reserve fixture from slot 415475589. § 6. |
| What we are NOT doing in v1 | No APY; no PDA escrow Jupiter; no exact-amount withdraw; no multi-reserve; no oracle-pubkey pinning. |
| Decoder gap to close | Add the rate-config offsets to `raw.rs` (or a sibling decoder module) with citation comments. Re-walk `Pack::pack_into_slice` first. § 2.2 / B-O1. |
| Demo wording | Ensure UI / script say *"supply APR < 10%"*, not *"supply APY < 10%"*, when shipping APR. § 4.1, R-O3. |

---

## Final Report (for the calling agent)

**Files changed.**
- Created `docs/STAGE2_SOLEND_APY_RESEARCH.md` (this file). No other files modified.
- No product code touched.

**Official sources consulted.** § 1. All Solend mainnet branch (`reserve.rs`, `last_update.rs`, `state/mod.rs`, `instruction.rs`, `math/common.rs`); existing repo decoder `crates/gateway/src/integrations/solend/raw.rs`; existing live proof `docs/proofs/SOLEND_USDC_DEPOSIT_PHASE4C_E2E.md` for fixture pubkey.

**Recommended calculator API.** New crate `clawsol-solend-rate` with `supply_apr_wad(&ReserveRateInputs) -> Result<Wad, RateError>`, pure-Rust u128 WAD fixed-point, no Solana / SDK deps. Daemon and on-chain program both depend on it. § 4.5, § 4.6.

**APR vs APY.** Recommend **APR** as the v1 metric (§ 4.1). APY is allowed only as an explicit `RateKind::Apy` extension with separate compute-budget allocation and parity fixtures; not gated on for the demo. Demo wording must reflect APR.

**Recommended WatchRule fields.** § 5: `source_type, reserve_pubkey, lending_market, solend_program_id, comparison, threshold_bps, rate_kind, formula_version, max_reserve_staleness_slots, required_refresh_same_tx, authz_time_config_snapshot (optional)`.

**Top risks / blockers.** § 8. Three blockers: B-O1 (re-verify reserve byte offsets for rate-config fields), B-O2 (pin `SLOTS_PER_YEAR` constant in shared crate), B-O3 (decide standalone-crate vs SDK-dep design). Five risks (R-O1..R-O5), all manageable.

**Open questions.** § 9. Six items, none blocking.

**Git status.**
```
?? config/phantom-mainnet-solend.toml          (pre-existing, untouched)
?? docs/DEMO_SCRIPT_NL_TO_DEFI.md              (pre-existing, untouched)
?? docs/STAGE2_OFFICIAL_DOCS_AUDIT.md          (pre-existing, untouched)
?? docs/WEEKLY_UPDATES.md                      (pre-existing, untouched)
?? docs/STAGE2_SOLEND_APY_RESEARCH.md          (this file, new)
```

**Live RPC:** none. **Deploys:** none. **Pushes:** none. **Code edits:** none.
