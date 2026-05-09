# Stage 2 — Pyth Price Adapter Research / Spec

**Status:** Research and design only. No product code. No live RPC. No deploy. No push.
**Date:** 2026-05-09.
**Companions:**
- [`STAGE2_DELEGATED_CONDITIONAL_EXECUTION_SPEC.md`](STAGE2_DELEGATED_CONDITIONAL_EXECUTION_SPEC.md) — § 6 Jupiter, § 7.1 Pyth tightening, § 10 Cross-Cutting Invariants, § 11 Implementation Phasing, § 14 Audit Corrections
- [`STAGE2_OFFICIAL_DOCS_AUDIT.md`](STAGE2_OFFICIAL_DOCS_AUDIT.md) — § 2 (P-A1..P-D1 confirmed claims), § 3 (C-7, C-8), § 4 (U-4..U-6), § 5 (R-5), § 9.2 (Pyth pull-integration addendum)

This document specifies the Pyth price-adapter substrate that Stage 2 Scenario B requires: *"If BTC > 75,000 AND ETH > 2,300 AND SOL < 90, use up to 5 USDC to buy SOL with max slippage 1% within 48h."* The on-chain program **must not** trust daemon-reported prices; it re-evaluates each Pyth condition from accounts supplied to `execute_action_v2` using the Pyth Solana Receiver SDK helper.

The adapter has three surfaces: (a) the off-chain daemon API that selects, posts (when sponsored heartbeat is insufficient), and submits price-update accounts; (b) the on-chain program verification API that re-derives the boolean condition from `PriceUpdateV2`; and (c) the canonical-intent `WatchRule` schema fields that pin the condition's parameters into the canonical hash so the daemon cannot rewrite the threshold or the feed identity at execute time.

---

## 1. Constraints From Existing Spec

These are non-negotiable from the Stage 2 spec and audit; the adapter design must satisfy them.

| ID | Constraint | Source |
|---|---|---|
| C-1 | Verify inner `feed_id`, not just account pubkey | spec § 7.1; audit P-A2 |
| C-2 | `verification_level` MUST be `Full`; reject `Partial` | spec § 7.1 (U-5); audit § 9.2 |
| C-3 | Per-action max age: ≤ 30 s for trading triggers; ≤ 60 s for slow conditions | spec § 7.1; audit C-7 |
| C-4 | `max_publish_time_age_seconds >= sponsored_heartbeat_seconds + slot_buffer` | spec § 7.1 |
| C-5 | No EMA fallback on the execute path | spec § 7.1 (U-4) |
| C-6 | Confidence-ratio liveness gate `(conf * 10_000) / abs(price) ≤ max_confidence_bps` | spec § 7.1 |
| C-7 | Adverse-bound pricing for derived `min_output_amount_raw` | spec § 7.1; audit C-8 |
| C-8 | No floats anywhere on-chain or in daemon parity computation | spec § 10 |
| C-9 | Exponent normalisation in `i128` integer space; reject on overflow | spec § 7.1 |
| C-10 | Fail closed on any oracle failure; no fallback feed, no stale-but-close-enough | spec § 7.3 |
| C-11 | Pyth feed pubkey + threshold + comparison + max age all part of `canonical_intent_hash` | spec § 3; design implication of C-1 + replay protection |
| C-12 | Anchor version pinned to one of {0.28.0, 0.29.0, 0.30.1, 0.31.1} for SDK compatibility | audit § 9.2 |

---

## 2. Off-Chain Daemon API

### 2.1 Responsibilities

The daemon is the **untrusted producer** of price-update inputs. Its job is to ensure the on-chain program has a fresh, identity-correct, fully-verified `PriceUpdateV2` account when it submits `execute_action_v2`. It must never report a derived boolean to the program; the program re-derives.

### 2.2 Adapter Trait (proposed)

```rust
pub trait PythPriceAdapter {
    fn pick_price_update_account(
        &self,
        feed_id: &FeedId,
        max_age_seconds: u64,
        verification_level_required: VerificationLevel,
    ) -> Result<PickedPriceUpdate, AdapterError>;

    fn ensure_fresh_price_update(
        &self,
        feed_id: &FeedId,
        max_age_seconds: u64,
        verification_level_required: VerificationLevel,
    ) -> Result<EnsuredPriceUpdate, AdapterError>;

    fn evaluate_condition_offchain(
        &self,
        cond: &PythCondition,
        update: &PriceUpdateV2,
        clock: &Clock,
    ) -> Result<ConditionEvalResult, AdapterError>;
}

pub struct PickedPriceUpdate {
    pub price_update_account: Pubkey,
    pub posted_slot: u64,
    pub publish_time: i64,
    pub age_seconds: u64,
    pub verification_level: VerificationLevel,
    pub price_mantissa: i64,
    pub conf: u64,
    pub exponent: i32,
}

pub struct EnsuredPriceUpdate {
    pub picked: PickedPriceUpdate,
    pub self_posted: bool,                // true if daemon posted the VAA
    pub vaa_post_lamports_spent: u64,     // 0 when sponsored picked
}

pub enum ConditionEvalResult {
    True,
    False,
    StaleOracle,
    ConfidenceTooWide,
    VerificationLevelInsufficient,
    FeedIdMismatch,
}
```

### 2.3 Picking vs Ensuring

