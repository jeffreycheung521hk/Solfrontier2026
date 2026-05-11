# Stage 2 W5b — Controlled-Wallet Solend USDC Deposit Mainnet Proof

**Status:** Finalized.
**Date observed:** 2026-05-11.
**Harness:** Agent D — W5b live-send slice, default-skip behind double opt-in env gates (W5a preflight pass → W5b execute go).

## On-chain anchor

| | |
|---|---|
| **Transaction signature** | `5BiNNVcKfrRkzx16kM3aTYfEYwP4uCk9grSZ2Jvsig2RQEs4Lgw8uv61hqzzBa2Dqzx5EWHCmbqpdo9w8rDSaLwa` |
| **Solscan** | [`5BiNNVcK…aLwa`](https://solscan.io/tx/5BiNNVcKfrRkzx16kM3aTYfEYwP4uCk9grSZ2Jvsig2RQEs4Lgw8uv61hqzzBa2Dqzx5EWHCmbqpdo9w8rDSaLwa) |
| **Finalized slot** | 418,985,346 |
| **Cluster** | Solana mainnet-beta |
| **Confirmation status** | Finalized (per `getSignatureStatuses` after the harness's confirmation tracker) |

## What this proves

Live controlled-wallet Solend USDC **deposit substrate** end-to-end on mainnet:

- The Stage 2 W5a preflight (live RPC account/program/layout check against Solend mainnet-beta) succeeded for this wallet + reserve + obligation tuple.
- The W5b live-send slice (the controlled-wallet-signs path; **no** clawsol-authority signer in this tx) finalized one Solend deposit on mainnet.
- The instruction sequence assembled by the W5b builder matches the Solend mainnet processor's expectations: `ComputeBudget` setup, `RefreshReserve`, `DepositReserveLiquidityAndObligationCollateral`, and the inner SPL Token movements (transfer of liquidity in, cToken mint, cToken transfer to the obligation collateral account).
- CU budget headroom is healthy: 87,803 CU consumed of 400,000 requested (~22%), leaving ample margin for future co-located ixs.

## What this does NOT prove

> **No-overclaim statement.** This proves live **controlled-wallet** Solend deposit substrate. It does **NOT** prove Stage 2 **clawsol-authority `ExecuteAction`** live withdraw yet.

Specifically, this evidence does not establish:

- That the ClawSol program's `ExecuteAction` ix has been finalized on mainnet under its program authority.
- That a conditional autonomous withdraw triggered by a watch rule has executed on mainnet.
- That sibling-instruction verification (the Stage 2 Jupiter bracket B-1 requirement) has been exercised on a live swap.
- That a real third-party user (rather than the controlled demo wallet) has authorised any Stage 2 path.
- W6 refund / sweep behaviour on mainnet — not in scope for W5b.

The substrate is one rail; the clawsol-authority execute path is a separate rail still to be proven.

## Wallets and accounts

(All abbreviated; full pubkeys verifiable via the Solscan link above.)

| Role | Account |
|---|---|
| Controlled signer wallet (fee payer + obligation owner + user transfer authority) | `BPfDMm…` |
| Source USDC ATA (input liquidity) | `7LFdKcSV…` |
| Solend obligation account | `BdFLjCc…` |
| Solend USDC reserve | `BgxfHJDz…` |

The controlled wallet is the same Slice 3C demo wallet used in prior Stage 1 evidence (`BPfDMm…hhs5L`); the keypair is held off-machine by Jeff. The wallet's role in this tx is **the only signer**: fee payer, obligation owner, and user transfer authority all map to the same controlled key. No clawsol PDA or program-derived signer appears in this tx.

## Amount facts

| | Value |
|---|---|
| Deposit amount (UI) | 1.000000 USDC |
| Deposit amount (raw, 6-decimal) | 1,000,000 |
| Source USDC ATA pre-balance (UI) | 1.653487 USDC |
| Source USDC ATA post-balance (UI) | 0.653487 USDC |
| USDC delta | −1.000000 USDC (matches deposit) |
| cToken (collateral) pre-balance (raw) | 772 |
| cToken (collateral) post-balance (raw) | 772,093 |
| cToken delta | +771,321 raw cToken (consistent with mature reserve exchange rate; cToken < liquidity due to accrued interest) |

The cToken delta is reported as observed; exchange-rate parity against the on-chain reserve state at this slot is implicit in the deposit ix accepting the operation.

## Instruction sequence (program-side outer + inner)

```
[0] ComputeBudget — set_compute_unit_limit
[1] ComputeBudget — set_compute_unit_price
[2] Solend — RefreshReserve
[3] Solend — DepositReserveLiquidityAndObligationCollateral
        ├── inner: SPL Token — Transfer  (USDC liquidity in: source ATA → reserve liquidity supply)
        ├── inner: SPL Token — MintTo    (cToken minted: reserve cToken mint → reserve collateral supply)
        └── inner: SPL Token — Transfer  (cToken → obligation collateral)
```

- `RefreshReserve` and the deposit ix are in the **same transaction** — consistent with the Stage 2 spec § 5.2 atomic-tx requirement (deposit-only obligation does not require `RefreshObligation`).
- `DepositReserveLiquidityAndObligationCollateral` is Solend's compound deposit ix; the SPL Token inner sequence (`Transfer` / `MintTo` / `Transfer`) is the expected processor signature for the variant.
- The processor account ordering and signer/writable mutability were preserved exactly as defined by the Solend mainnet source (Agent I W5a precondition).

## Compute usage

| | |
|---|---|
| CU requested (ComputeBudget) | 400,000 |
| CU consumed | 87,803 |
| Headroom | ~78% (312,197 CU unused) |

The headroom indicates a same-tx ClawSol pre/post bracket would have substantial CU budget to spare for the Stage 2 strong-binding execute path, *if* tx byte size and account-key count also fit. CU is not the binding constraint here.

## Safety posture

This W5b run upholds the W5b safety contract verbatim:

- **Controlled wallet only.** The signer is the off-machine Slice 3C controlled keypair. No production user, no external delegated wallet, no third party.
- **No user main wallet signer.** Zero signing requests against any non-controlled wallet. Jeff's personal main wallet does not appear in the tx.
- **No Jupiter.** This tx is a pure Solend deposit; no Jupiter program id, no swap routing, no sibling-bracket. Jupiter conditional execution remains gated on the J4 sibling-ix verifier and J5 dual-bracket prototype fitting the tx-size budget — neither path was exercised here.
- **No W6 refund.** W6 sweep / refund logic was not invoked. The deposit landed; no compensating tx was issued.
- **No clawsol-authority `ExecuteAction`.** The ClawSol on-chain program's `ExecuteAction` instruction is not present in this tx. This proves the *underlying Solend deposit rail* works under the controlled wallet, not that the Stage 2 program authority signs anything on mainnet yet.
- **Default-skip env gates.** The harness required `CLAW_W5_LIVE=1` (master) and `CLAW_W5B_GO=1` (execute) to reach the send branch. CI / `cargo test` performs zero network calls and cannot reach this code path.
- **One attempt.** No automatic retry on broadcast or confirmation. A failed run produces a typed failure outcome, not a re-broadcast.
- **Programmatic proof-doc trigger.** This document is written only because the harness observed `Finalized` from the confirmation tracker for the signature above.

## Relation to other proofs

- **Stage 1 baseline (predecessor):** Phase 4C and Phase 5G prove the LLM-guided controlled-wallet Solend deposit rail at slot 415,475,589 and 415,571,964 respectively. W5b is the Stage 2 substrate continuation of that lineage — same Solend execution rail, same controlled wallet, exercised today as a precondition for Stage 2 clawsol-authority work.
- **Stage 1 Tail withdraw counterpart:** Phase 6I-K (tx `3tMpTSEn…EZPJZ` at slot 418,395,346, 2026-05-08) proves the controlled-wallet withdraw rail. W5b proves the deposit rail at a later slot today.
- **What is still missing on the Stage 2 ledger:** clawsol-authority `ExecuteAction` mainnet, sibling-ix verifier mainnet, real third-party user delegation. These are listed honestly in `ROADMAP.md` Phase 6 / Long-term Vision and in `STAGE2_DELEGATED_CONDITIONAL_EXECUTION_SPEC.md` § 14 / § 15.

## Verification quick-check

1. Open [`https://solscan.io/tx/5BiNNVcK…aLwa`](https://solscan.io/tx/5BiNNVcKfrRkzx16kM3aTYfEYwP4uCk9grSZ2Jvsig2RQEs4Lgw8uv61hqzzBa2Dqzx5EWHCmbqpdo9w8rDSaLwa).
2. Confirm `Finalized` status at slot 418,985,346.
3. Confirm fee payer / signer is `BPfDMm…`.
4. Confirm the ix sequence matches §"Instruction sequence" above: `ComputeBudget` × 2, `RefreshReserve`, `DepositReserveLiquidityAndObligationCollateral` with the three inner SPL Token movements.
5. Confirm USDC balance delta on `7LFdKcSV…` equals −1.000000 USDC.
6. Confirm cToken balance on the obligation collateral account moved 772 → 772,093 raw.

## Cross-references

- [`../../ROADMAP.md`](../../ROADMAP.md) — Phase 6 Stage 1 Tail context; Long-term Vision Stage 2 framing.
- [`../STAGE2_DELEGATED_CONDITIONAL_EXECUTION_SPEC.md`](../STAGE2_DELEGATED_CONDITIONAL_EXECUTION_SPEC.md) — Stage 2 execution spec (W5a / W5b / clawsol-authority `ExecuteAction` definitions); § 5.2 atomic-tx requirement; § 14 audit corrections.
- [`SOLEND_USDC_DEPOSIT_PHASE4C_E2E.md`](SOLEND_USDC_DEPOSIT_PHASE4C_E2E.md) — Stage 1 Solend deposit baseline.
- [`LLM_SOLEND_MAINNET_E2E_PHASE5G.md`](LLM_SOLEND_MAINNET_E2E_PHASE5G.md) — Stage 1 LLM-guided Solend deposit.
- [`LLM_SOLEND_WITHDRAW_MAINNET_E2E_PHASE6I.md`](LLM_SOLEND_WITHDRAW_MAINNET_E2E_PHASE6I.md) — Stage 1 Tail withdraw counterpart.
- [`INDEX.md`](INDEX.md) — Proof ledger.
