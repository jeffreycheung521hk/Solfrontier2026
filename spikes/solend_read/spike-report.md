# Solend Read-Only Spike — Report

**Date:** 2026-04-19
**Purpose:** Validate `docs/lending_policy_vocabulary.md` Parts 2, 3A, 3B against real on-chain lending state. Throwaway. No production path touched.

---

## 1. Protocol chosen: Solend

**Why Solend, not Kamino / MarginFi / Drift / port.finance:**

- Active on mainnet (`So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo`), scan-able via public RPC.
- Obligation + Reserve layouts are documented as SPL-style fixed-size packed structs with public `Pack` impl in `solendprotocol/solana-program-library`. This means the layout is a citation, not a guess.
- Public `api.solend.fi` exposes a market-config endpoint that lists lending market pubkeys without authentication — sufficient for target discovery, not used as truth source for on-chain state.
- Multiple secondary markets (Coin98, NFT, AMM, etc.) are small enough to scan with `getProgramAccounts` under public RPC scan-abort limits.

**Why not Kamino etc.:** Their account layouts are Anchor/zero-copy with more moving parts; for a first spike that must validate a schema (not a whole protocol), a simpler packed layout gives cleaner evidence.

**Not a commitment.** Part 6 explicitly defers protocol selection. This spike uses Solend as a single concrete sample; nothing here signals "we have chosen Solend for V1."

## 2. Target discovery — method

Target discovery was done in isolation from layout validation (earlier iteration merged the two; feedback during the spike corrected this). Final pipeline:

1. **Market list** — fetched from `https://api.solend.fi/v1/markets/configs?scope=all&deployment=production`. Returned 8 markets with their lending_market pubkeys. This is the only external-API reliance.
2. **Obligation layout** — cited from `solendprotocol/solana-program-library` `token-lending/program/src/state/obligation.rs` `Pack` impl. Offsets recorded in `spike.py` as a code comment.
3. **Narrow scan** — `getProgramAccounts` against the Solend program id with filters `dataSize=1300` + `memcmp(offset=10, bytes=<lending_market>)`, hitting smaller markets first to stay under public RPC scan limits. `dataSlice` of 210 bytes returned just enough to read `deposits_len` (byte 202) and `borrows_len` (byte 203).
4. **Pick richest non-empty** — sorted by `(borrows > 0, deposits+borrows)` descending; picked the first Coin98 candidate with both arrays populated.

Discovery produced the target. Layout *validation* — "does the spec match real bytes?" — was a separate step run by `spike.py` against the picked target.

## 3. Target used

- **Obligation:** `6rFb29ZAWpPeHTgZ4rcBVTGcL7eG3n2ym4wv2TC667JR`
- **Wallet (obligation owner):** `5YXCR5zWX8Ew1uPQoa2Gr22pm1MvHViQvNerqSBwyw4U`
- **Lending market:** `7tiNvRHSjYDfc6usrWnSNPyuN68xQfKs1ZG2oqtR5F46` (Solend Coin98 pool)
- **Obligation size:** 1300 bytes — matches cited `OBLIGATION_LEN`.
- **Positions:** 3 deposits, 1 borrow. Exercises both arrays in the `data_flat` region.

Run command:

```bash
python spike.py --obligation 6rFb29ZAWpPeHTgZ4rcBVTGcL7eG3n2ym4wv2TC667JR
```

## 4. Raw account types fetched

| Account kind | Count | Pubkey (first seen) | Size | Owner |
|---|---|---|---|---|
| Obligation | 1 | `6rFb29ZA…` | 1300 | Solend program |
| Reserve | 3 | `46Lh1P2X…`, `5twXA9pw…`, `GdJd6a8Z…` | 619 each | Solend program |
| Switchboard On-Demand feed | 3 | `8hhBZDLe…`, `AKp55Lbp…`, `BjUgj6YC…` | 3851 each | `SW1TCH7qEPTdLsDHRgPuMQjbQxKdH2aBStViMFnt64f` |
| Pyth Lazer receiver account | 1 | `Dpw1EAVr…` | 134 | `rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ` |
| Sentinel null-pubkey (placeholder) | 2 occurrences | `nu11111111111111111111111111111111111111111` | n/a | account not present on chain |

