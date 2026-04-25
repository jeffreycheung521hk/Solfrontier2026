# Solend / Save USDC Live Deposit Proof — Slice 3G

**Date:** 2026-04-23
**Scope:** Controlled test-wallet live mainnet deposit into Solend/Save USDC reserve.

## Summary

First controlled live deposit on the Solend/Save lending path. A tiny `0.001 USDC` (1,000 raw) was deposited into the Save Main Pool USDC reserve using a test-only wallet. The three-transaction sequence (`obligation setup → RefreshReserve → Deposit`) was exercised under the Slice 3G guardrails: every broadcast preceded by a successful `simulateTransaction` preflight on the exact same signed payload, bounded confirmation polling between transactions, and no other transaction types.

## Target

| Field | Value |
|---|---|
| Reserve | `BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw` |
| Lending market | `4UpD2fh7xH3VP9QQaXtsS1YY3bxzWhtfpks7FatyKvdY` (Main Pool) |
| Mint | `EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v` (USDC, dec=6) |
| Token program | `TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA` (classic SPL Token) |
| Pyth oracle | `Dpw1EAVrSB1ibxiDQyTAW6Zip3J4Btk2x4SgApQCeFbX` (Pyth Solana Receiver, `rec5EK…`) |
| Switchboard | sentinel (`nu111…`) |
| Deposit limit | 75,000,000 UI USDC |
| Pre-deposit headroom | 42,879,629,217,077 raw (≈ 42.88M UI USDC) |

## Amount

- Deposited: **1,000 raw** = **0.001 USDC**
- Hard-coded amount guardrail (`FIXED_AMOUNT_RAW`): 1,000. Any other env value would abort before any RPC work.

## Controlled wallet (test-only)

- Owner / fee payer: `BPfDMmeMBmCbMC1rWh7hwigMBoKGBrKwXxSeUu9hhs5L`
- USDC source ATA: `7LFdKcSV7JQYi3or5y9phHVPjhGigu5DDjUakAFbBmk3`
- Collateral ATA (cUSDC): `BQazv4UQNFV8t4QGVntELr1ee3bCeTN8AvdPGRutFKn7`
- Obligation: `5WxoAkPoMoNZ8E4sGG4GkxKdX1E6qsGW2rHZm2Ethkyo`

Keypair files are stored OUTSIDE the repo. Paths are not included in this proof.

## Transaction signatures

| # | Purpose | Signature | Slot | Status |
|---|---|---|---|---|
| 1 | `CreateAccount` + `InitObligation` | `4Ud5USKMrG3UJsERu9NPjokY1UuNs5X7PDZ55JTP39L68x5jxA6HtK6Cuk1SArB6kFPm1cfM2Pu2b9HPWxm8UL9W` | 415_083_795 | confirmed |
| 2 | `RefreshReserve` | `3pVZoGNyrs7oHBFMzouAA26gW9jKCcopzg1mUUoZGWtNn85FQZjFwqWydFg5Kc9JvieiFw7cFLGcD9ccCu76Spku` | 415_083_800 | confirmed |
| 3 | ATA `CreateIdempotent` + `DepositReserveLiquidityAndObligationCollateral` | `6aU5syvXgg9Ps2r6HFuqMiA4mignBjUSGRbfFreRoUHdSyuadLQfoLdVbVUidJQHLWqmqjaEtFQsVWhAZa4eS7N` | 415_083_804 | confirmed |

Each transaction included a modest priority fee (`ComputeBudgetInstruction::set_compute_unit_price(10_000)` micro-lamports/CU) and used a freshly-fetched blockhash (no reuse across transactions).

## Before / after balances