- **`pick_price_update_account`** — read-only. Searches the known sponsored shard-0 price-update account for the requested `feed_id`, decodes it, verifies the inner fields (feed identity, verification level, age, confidence ratio). Returns the candidate or `AdapterError::NoFreshUpdate` / `StaleSponsoredHeartbeat`.

- **`ensure_fresh_price_update`** — read-write side-effecting. If `pick` returns stale (sponsored heartbeat lapse) and the action's `max_age` is tighter than the sponsored cadence, the daemon falls back to **self-posting** a fresh Hermes VAA via `post_update_atomic` (or `post_update` after a Wormhole-VAA verify). Self-posting consumes the delegated wallet's SOL gas budget; the cost MUST be visible to the user before the authorisation is signed.

The on-chain program does not distinguish sponsored vs self-posted accounts — both are `PriceUpdateV2` with `verification_level = Full`. It does, however, verify ownership via `Account<'info, PriceUpdateV2>` (Anchor performs the program-owner check automatically per the official docs).

### 2.4 Submission Path

For Scenario B, the execute tx must include **three** price-update accounts (BTC, ETH, SOL) plus the instructions sysvar plus the Jupiter swap accounts. The daemon's submission path:

1. For each Pyth condition in the rule, call `ensure_fresh_price_update`.
2. Off-chain pre-flight: re-evaluate the rule's condition logic on the picked accounts with the same fixed-point math the program will use. If false → do not submit.
3. Build the v0 transaction with the bracket (pre_check_v2 → Jupiter setup → swap → cleanup → post_check_v2), the price-update accounts, ALT references, and the instructions sysvar.
4. Submit. The program does its own check; the daemon's pre-flight is an efficiency filter, not a trust boundary.

### 2.5 Sponsored Self-Post Cost Accounting

Per audit U-6, sponsored self-post is not free. Estimated cost components:

- One signature for `post_update_atomic` (≈ 5 000 lamports base fee × Jito tip, if used).
- Transient `PriceUpdateV2` rent until close (`close_update` reclaims).
- Compute-unit cost of merkle-root + ed25519 signature verification.

The `[delegated_wallet.gas_policy].fee_buffer_sol` value in the existing spec § 4.1 must be sized to cover **at least one self-post per execute tx per stale feed**. For Scenario B with three feeds, the worst case is three self-posts in the same execute window. The Stage 2 substrate spec must record this in the gas-policy commentary.

---

## 3. On-Chain Program Verification API

### 3.1 Anchor Account Definition

```rust
#[derive(Accounts)]
pub struct ExecuteJupiterBuyV2<'info> {
    // ... authorisation PDA, executor, token accounts, instructions sysvar ...

    // One PriceUpdateV2 per Pyth condition, in canonical-intent declared order.
    pub price_update_btc: Account<'info, PriceUpdateV2>,
    pub price_update_eth: Account<'info, PriceUpdateV2>,
    pub price_update_sol: Account<'info, PriceUpdateV2>,
}
```

`Account<'info, PriceUpdateV2>` enforces:
- the account is owned by `pyth_solana_receiver_sdk::ID` (the Pyth Pull Oracle program — pinned via the SDK constant, **not** a hardcoded base58 literal in our code),
- the account discriminator matches the `PriceUpdateV2` struct.

This closes the "account belongs to a different program" attack surface for free.

### 3.2 Per-Condition Re-Evaluation

```rust
fn verify_pyth_condition(
    cond: &PythCondition,
    price_update: &PriceUpdateV2,
    clock: &Clock,
) -> Result<bool, ProgramError> {
    // Step 1 — feed identity, verification level, staleness in one call.
    // get_price_no_older_than enforces verification_level == Full per the SDK source.
    let price = price_update
        .get_price_no_older_than(clock, cond.max_age_seconds, &cond.feed_id)
        .map_err(|e| match e {
            GetPriceError::PriceTooOld => ClawError::PythStale,
            GetPriceError::MismatchedFeedId => ClawError::PythFeedIdMismatch,
            GetPriceError::InsufficientVerificationLevel => ClawError::PythNotFullVerified,
            _ => ClawError::PythInvalid,
        })?;

    // Step 2 — confidence-ratio liveness gate.
    require_confidence_within_bps(&price, cond.max_confidence_bps)?;

    // Step 3 — adverse-bound trigger evaluation in i128 fixed-point.
    let bound = adverse_bound_for_trigger(&price, cond.comparison)?;
    let lhs = normalize_to_threshold_exponent(bound, price.exponent, cond.threshold_exponent)?;
    let rhs = cond.threshold_mantissa as i128;

    Ok(match cond.comparison {
        Comparison::GreaterThan        => lhs >  rhs,
        Comparison::GreaterOrEqual     => lhs >= rhs,
        Comparison::LessThan           => lhs <  rhs,
        Comparison::LessOrEqual        => lhs <= rhs,
    })
}
```

### 3.3 Why `get_price_no_older_than` Is the Right Helper

Per `pyth_solana_receiver_sdk::price_update::PriceUpdateV2::get_price_no_older_than` (source: `target_chains/solana/pyth_solana_receiver_sdk/src/price_update.rs`), the helper performs three checks atomically:

1. `verification_level == Full` → else `InsufficientVerificationLevel`.
2. inner `price_message.feed_id == &feed_id` → else `MismatchedFeedId`.
3. `price_message.publish_time + max_age >= clock.unix_timestamp` → else `PriceTooOld`.