**Observation:** Two of the three reserves have `pyth_oracle = nu11…` — a human-readable sentinel pubkey used by Solend to mean "no Pyth oracle configured; this reserve is priced by Switchboard alone." The third reserve has BOTH a Pyth Lazer pubkey AND a Switchboard pubkey configured. This has direct consequences for Part 2 §11 (see §6 below).

## 5. Raw state ⇢ Part 2 schema mapping

### 5.1 Clean mappings (evidence supports the spec)

| Part 2 concept | Where it lives in Solend | Verdict |
|---|---|---|
| `snapshot.session.wallet` | `Obligation.owner` (bytes 42..74) | ✅ decodable as base58 pubkey. |
| `snapshot.protocol_tag` | account's `owner` program == Solend program id | ✅ trivial. |
| `snapshot.fetched_at_slot` | RPC `context.slot` on `getAccountInfo` | ✅ present on Solana RPC. |
| `snapshot.obligation.identifier` | the obligation account pubkey itself | ✅ trivial. |
| `snapshot.obligation.owner` | `Obligation.owner` | ✅ as above. |
| `snapshot.obligation.protocol_last_updated_slot` | `Obligation.last_update.slot` | ✅ present. |
| `snapshot.obligation.lifecycle_markers.stale` | `Obligation.last_update.stale` | ✅ present. |
| `snapshot.reserves[].mint` | `Reserve.liquidity.mint_pubkey` | ✅ present. |
| `snapshot.reserves[].decimals` | `Reserve.liquidity.mint_decimals` | ✅ present. |
| `snapshot.reserves[].protocol_last_updated_slot` | `Reserve.last_update.slot` | ✅ present. |
| Obligation collateral entry | `ObligationCollateral{deposit_reserve, deposited_amount, market_value}` | ✅ decoded cleanly for all 3 deposits. |
| Obligation borrow entry | `ObligationLiquidity{borrow_reserve, borrowed_amount_wads, market_value, cumulative_borrow_rate_wads}` | ✅ decoded cleanly for the 1 borrow. |

### 5.2 Mappings that do NOT clean up against current Part 2

See §6 for the detailed reopens.

## 6. Part 2 assumptions that need to be reopened

These are the material findings. Each is grounded in evidence from the concrete target.

### 6.1 Multi-oracle per priced asset is the *norm*, not a corner case

**Current Part 2 §11** speaks of "the Oracle" (singular) for a priced asset.
**Evidence:** every Solend reserve has TWO oracle slots (`pyth_oracle` + `switchboard_oracle`). Either slot can be the null sentinel. In practice:
- Reserve `46Lh1P2X…`: pyth = null sentinel, switchboard = present
- Reserve `5twXA9pw…`: pyth = null sentinel, switchboard = present
- Reserve `GdJd6a8Z…`: pyth = Pyth Lazer, switchboard = Switchboard On-Demand

**Reopen:** Part 2 §11 should describe Oracle as a *set of feeds per priced asset*, with each feed independently:
- potentially present or absent
- independently versioned / staleness-measured
- with its own provider label
Policy rules like `MaxOracleStalenessMs` (Part 3A §18.2) will then need to decide whether the threshold applies per-feed or to the worst-case feed among all configured feeds for that asset.

### 6.2 Oracle provider heterogeneity is intra-reserve, not just inter-protocol

**Current Part 2 §11** treats `oracle provider label` as a property of each Oracle.
**Evidence:** a single reserve (`GdJd6a8Z…`) uses TWO different oracle providers simultaneously (Pyth Lazer + Switchboard On-Demand), each owned by a different on-chain program. The account *sizes* are different too (134 vs 3851 bytes), confirming they use distinct binary formats.

**Reopen:** Part 2 §11 already says "provider label" is per-Oracle, which is technically fine, but the composite rule in §12.3 ("worst-component dominates") needs clarification: within a single priced asset, multiple feeds from different providers coexist, and policy must choose between *disjunction* (any feed fresh = asset fresh) or *conjunction* (all feeds must be fresh). This is not currently specified.

### 6.3 `stale=True` is the default on-chain state, not a policy-rare edge case

**Current Part 2 §12 / Part 1 §4** frames staleness as a policy-relevant signal.
**Evidence:** the sampled obligation's `last_update.stale = True`. All three reserves' `last_update.stale = True`. Obligation `last_update.slot = 161,416,083`; current RPC context slot = 414,140,784 — a ~253M-slot gap (months). This is because Solend's `RefreshObligation` / `RefreshReserve` instructions are only executed as part of a user-initiated transaction; they don't run passively.