| Account | Before | After | Delta |
|---|---|---|---|
| Source USDC ATA (raw) | 10,000 | 9,000 | −1,000 (= deposited amount) |
| Source USDC ATA (UI) | 0.010000 | 0.009000 | −0.001 USDC |
| Collateral ATA (raw) | 0 | 0 | 0 (transient; see note below) |
| Obligation account length | (not on chain) | 1,300 bytes | created |
| Obligation deposits | n/a | 1 | this deposit |
| Obligation borrows | n/a | 0 | (deposit-only) |
| Reserve total_supply (raw) | 32,108,279,864,322 (pre-refresh) | 32,120,371,578,862 | +1,000 from this deposit (plus ambient activity) |

**Collateral ATA note:** `DepositReserveLiquidityAndObligationCollateral` mints the reserve's cUSDC to the user's collateral ATA, then immediately transfers those cUSDC into the reserve's `destination_deposit_collateral` pool as the obligation's collateral backing. The user's collateral ATA is therefore transient — its end-state balance is zero. The collateral is represented on-chain inside the obligation account, as confirmed by the decoded `deposits=1` entry.

## Walls cleared

All previously-identified Solend/Save deposit blockers are now sealed end-to-end:

| Wall | Sealed by | Status |
|---|---|---|
| `Custom(9)` `InvalidTokenProgram` | Slice 3D(ii) (mainnet 14-account ABI fix) | CLEARED |
| `Custom(10)` `InvalidAmount` / `deposit_limit=0` | Slice 3D(iii) / 3E(iv) retarget | CLEARED |
| `MissingAta` / SPL account validation | Slice 3D(i) ATA-create bundle | CLEARED on chain |
| `ObligationUninitialized` | Slice 3E(i) builders, Tx 1 broadcast | CLEARED on chain |
| `Custom(14)` `MathOverflow` | Slice 3E(iv) recon → 3E(v) retarget | CLEARED |
| `Custom(1)` `InsufficientFunds` | Slice 3F funding (0.01 USDC) | CLEARED |

## Policy / scope

- **Deposit-only.** No Withdraw, no Borrow, no Repay instructions were broadcast or built.
- **No policy weakening.** `lending/policy.rs`, `gate.rs`, `oracle_decoder.rs`, and Switchboard/classic-Pyth decoder surfaces were not modified by Slice 3G.
- **No oracle aggregation change.** `FeedPublishFreshness` semantics unchanged; Switchboard remains `Unknown`/fail-closed.
- **No production route.** No `daemon.rs`, no approval workflow, no Phantom path, no LLM/agent route, no live-deposit tool was added. The broadcast flow lives entirely inside an opt-in test file gated by `CLAW_LIVE_SOLEND_DEPOSIT_EXECUTE=1`.
- **Controlled wallet only.** Signer is a test-only keypair outside the repo, funded with a trivial amount (0.01 USDC) by the user manually in a prior slice (3F).

## Broadcast discipline

- `skipPreflight = false` on every `sendTransaction`.
- Each transaction: build-once → simulate → (on success) send same signed object.
- Three distinct `getLatestBlockhash` calls, one per transaction.
- `getSignatureStatuses` bounded poll to `Confirmed` before proceeding to the next transaction.
- Post-confirmation account visibility uses bounded retry (5 attempts, 1.5 s backoff).

## Files referenced in this proof

- Test harness: `crates/gateway/tests/live_solend_deposit_execute.rs` (opt-in, default-skip).
- Builders used (unchanged this slice):
  - `integrations::solend::init_obligation::build_create_obligation_account_instruction`
  - `integrations::solend::init_obligation::build_init_obligation_instruction`
  - `integrations::solend::refresh::build_refresh_instructions`
  - `integrations::solend::ata::build_create_ata_idempotent_instruction`
  - `integrations::solend::deposit::build_deposit_reserve_liquidity_and_obligation_collateral_instruction`
- Decoders used (unchanged this slice):
  - `integrations::solend::raw::decode_reserve`
  - `integrations::solend::oracle_decoder::extract_publish_freshness`

## Caveats