This is exactly the union of audit findings P-B1 + U-5 + C-7. Using the helper rather than re-implementing the checks is preferred: it removes a class of consumer-side parity bugs.

The helper does **not** check the confidence ratio (that is a consumer policy decision) and does **not** apply adverse-bound math. Both are added on top.

---

## 4. WatchRule Schema — Pyth Condition Fields

Each Pyth condition entry inside the canonical intent's `conditions[]` array MUST commit the following fields. Every field is part of the canonical intent hash (Stage 1 Tail rules) so the daemon cannot mutate any of them at execute time without invalidating the authorisation PDA.

```text
PythCondition {
    source_type:                  enum  = PythPrice                                    // discriminant byte
    feed_id:                      [u8; 32]                                             // Pyth feed identifier
    price_update_account:         Pubkey                                               // expected account pubkey (sponsored shard-0 by default)
    comparison:                   enum  = GreaterThan | GreaterOrEqual | LessThan | LessOrEqual
    threshold_mantissa:           i64                                                  // user-supplied threshold integer
    threshold_exponent:           i32                                                  // exponent applied to mantissa, e.g. -2 for "75,000.00"
    max_age_seconds:              u32                                                  // per-condition staleness bound (≤ 30 for trading; ≤ 60 for slow)
    max_confidence_bps:           u16                                                  // liveness gate ceiling, e.g. 50 = 0.5%
    verification_level_required:  enum  = Full                                         // Full only in v1
    bound_mode:                   enum  = AdverseLowerForGT | AdverseUpperForLT | Midpoint
}
```

### 4.1 Field Notes