**Reopen:** Part 2 §12 should make explicit that:
- For Solend-style protocols, a raw `getAccountInfo` read will almost always return `stale = True`.
- "Freshness" in V1 must therefore distinguish *account staleness* (the protocol's own freshness marker) from *fetch staleness* (how long ago Claw read it).
- Part 3A's `RequireFreshState` rule (§18.1) needs a concrete definition of which staleness bit it enforces. A reading where it simply enforces `obligation.last_update.stale == False` would HardBlock every ordinary Solend read — which is probably correct (it forces a refresh before policy evaluation), but the spec needs to say so plainly.

### 6.4 The sentinel-pubkey idiom is schema-level, not ignorable

**Current Part 2 §11** describes Oracle identifiers as "the on-chain account pubkey" with no discussion of sentinel values.
**Evidence:** Solend uses `nu11111111111111111111111111111111111111111` (literally "nu11…") as an on-chain sentinel meaning "no oracle of this kind". Naively fetching that pubkey returns 404; naively treating it as an oracle identifier produces `status=NOT_FOUND` noise rather than "this slot is unused".

**Reopen:** Part 2 §11 must acknowledge sentinel values as a protocol-specific concern. Either:
- the schema strips sentinels at assembly time (so policy rules never see them), or
- the schema encodes oracle slots as `Option<Oracle>` with sentinel-recognition delegated to the protocol adapter.
Either choice is defensible; silence is not.

### 6.5 Per-component freshness slots are NOT aligned

**Current Part 2 §12.1** already says component-level freshness descriptors are independent — this is *supported*, not reopened.
**Evidence:** the three reserves in one snapshot carry `last_update.slot` values of 397,756,108 / 210,130,891 / 378,843,460 respectively. The obligation's own slot is 161,416,083. None agree. The §12.3 "worst-component dominates" rule is therefore load-bearing, not theoretical.

**Not a reopen — a confirmation.** Part 2 §12.1 and §12.3 are directly supported by this evidence.

### 6.6 Units mix within a single snapshot

**Evidence:** `obligation.borrows[0].borrowed_amount_wads` is in wads (`10^18`-scaled). `obligation.deposits[0].deposited_amount` is in c-token units (the reserve's collateral token, not the underlying). `reserve.liquidity.available_amount` is in underlying units (e.g., USDC micro-units with 6 decimals). These are three different units in the *same* snapshot.

**Reopen:** Part 2 §10 should require each numeric field to carry a unit tag ("underlying/native", "c-token/collateral", "wads/scaled-1e18", etc.) and §13 should restate that Part 4 metrics are responsible for converting *between* these units — the snapshot MUST NOT silently normalize.

## 7. Part 3A V1 rule answerability

Walking each V1 mandatory rule against what the snapshot carries:

| Rule | Answerable from snapshot alone? | Notes |
|---|---|---|
| `RequireFreshState` (§18.1) | **Partial.** | Snapshot carries `last_update.stale` + `last_update.slot`; rule needs a concrete threshold policy (how stale is too stale?). §6.3 above needs to be resolved first. |
| `MaxOracleStalenessMs` (§18.2) | **Partial.** | Snapshot can expose oracle `publish_slot` (cf. §12.2 "publish_timestamp / publish_slot"), but the spike did NOT decode Pyth Lazer / Switchboard On-Demand binary payloads — just the owner program. Actual extraction of `publish_slot` is a Part 5 decoder problem, not a Part 2 schema problem. The schema shape works; the decoder hasn't been proven. |
| `AllowedLendingProtocols` (§18.3) | **Yes.** | Obligation's account owner == Solend program id. One comparison. |
| `AllowedMints` (§18.4) | **Yes.** | Every reserve's `liquidity.mint_pubkey` is decodable. Policy can enumerate permitted mints and compare. |
| `MaxActionInputAmount` (§18.5) | **Yes** — action-level, not state-level. Independent of snapshot. |
| `RiskReducingActionOnly` (§3.1 / §18.6) | **Yes** — action-level, not state-level. Independent of snapshot. |

**Net:** 4 of 6 V1 rules are directly answerable from today's snapshot shape. The remaining 2 (`RequireFreshState`, `MaxOracleStalenessMs`) are answerable *in principle* but require (a) the §6.3 freshness-bit semantics decision, and (b) Part-5-level oracle binary decoders that the spike did not attempt.

## 8. Part 3B gate / verdict model validity

Walking the three-layer gate model against real state:

### 8.1 System Invariant Gate (§24)

Examples of checks:
- Obligation account owner equals Solend program id → pass
- Obligation data size == 1300 → pass
- Account is not closed / zero lamports → pass

**Verdict:** the gate model survives real state. These are all decidable from what `getAccountInfo` returns before any Part 2 assembly. No evidence of an invariant check that cannot be answered at this layer.

### 8.2 Scope Boundary Gate (§25)

Examples:
- Action is `Deposit` / `Repay` (risk-reducing) → pass; action is `Borrow` / `Withdraw` → HardBlock(`ScopeBoundary`)
- Target protocol ∈ `AllowedLendingProtocols` → pass

**Verdict:** these can be answered entirely from the action payload + a small allowlist. No snapshot needed. Consistent with §25.1.

### 8.3 Rule Evaluation (§26)

Examples walked above in §7. All six V1 rules fit the "evaluated after gates pass, against a valid snapshot" model.

**Verdict:** the three-layer model holds. No evidence of a decision that naturally lives *between* layers or *outside* all three.

### 8.4 Fail-fast / reason categories

The three reason categories (`SystemInvariantFailed`, `ScopeBoundary`, `RuleRejected`) map 1:1 onto the gates. No observed condition required a fourth.

**Verdict on Part 3B:** supported as-is by this evidence.

## 9. Remaining uncertainty

- **Oracle binary decode** — the spike identifies oracle *owner program* (enough to know "this is Pyth Lazer" or "this is Switchboard On-Demand") but does NOT decode `publish_slot`, price, or confidence interval. Those decoders are nontrivial and were deliberately out of scope. `MaxOracleStalenessMs` enforcement remains unproven.
- **Refresh semantics** — Solend's `RefreshObligation` / `RefreshReserve` instructions reset `last_update.stale` to false but were NOT exercised here (read-only). A real V1 path needs to call refresh *before* reading, or define policy as "evaluate against latest-refreshed slot", or accept `stale=True` as a structural `HardBlock(RuleRejected(RequireFreshState))`. §6.3.
- **V1 action implementation** — Solend's deposit/repay/withdraw/borrow *instruction encoding* is not validated here; the spike is read-only. That validation belongs to a separate spike ("can we build a Solend RiskReducing action?").
- **Multi-market isolation** — the target is in Coin98 pool, not Main. Reserves and obligations from different markets are schema-identical but policy rules (e.g., `AllowedLendingProtocols`) may want market-level granularity. §6.1 touches this.

## 10. Recommendation

**Do NOT enter Part 4 yet.** Three small reopens should close first:

1. **Part 2 §11 — Oracle shape.** Restate Oracle as a *set of feeds per priced asset*, with explicit handling of: null-sentinel slots (§6.4), independent provider labels per feed (§6.2), and independent staleness per feed (§6.5-confirmed, already in §12.1). Keep the reopen narrow — no Part 4 metric work.

2. **Part 2 §10 — Unit tags on numeric fields.** Add a short rule that each numeric amount carries a unit tag (underlying / c-token / wads), so Part 4 cannot silently normalize (§6.6).

3. **Part 3A §18.1 — `RequireFreshState` semantics.** Decide: does V1 treat `last_update.stale == True` as a structural HardBlock, or does V1 require the caller to issue a `RefreshObligation`/`RefreshReserve` ix before evaluation, or does V1 compute staleness from `observed_slot - last_update.slot` against a threshold? All three are defensible; the spec must pick one (§6.3).

None of these require reopening frozen Part 3B decisions (fail-closed, two-variant verdict, three-layer gate model, fail-fast ordering). The gate and verdict contracts survived the evidence intact.

After those three reopens are closed, Part 4 (metrics derivation from snapshot) can proceed with confidence.

## 11. Disposal note

This spike is scoped to dispose. After the three Part 2/3A reopens are closed and recorded in `docs/lending_policy_vocabulary.md`, delete `spikes/solend_read/` entirely. Nothing in the production graph depends on it — it is not a crate, not wired to any workspace `Cargo.toml`, and contains no reusable code.