- This is a **controlled test-wallet proof**, not a production-ready deposit flow. Daemon wiring, approval workflow integration, LLM/agent routing, and policy re-evaluation against the post-deposit snapshot are all explicitly out of scope.
- `raw::decode_obligation` happened to succeed against the post-deposit obligation because the freshly-initialized account's extended mainnet fields are all zero. The latent mainnet `Obligation` Pack-layout padding drift noted in earlier slices is **not yet fixed** — if/when the obligation accrues borrows or non-zero value fields, the padding-check invariant at offsets 138..202 may reject the data. This is documented future-slice decoder work.
- Policy freshness (`RequireFreshState`) was not re-run against the post-deposit snapshot through the three-layer gate; the proof demonstrates that the on-chain deposit path works, not that a policy-evaluator-driven deposit would have passed.

## Milestone Seal & Phase 4 Readiness

### Audit + decoder status
- **Slice 3G closeout audit: `VerifiedComplete`.** All three transaction signatures reached `finalized` commitment with `err=null`. On-chain obligation, source USDC ATA, collateral ATA, and reserve state all match the proof-doc claims.
- **Slice 3H mainnet-Obligation decoder drift fix: completed.** `raw::decode_obligation` now matches the deployed mainnet `Pack` layout (5 extension fields at bytes 138..188, true 14-byte padding at 188..202, strict 0/1 bool decoding). The previously-noted latent padding-check risk is closed with a dedicated regression test.

### Local test matrix (re-run at seal time, network-free)
- `cargo test -p claw-gateway --lib solend` — **72 passed**
- `cargo test -p claw-gateway --lib lending` — **38 passed**
- `cargo test -p claw-gateway --lib` — **288 passed**
- `live_solend_assembler_smoke` — 1 passed (default skip)
- `live_solend_recon` — 1 passed (default skip)
- `live_solend_deposit_dry_sim` — 5 passed (default skip + 4 offline retarget)
- `live_solend_refresh_proof` — 1 passed (default skip)
- `live_solend_refresh_recon` — 1 passed (default skip)
- `live_solend_deposit_execute` — 1 passed (default skip)

All integration tests are opt-in; the default invocation performs zero network calls.

### V1 scope (sealed by this milestone)
- **Deposit-only.** No Withdraw, no Borrow, no Repay are built or broadcast.
- **No policy weakening.** The three-layer gate, `RequireFreshState`, and `MaxOracleStalenessMs` are unchanged. Switchboard remains `Unknown`/fail-closed.
- **No daemon / approval-workflow / LLM-route integration yet.** The entire Solend broadcast flow is confined to opt-in integration tests with a controlled test wallet.

### Known future-scope items (explicitly deferred)
- **Phase 4 productization** — promote the test-only broadcast sequence into a first-class tool surface (see Phase 4 assumptions below).
- **Switchboard On-Demand decoder** still absent; blocks the majority of Solend reserves outside the Pyth-Receiver subset.
- **Classic-Pyth decoder** still absent; not needed for the current Pyth-Receiver USDC target.

### Phase 4 assumptions (to be enforced by Phase 4A-1 prompt)
- **Assembler placement**: the multi-tx deposit assembler belongs under `crate::integrations::solend::assembler`, not `crate::lending::assembler`. The `lending` module remains protocol-agnostic; Solend-specific orchestration stays inside the Solend subtree (ARCHITECTURE.md §7).
- **Propose stage does not auto-broadcast refresh.** The propose surface returns a descriptor + unsigned plan only; any broadcast decision is a separate approval-gated step.
- **Execute stage initially follows the proven Slice 3G multi-tx sequence**: (1) CreateAccount + InitObligation if needed, (2) RefreshReserve, (3) ATA CreateIdempotent + Deposit. Single-tx packing and ALT (Address Lookup Table) optimization are explicitly future work, not first-pass scope.
- **Session-bound wallet identity.** Signer pubkey comes from the `SessionBoundWallet` seam, never from LLM input (same rule that sealed the A2-Execute LLM-driven proof).