- **`feed_id` and `price_update_account` are both committed.** `feed_id` is the inner identity check the SDK helper enforces. `price_update_account` is the account pubkey the daemon promises to use; the program does not re-pin the pubkey at execute time (Anchor's `Account<'info, PriceUpdateV2>` already verifies program ownership), but the canonical hash records it so a mid-life daemon swap from sponsored shard-0 to a different writer is visible in the audit trail. Different shards of the same feed are allowed only by re-authorisation.
- **`threshold_mantissa: i64` and `threshold_exponent: i32`** mirror Pyth's own representation. Threshold "75,000" with two-decimal precision is `mantissa = 7_500_000, exponent = -2`. The on-chain comparator widens both sides to `i128` and aligns exponents before comparing.
- **`max_age_seconds: u32`** is per-condition because Scenario B may sensibly mix a tight 15 s feed (BTC) with a looser 30 s feed (SOL). The default invariant `>= sponsored_heartbeat_seconds + slot_buffer` is enforced **at authorisation time** in the daemon's intent-builder (refusing to commit a value that cannot be sustained against the picked feed's writer cadence) and re-enforced **at execute time** by the program through `get_price_no_older_than`.
- **`max_confidence_bps: u16`** unit is basis points of price (e.g. 50 = 0.5 %). The check is `(conf * 10_000) / abs(price.price as i128) <= max_confidence_bps as i128`. Liveness only.
- **`verification_level_required`** is encoded for forward compatibility; v1 demo allows `Full` only and the program rejects anything else. Future variants may permit `Partial { num_signatures: u8 }` after explicit writer pinning.
- **`bound_mode`** records the adverse-bound discipline applied to the **trigger evaluation** (not min-out derivation; that is a separate Jupiter-side field). For Scenario B the canonical mode is:
  - `BTC > 75,000` → `AdverseLowerForGT` → trigger fires when `price.price - conf as i64 ≥ 7_500_000 × 10^-2`.
  - `ETH > 2,300` → `AdverseLowerForGT` → trigger fires when `price.price - conf as i64 ≥ 2_300 × 10^0` (or with `-2` exponent for cents).
  - `SOL < 90` → `AdverseUpperForLT` → trigger fires when `price.price + conf as i64 ≤ 9_000 × 10^-2`.
  `Midpoint` exists for diagnostic / preview use but is not allowed in v1 production triggers.

### 4.2 Borsh Wire Shape

Following the audit's Borsh advisory:
- enums encoded as `u8` discriminant + variant body (no body for unit variants).
- `[u8; 32]` and `Pubkey` (alias for `[u8; 32]`) are 32 raw bytes, no length prefix.
- integers little-endian.
- field order in `PythCondition` is the declaration order above. Reordering changes the canonical hash.

A Pyth condition therefore serialises to: `1 + 32 + 32 + 1 + 8 + 4 + 4 + 2 + 1 + 1 = 86 bytes` per condition. Three conditions = 258 bytes inside the `conditions[]` Borsh vec, plus the 4-byte length prefix. Comfortable inside the canonical-intent budget.

---

## 5. Account List Per Pyth Condition

For each Pyth condition in a rule the execute-tx contributes **one account slot**: the `PriceUpdateV2` account itself.

For Scenario B (3 conditions) the Pyth contribution to the static-account budget is:

| Slot | Account | Writable | Signer |
|---|---|---|---|
| n+0 | BTC price_update_account | no | no |
| n+1 | ETH price_update_account | no | no |
| n+2 | SOL price_update_account | no | no |

Plus one **shared** slot for the Pyth Pull Oracle program id (resolved via Anchor's program-owner check at deserialisation; this is the same slot for all three feeds because Anchor's discrimination is by the account's owner field, not by re-passing the program id account).

Net cost: **3 slots** for Pyth, against the v0+ALT 64-account budget. This is comfortable on its own; the budget pressure comes from Jupiter's route accounts (audit § 9.1: start at `maxAccounts=64`, reduce by 2). Agent I's tx-size report (spec § 6.3.1) MUST include the three Pyth slots in the static count.

The instructions sysvar (1 slot, audit B-1) is independent of Pyth and accounted under the Jupiter bracket.

---

## 6. Fixed-Point Arithmetic Strategy

### 6.1 Representation

Both daemon and program operate on Pyth's native representation:
- `price: i64` (signed mantissa)
- `conf: u64` (unsigned half-width confidence)
- `exponent: i32` (negative for decimal feeds; e.g. `-5` means mantissa is in 10⁻⁵ units)
- `publish_time: i64` (Unix seconds)

User-supplied threshold uses the same shape: `(mantissa: i64, exponent: i32)`.

### 6.2 Comparison Algorithm (no floats)

```text
fn compare_in_i128(price: &Price, threshold_mantissa: i64, threshold_exp: i32, op: Comparison)
    -> Result<bool, OverflowError>:

    // Step A — apply adverse bound.
    bound: i128 = match op:
        GreaterThan|GreaterOrEqual: (price.price as i128) - (price.conf as i128)
        LessThan|LessOrEqual:       (price.price as i128) + (price.conf as i128)

    // Step B — align exponents. We never *down-shift* (would lose precision); we *up-shift*
    // the side with the larger exponent to match the smaller exponent space.
    diff: i32 = price.exponent - threshold_exp
    if diff == 0:
        lhs, rhs = bound, threshold_mantissa as i128
    else if diff > 0:
        // price exponent larger (less negative) → multiply price side by 10^diff
        lhs = bound.checked_mul(pow10_i128(diff)).ok_or(Overflow)?
        rhs = threshold_mantissa as i128
    else:
        // threshold exponent larger → multiply threshold side by 10^(-diff)
        lhs = bound
        rhs = (threshold_mantissa as i128).checked_mul(pow10_i128(-diff)).ok_or(Overflow)?

    return op.apply(lhs, rhs)
```

`pow10_i128(n)` is a const-table lookup (max `n = 38` before `i128` overflows; we reject earlier — n ≤ 24 is the realistic ceiling for Pyth's published exponent range).

### 6.3 Forbidden Patterns

- No `f32` / `f64` / `From<f64>` anywhere in the program crate or in the daemon's parity-computation crate. Enforced via clippy `disallowed_types`.
- No silent saturating arithmetic on the comparison path. Use `checked_*` and propagate overflow as a hard error (`PythComputeOverflow`). Saturating arithmetic on a price comparison is "wrong answer, no error" — disallowed.
- No `as` casts that narrow `u64` → `u8` / `i64` → `u8`. The Solend audit C-3 surfaced one such regression (super_max_borrow_rate); the same lint must catch any Pyth-side analogue.

### 6.4 Why `i128`

Worst case: `price.price ≈ 2^63`, `conf ≈ 2^64` → bound up to `2^64`. Multiplied by `pow10(24) ≈ 2^80` → `2^144`. That overflows `i128` (max `2^127`). For this reason the comparator MUST reject thresholds with `|diff| > 18` or any path where the product would exceed `i128::MAX`. The audit's "reject on overflow rather than wrap" wording (spec § 7.1) matches this design.

For the realistic Pyth feeds (BTC/ETH/SOL exponents typically `-8` to `-5`) and human thresholds (exponent `-2` for cents), `diff` is in `[-3, 6]` and never approaches the overflow ceiling. The check is a regression guard, not an expected runtime path.

---

## 7. Adverse-Bound Pricing Rules

Two distinct adverse-bound applications, with different motivations, apply at different points in the pipeline.

### 7.1 Trigger-Evaluation Adverse Bound (this adapter)

**Purpose:** never let confidence-interval width fool the trigger into firing on a price that *might* not actually satisfy the user's condition.

| Comparison | Adverse bound | Rationale |
|---|---|---|
| `GreaterThan` | `price - conf` (lower) | Fire only if even the worst-plausible-low price exceeds the threshold. Prevents firing when `price = 75,001`, `conf = 50` while the true price could be `74,951`. |
| `GreaterOrEqual` | `price - conf` (lower) | Same. |
| `LessThan` | `price + conf` (upper) | Fire only if even the worst-plausible-high price is below the threshold. For `SOL < 90` with `price = 89.95, conf = 0.10`, true price could be `90.05` — do not buy. |
| `LessOrEqual` | `price + conf` (upper) | Same. |

The user wins (no false positive) at the cost of slightly delayed triggers when prices are near the threshold and confidence is wide. This is the right trade-off for the Stage 2 demo; loosening it is a future scope item.

### 7.2 Min-Output Adverse Bound (Jupiter side, already in spec § 7.1 / audit C-8)

**Purpose:** never let confidence-interval width fool the daemon into computing a too-loose `min_output_amount_raw` for the swap leg.

For "buy SOL with X USDC", let `Q` be the price of SOL expressed in USDC per SOL. The daemon computes:

```text
expected_sol_out  = X_usdc / Q
min_out_sol_raw   = expected_sol_out * (1 - slippage_bps / 10_000)
```

The user's protection is **tighter** when `min_out_sol_raw` is **larger**. `min_out` grows as `Q` shrinks. So the daemon must use the **lower bound** of the output asset's price:

- **Buy of the output asset (SOL in Scenario B):** `Q := price - conf` for the SOL/USD feed → the daemon assumes SOL is at the cheap end of the confidence interval, demands more SOL on the fill, the user is protected if the executed price is anywhere within the interval.
- **Sell of the input asset (symmetric):** `Q := price + conf` if the user is selling, applied to the input asset's price.

Why not `price + conf`? Using `price + conf` as `Q` makes `min_out` smaller. A loose `min_out` lets a bad fill pass — which is exactly the failure mode the bracket exists to prevent.

Audit C-8 is correct as written: *"Buy of the output asset → use `price - conf` (the worst plausible buy price for the user)"* — i.e. the price interpretation that is worst for the user *if it turned out to be wrong* is the one we must commit to up-front, so the swap fails closed rather than open.

### 7.3 The Two Bounds Are Not the Same

The trigger bound (§ 7.1 above) and the min-out bound (§ 7.2) can disagree on the same feed in the same execute tx:

- Trigger: `BTC > 75,000` uses BTC `price - conf` (lower).
- If we additionally used BTC price somewhere in min-out math (we don't in Scenario B; only SOL is the swap output), it would also use the lower-of-two-evils bound for that purpose. They can coincide or not depending on which side of the pair you're on.

Implementation rule: each adverse-bound application is computed in the function that needs it; do not share a single "adversely-bounded price" variable across trigger and min-out paths. The naming will lie.

---

## 8. Max Age & Confidence Defaults

### 8.1 Per-Action Defaults

| Action context | `max_age_seconds` (default) | `max_confidence_bps` (default) |
|---|---|---|
| Jupiter price-trigger leg (BTC, ETH, SOL) | **30** | **50** (= 0.5 %) |
| Solend supply-APY trigger (Pyth not used) | n/a — Solend reserve, not Pyth | n/a |
| Demo-only / showcase / Playwright | **60** (relaxed; clearly demo) | **200** (= 2 %) |

The 30 s figure aligns with Pyth's own canonical sample (`let maximum_age: u64 = 30;` per audit § 9.2).

### 8.2 Sponsored Heartbeat Tension

Sponsored push feeds for **BTC/USD, ETH/USD, SOL/USD on Solana mainnet** all heartbeat at **60 s / 0.5 % deviation** (per Pyth's own sponsored-feeds docs, fetched 2026-05-09). The invariant `max_age >= heartbeat + slot_buffer` requires the consumer max_age to be **at least ~63 s** when relying on sponsored cadence alone.

This is a **hard collision** with the trading default of 30 s.

Resolution: the daemon MUST self-post a fresh Hermes VAA before each execute tx whenever the picked sponsored update has age `≥ max_age - margin`. For Scenario B with `max_age = 30`, the daemon's `ensure_fresh_price_update` self-posts when the sponsored update is older than ~10 s and the next sponsored heartbeat is more than ~10 s away. The cost is in the gas budget (§ 2.5).

A user who explicitly opts into a 60-s `max_age` (slow-trading mode, recorded in the canonical intent and visible in the UI consent) avoids the self-post path. v1 demo uses tight 30 s by default with a UI affordance to relax.

### 8.3 Demo-Mode Relaxation

The "demo / showcase" relaxation MUST NOT silently degrade a real authorisation. It is gated:
- `#[cfg(feature = "demo-tools")]` in the daemon config loader,
- recorded in the canonical intent's `safety_profile = Demo` discriminant (separate field; not part of the threshold but part of the hash),
- the program rejects an authorisation whose `safety_profile = Demo` if the program build does not also enable the demo feature (mainnet builds do not).

This mirrors the spec § 9.3 force-tick gate — demo affordances visible in dev, removed from production binaries.

---

## 9. Verification Level Requirement

### 9.1 v1 Mandate: `Full` Only

Per audit U-5 and Pyth SDK source (verified 2026-05-09), `VerificationLevel` is:

```rust
pub enum VerificationLevel {
    Partial { num_signatures: u8 },
    Full,
}
```

`get_price_no_older_than` enforces `Full` internally. v1 program code paths use this helper exclusively; no path calls `get_price_unchecked` (that helper exists for diagnostic use and is forbidden in execute paths).

### 9.2 Why Not `Partial`

Partial updates accept fewer than the two-thirds-of-guardians signature threshold. Without also pinning the writer, a `Partial` update is meaningfully weaker than `Full`. The audit notes the receiver explicitly exposes `addPostPartiallyVerifiedPriceUpdates` as a single-tx-fit optimisation; consumers that don't pin the writer accept a stranger's data.

For Stage 2 we have the budget for `Full` (one PriceUpdateV2 account per feed; sponsored writers post `Full` updates at heartbeat). The savings from `Partial` are not worth the surface.

### 9.3 Future: Writer-Pinned Partial

If tx-size pressure ever forces `Partial`, the v1.5+ design must add a `expected_write_authority: Pubkey` field to `PythCondition` and reject updates whose `write_authority != expected_write_authority`. That is **not** v1 work.

---

## 10. Sponsored Heartbeat Caveat (operator-facing)

This section is reproduced for reference in operator runbooks; it is not a code requirement.

- Pyth's sponsored push-feed program covers BTC/USD, ETH/USD, SOL/USD on Solana mainnet+devnet at **1 minute heartbeat / 0.5 % deviation** (Pyth docs, 2026-05-09).
- Pyth provides **no SLA** on sponsored feeds. They have lapsed historically.
- Pyth's own docs recommend operators run an independent price-pusher as backup (audit R-5).
- The Stage 2 daemon's self-post path (§ 2.3) is the v1 backup. It is **not** a long-term substitute for an independently-pushed feed; if a sponsored feed lapses for hours, every execute pays the self-post cost.
- Operator alert: the watcher MUST surface "sponsored feed last heartbeat > 90 s ago, falling back to self-post" as a visible status and an audit event. The user is paying gas for it.

---

## 11. Implementation Risks

### 11.1 Devnet vs Mainnet Feed Availability

| Feed | Mainnet | Devnet | Risk |
|---|---|---|---|
| BTC/USD | sponsored 60 s | sponsored 60 s | none — both networks covered |
| ETH/USD | sponsored 60 s | sponsored 60 s | none |
| SOL/USD | sponsored 60 s | sponsored 60 s | none |
| **Receiver program** | live | live | none — the same `pyth_solana_receiver_sdk::ID` constant resolves on both networks |

For Stage 2 S4 (Pyth on devnet) the path is identical to mainnet aside from feed-id values. This is the safest of the Stage 2 substrates.

### 11.2 Tx Account Pressure (Scenario B)

Three Pyth slots + 1 instructions sysvar slot + ClawSol pre/post bracket (~6 slots) + Jupiter route (~50 slots at `maxAccounts=64` minus Pyth & sysvar headroom) + ALT pubkey references → near the v0+ALT 64-account ceiling. The Agent I tx-size report (spec § 6.3.1) MUST include these line items. **Mitigation:** start the daemon's `/quote` request with `maxAccounts=58` (= 64 − 3 Pyth − 1 sysvar − 2 bracket-overhead) for the 3-condition Scenario B variant. Iterative tightening per audit § 9.1 is then about route quality, not budget headroom.

### 11.3 Program Crate Dependency Size

The `pyth_solana_receiver_sdk` crate's BPF footprint is non-trivial. Before committing to it as a dependency:
- Compile a stub program (just the `Account<'info, PriceUpdateV2>` deserialiser + `get_price_no_older_than`) for BPF and measure `.so` size.
- If the SDK pulls in `wormhole-sdk` or merkle-tree code transitively beyond what we need, evaluate a feature-flag prune.
- Fallback (only if SDK is too heavy for BPF): hand-rolled `PriceUpdateV2` deserialiser with the same field layout, **but** lifting only the exact same checks the SDK helper performs. This is a maintenance cost (every Pyth schema bump becomes our problem) and should be a last resort. The audit's Anchor-version pinning (0.28..0.31.1) makes the heavy-SDK path strongly preferred.

### 11.4 Receiver-Account Availability At Execute Time

A sponsored shard-0 price-update account exists only after the first push for that feed on that network. For mainnet and devnet, all three (BTC/ETH/SOL) are continuously pushed; account is always live. For a *new* feed Stage 2 might want to add later, the daemon must `getAccountInfo` to check existence before using a canonical-intent that pins it. v1 demo: BTC/ETH/SOL only, no surprise.

### 11.5 `max_age` Floor Below Sponsored Cadence

The C-4 invariant (`max_age >= sponsored_heartbeat + slot_buffer`) and the trading default (≤ 30 s for Jupiter triggers) collide for sponsored-only consumption. Resolved by self-post (§ 2.3, § 8.2). Risk: a daemon bug that *thinks* it self-posted but didn't (e.g. tx dropped, signature failed) — execute tx would land with a stale sponsored account and the program would correctly reject. Daemon must observe the self-post tx confirmation **before** building the execute tx.

### 11.6 Self-Post Cost Visibility

If sponsored heartbeat is sufficient (60 s `max_age` mode), zero extra cost.
If self-post is needed (30 s `max_age` mode), one extra signature + transient rent **per stale feed per execute**.
A worst case where all three feeds are stale at trigger time = three self-posts in a single watcher tick. The gas-policy `fee_buffer_sol` must accommodate that worst case, not the average.

### 11.7 Open Risks Tracked in § 13

- Hermes API rate-limiting (no first-party budget guidance).
- Self-post race vs sponsored writer landing in the same slot (one wins, daemon wastes a tx).
- Ed25519 verification compute-unit cost on the self-post path (CU budget).

---

## 12. Recommended Tests

### 12.1 Unit Tests (no live RPC)

- **Exponent handling fixtures.** `(price, exponent, threshold, threshold_exp)` matrix covering: same exponent, price exponent more negative, price exponent less negative, both negative, edge `diff = 0`, `diff = ±18`, deliberate overflow → `PythComputeOverflow`.
- **Stale price rejected.** Synthesise `PriceUpdateV2` with `publish_time = clock - 31` and `max_age = 30` → expect `PythStale` (via `GetPriceError::PriceTooOld`).
- **Confidence too wide rejected.** `price = 100_000_000`, `conf = 1_000_000`, `max_confidence_bps = 50` → ratio = 100 bps > 50 → `PythConfidenceTooWide`.
- **Wrong feed_id rejected.** Pass a `PriceUpdateV2` with `feed_id = ETH_FEED` against a condition expecting `BTC_FEED` → expect `PythFeedIdMismatch` (via `GetPriceError::MismatchedFeedId`).
- **`Partial` verification rejected.** Synthesise update with `verification_level = Partial { num_signatures: 5 }` → expect `PythNotFullVerified` (via `GetPriceError::InsufficientVerificationLevel`).
- **`get_price_unchecked` not callable from execute paths.** clippy `disallowed_methods` lint catches it; CI fails on regression.

### 12.2 Adverse-Bound Tests

- **GreaterThan boundary.** `price = 75_001, conf = 50, threshold = 75_000` (same exponent) → `bound = price - conf = 74_951 < 75_000` → trigger **does not** fire.
- **GreaterThan clearly above.** `price = 75_100, conf = 50, threshold = 75_000` → `bound = 75_050 > 75_000` → trigger fires.
- **LessThan boundary.** `price = 89_99, conf = 5, threshold = 90_00` (exp -2) → `bound = price + conf = 90_04 > 90_00` → trigger does **not** fire.
- **LessThan clearly below.** `price = 89_50, conf = 5, threshold = 90_00` → `bound = 89_55 < 90_00` → trigger fires.
- **Symmetry.** For each direction, a midpoint-comparison fixture is asserted to fire **earlier** (more permissively) than the adverse-bound version. This catches a regression that flips the bound sign.

### 12.3 Multi-Condition AND Tests

- Scenario B golden path: BTC, ETH, SOL all pass adverse-bound check → `verify_all_conditions(rule)` returns `true`.
- One feed stale, two pass → `verify_all_conditions` short-circuits to false on the stale one.
- One feed confidence too wide, two pass → `verify_all_conditions` short-circuits.
- All three pass at midpoint but ETH fails at adverse bound → `verify_all_conditions` returns false.

### 12.4 No-Float Lint

- clippy `disallowed_types = ["f32", "f64"]` on the program crate and the daemon's pyth-parity crate.
- CI runs `cargo clippy -- -D warnings` and fails on any float type in those crates.

### 12.5 Daemon Parity Fixtures

- Recorded `PriceUpdateV2` byte fixtures from devnet (BTC, ETH, SOL).
- Daemon's Rust evaluator and the program's evaluator both deserialise the same bytes, both run the same comparator, both produce the same boolean.
- CI fails if daemon and program disagree on any fixture. (Same parity discipline as Solend APR fixtures, audit C-2.)

### 12.6 Borsh Wire Shape

- Round-trip: serialise a `PythCondition` instance, deserialise, assert byte-identical.
- Hash stability: changing field order, threshold mantissa, feed_id, or `max_age_seconds` produces a different `canonical_intent_hash`. Catches the "I added a field and forgot to bump `schema_version`" regression.

---

## 13. Open Questions

1. **Self-post tx-size budget.** A Pyth `post_update_atomic` is itself a non-trivial tx (merkle proof + ed25519 verify). It cannot be folded into the execute tx; it must be submitted ahead of time. Operationally fine but adds a confirmation-wait dependency that the watcher's "trigger fires" → "execute tx confirmed" SLO must accommodate.
2. **`safety_profile` field placement in the canonical intent.** Currently proposed as a top-level intent field, not a per-condition field. Confirm with Stage 2 substrate spec authors.
3. **Demo-mode `max_age = 60`** still requires the sponsored writer to be punctual. If a sponsored heartbeat lapses for 9 minutes (the WSTUSR/USR.RR outlier in Pyth's sponsored-feeds table), the demo path also self-posts. Demo UX should surface this honestly rather than pretend "demo = no gas." Tracking decision: do we keep `max_age = 60` as the demo default, or relax to e.g. 90 to absorb the 9-min worst-case? Recommendation: keep 60 and rely on self-post; relaxing makes the demo dishonest about real timing.
4. **Hermes API rate limits.** No first-party budget on `lite-api.jup.ag` analogue for Hermes. If the watcher is polling at 5 s and self-posts every other tick during a heartbeat lapse, we may hit per-IP throttling. Hermes operator budget needed before mainnet rehearsal.
5. **Ed25519 CU cost on self-post.** `post_update_atomic` invokes `ed25519_program` for guardian signature verification. CU cost is non-trivial; if it pushes against the per-tx CU ceiling, falling back to `post_update` (Wormhole-pre-verified VAA) is heavier in tx count but lighter per tx. Measurement target before mainnet.
6. **Account-rent recovery for self-posted PriceUpdateV2 accounts.** `close_update` reclaims rent. Should the daemon close after every execute tx, or batch-close periodically? Closing immediately is safer (no stale accounts of unknown provenance); batching is cheaper. v1 default: close immediately.
7. **`bound_mode = Midpoint` in v1?** Currently disallowed for production triggers. Confirm whether the showcase / preview UI may compute and display a midpoint result for explanatory purposes — answer = yes, but the canonical intent always commits the adverse-bound mode, never midpoint.
8. **Per-shard fallback.** Pyth supports multiple shards per feed for tx-size optimisation. v1 pins shard-0 (the default sponsored shard). If shard-0 has a writer issue, the daemon does not fall over to shard-1 (would change the canonical-intent commitment). Future-scope.
9. **Receiver program id pinning.** This research relies on `pyth_solana_receiver_sdk::ID` for Anchor's program-owner check. The agent's WebFetch returned candidate base58 values for the constant; this doc deliberately does **not** quote those literals because the model-summary path is not the right place to source a load-bearing pubkey. Implementation must read the constant from the SDK at compile time and cite the SDK commit hash in the build manifest.

---

## 14. Sources Consulted

All sources accessed 2026-05-09. No live RPC.

### Pyth Network
- [Pull-integration / Solana](https://docs.pyth.network/price-feeds/core/use-real-time-data/pull-integration/solana) — canonical Rust sample, 30 s `maximum_age`, Anchor compatibility list (0.28.0 / 0.29.0 / 0.30.1 / 0.31.1), `Account<'info, PriceUpdateV2>` ownership pattern.
- [Best practices](https://docs.pyth.network/price-feeds/best-practices) — confidence-interval discipline, adverse-bound discipline (lower for collateral / asset valuation; upper for liability / debt valuation), strict staleness for trading, fixed-point integer math.
- [Sponsored push feeds (Solana)](https://docs.pyth.network/price-feeds/core/push-feeds/solana) — BTC/ETH/SOL all 1 min heartbeat / 0.5 % deviation on mainnet+devnet; outliers (CASH 30 s; WSTUSR 9 min); shard-0 default pubkeys.
- [`pyth_solana_receiver_sdk/src/price_update.rs`](https://github.com/pyth-network/pyth-crosschain/blob/main/target_chains/solana/pyth_solana_receiver_sdk/src/price_update.rs) — `PriceUpdateV2` layout, `VerificationLevel { Partial { num_signatures: u8 }, Full }`, `get_price_no_older_than` enforces `Full` + feed_id + age atomically, `GetPriceError` variants.
- [`pyth_solana_receiver_sdk/src/lib.rs`](https://github.com/pyth-network/pyth-crosschain/blob/main/target_chains/solana/pyth_solana_receiver_sdk/src/lib.rs) — receiver program id constant exposed as `ID`; do not quote a literal pubkey from this research, pin via the SDK constant at build time.
- [`programs/pyth-solana-receiver/src/lib.rs`](https://github.com/pyth-network/pyth-crosschain/blob/main/target_chains/solana/programs/pyth-solana-receiver/src/lib.rs) — three entry points (`post_update`, `post_update_atomic`, `post_twap_update`); price-update accounts are arbitrary keypairs, not PDAs; `write_authority` controls re-writes.

### Already-confirmed sources from `STAGE2_OFFICIAL_DOCS_AUDIT.md`
- All P-A1 / P-A2 / P-A3 / P-B1 / P-D1 references stand.
- All Solana / Anza references for `instructions sysvar`, transaction sizing, ALT budgeting stand (relevant to § 11.2 budget calculation).

---

## Final Report

**File created:** `docs/STAGE2_PYTH_ADAPTER_RESEARCH.md` (this file). No other files modified.

**No product code changes. No live RPC. No deploy. No push.**

### Recommended Adapter API (summary)
- Off-chain trait `PythPriceAdapter` with `pick / ensure / evaluate` methods (§ 2).
- On-chain re-verifier uses `Account<'info, PriceUpdateV2>` + `get_price_no_older_than(&Clock, max_age, &feed_id)` from `pyth_solana_receiver_sdk` (§ 3).
- Confidence-ratio gate + adverse-bound trigger evaluation in `i128` fixed-point on top of the SDK helper (§ 6, § 7).

### Recommended WatchRule Pyth Fields
`source_type, feed_id, price_update_account, comparison, threshold_mantissa, threshold_exponent, max_age_seconds, max_confidence_bps, verification_level_required, bound_mode` — all part of `canonical_intent_hash` (§ 4).

### Top Risks / Blockers
1. Sponsored heartbeat (60 s) collides with trading default (≤ 30 s) → daemon self-post path is mandatory; gas policy must size for it (§ 8.2, § 11.6).
2. SDK BPF footprint not yet measured; fallback to hand-rolled deserialiser is undesirable but possible (§ 11.3).
3. Three-feed Scenario B static-account count + Jupiter route + sysvar pushes against v0+ALT 64 ceiling — Agent I tx-size report must itemise (§ 11.2).
4. Self-post tx confirmation latency adds to "trigger fires" → "execute tx confirmed" SLO (§ 13.1).

### Open Questions (carried into § 13)
- Hermes rate limits, ed25519 CU cost on self-post, account-rent close cadence, shard fallback policy, demo-mode default, `bound_mode = Midpoint` UI exposure, receiver program-id pinning at build time.

### Git Status (post-write)
```
 M docs/proofs/phantom_a2_bind.html        (NOT this agent — concurrent agent in same worktree)
?? config/phantom-mainnet-solend.toml
?? docs/DEMO_SCRIPT_NL_TO_DEFI.md
?? docs/STAGE2_OFFICIAL_DOCS_AUDIT.md
?? docs/STAGE2_PYTH_ADAPTER_RESEARCH.md    <-- new this run (Agent O1 — Pyth)
?? docs/STAGE2_SOLEND_APY_RESEARCH.md      (NOT this agent — concurrent agent in same worktree)
?? docs/WEEKLY_UPDATES.md
```

**Worktree-isolation flag.** Branch was clean at the start of this run (only the four pre-existing untracked items). The `phantom_a2_bind.html` modification and `STAGE2_SOLEND_APY_RESEARCH.md` appeared mid-run, indicating one or more concurrent agents are operating on the same working tree. Per the project's per-agent-clean-branch policy, future Stage 2 substrate agents should be dispatched with isolated worktrees. This agent's only output is `docs/STAGE2_PYTH_ADAPTER_RESEARCH.md`; no other tracked or untracked file was touched.

**End of research / spec.**
