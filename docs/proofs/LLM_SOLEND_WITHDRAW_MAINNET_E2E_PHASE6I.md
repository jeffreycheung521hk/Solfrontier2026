# Phase 6I-K — LLM-Guided Solend USDC Withdraw-All Mainnet Proof

**Date (UTC):** 2026-05-08
**Status:** `Finalized` on Solana mainnet-beta

This is the **withdraw-side counterpart** to the deposit-side
[`LLM_SOLEND_MAINNET_E2E_PHASE5G.md`](LLM_SOLEND_MAINNET_E2E_PHASE5G.md)
proof. The two together close the round-trip: the same wallet
`C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW` that deposited 5 USDC
via the LLM-guided deposit flow recovered its full balance plus accrued
interest via the LLM-guided withdraw-all flow, without the backend ever
signing the user's wallet slot.

## What this proves

The full round-trip:

```
natural-language
  → OpenAI / tool dispatch
    → human approval
      → JIT signing handoff (fresh blockhash)
        → Phantom signature (user-side only)
          → daemon submit + verify
            → mainnet Finalized
```

is now demonstrated on mainnet for **withdraw**, in addition to the
deposit demonstration in Phase 5G / 6B / 6E.

## On-chain outcome

- **Public on-chain hash:** `3tMpTSEnmvszjSwgow43KMd4dLqnZwwSvnTbHkcPsCiyfizLf2h7kcMc1M7LwokHLC3SBPzpTfykjsc48z8EZPJZ`
- **Solscan:** https://solscan.io/tx/3tMpTSEnmvszjSwgow43KMd4dLqnZwwSvnTbHkcPsCiyfizLf2h7kcMc1M7LwokHLC3SBPzpTfykjsc48z8EZPJZ
- **Finalized slot:** 418,395,346
- **Lifecycle states observed:** awaiting_approval → approved → awaiting_signature → submitted → finalized

## Withdraw parameters

- **Protocol:** Solend / Save Main Pool (V1, mainnet)
- **User wallet:** `C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW`
- **Obligation:** `HcKrv5Jo5f6qvzSGhJVYTNSqwKudRizn6fxbjPW7M8SV` (owner == user wallet — confirmed by the Phase 6H scanner)
- **Reserve:** `BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw` (Solend Main Pool USDC)
- **Reserve mint:** `EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v` (USDC, 6 decimals)
- **User USDC ATA:** `4bDArC4UVoBjecHUZaCKqhuhTYPoiKS4qpQQpxuvm1qn`
- **Mode:** withdraw-all (`u64::MAX` collateral sentinel via `solend_withdraw_all_usdc` tool)
- **SOL fee:** 35,150 lamports

### User-visible balance delta

| Account | Before | After | Δ |
|---|---|---|---|
| User USDC ATA | 0.646091 | 5.646487 | **+5.000396 USDC** |

The +5.000396 USDC = 5 USDC principal originally deposited (May 7,
Phase 6G [`yvDsGCJ1…pgq9iM`](https://solscan.io/tx/yvDsGCJ1bMReWxvScPdkrhgZms7aJ9CNwWQuTaEAVNGuKN5attBDpJ7B4ZgVRJPUAtAC628GZaQRLDxRapgq9iM))
+ ~0.000396 USDC accrued reserve interest.

## Proof flow

The flow mirrors the Phase 5G deposit proof; the deltas vs. that
document are highlighted **bold**.

1. RPC freshness check via `get_slot` succeeded.
2. Phase 6H scanner confirmed `obligation.owner == user wallet` —
   funds are user-owned on chain, not daemon-owned, and the withdraw
   is therefore a self-custody recovery, not a custodial release.
3. `wire_chat_handler_with_registry` constructed a real provider chat
   handler attached to the production Solend tool registry —
   **including the `solend_withdraw_all_usdc` tool** registered by
   `wire_solend_withdraw_all_usdc_tool` (Phase 6I-D / E).
4. One natural-language chat turn was sent through
   `ChatHandler::handle_chat`. The provider returned exactly one tool
   call: `solend_withdraw_all_usdc { obligation_pubkey: "HcKrv5Jo…M8SV" }`.
5. The handler dispatched to `SubmitSolendWithdrawAllUsdcTool`, which:
   - re-fetched the obligation account,
   - re-ran the four structural re-checks (owner / lending market /
     non-zero USDC deposit / no borrow),
   - parked a `ParkedSolendWithdrawAllIntent` keyed by a fresh
     `approval_request_id`,
   - **registered a real `ApprovalRequest` in the daemon-wide
     `ApprovalStore`** (Phase 6I-E), and
   - returned `awaiting_approval` (no auto-approve, no auto-sign).
6. The human-approval seam (`ApprovalStore::decide(Approve)` +
   `approval_routing::route_approval_outcome`) approved the request.
7. The withdraw resume task
   (`run_solend_withdraw_resume_task`, Phase 6I-E) awoke on the
   approval signal, **re-ran the obligation invariants against a
   fresh on-chain snapshot**, and signalled JIT-ready.
8. The frontend called the JIT-prepare route
   `POST /sessions/:s/approvals/:a/solend-withdraw-jit/prepare`
   (Phase 6I-F). That route called the production withdraw plan
   assembler with a fresh blockhash and parked the unsigned
   transaction in the same `SolendSigningStore` deposit uses.
9. **Phantom signed only the user wallet slot**; message body and
   obligation slot were not modified.
10. The shared `submit_signed_solend_transaction` path ran the
    verification pipeline (atomic consume, message-hash immutability,
    session + obligation signature checks, blockhash freshness) and
    broadcast.
11. `SolendConfirmationTracker` polled `getSignatureStatuses` until
    `confirmation_status = "finalized"`.
12. This proof document was written only after `Finalized` was
    observed on chain.

## Discovered account-layout / processor-parity fixes

The first three withdraw attempts each reverted on chain (user funds
were never at risk; only the per-tx fee was paid). Each revert pinned
a specific account-meta mismatch between the in-repo Solend SDK
helper and the deployed Solend processor:

| # | Failed tx | On-chain error | Root cause | Fix (commit / phase) |
|---|---|---|---|---|
| 1 | [`4YnQFUTm…Q7gnTv`](https://solscan.io/tx/4YnQFUTmPQEKpDoJRuUS6optFUs8pyxQuFknjFzRAUooWy88YjPQGeAFr9Ncv8JwZPV7xdp2cEjHm2Djk6Q7gnTv) | ix 2 `RefreshObligation` `Custom(5) InvalidAccountOwner` | Builder included `Clock` in `RefreshObligation`, shifting reserve-account positions | `RefreshObligation = [obligation, ...referenced_reserves]`, no Clock (`57f135f`, Phase 6I-H) |
| 2 | [`25REcnNp…zmT3Hp`](https://solscan.io/tx/25REcnNpUTbqx5eLV2L7LcN39vEMP7RZUZ9sNFZcURrBr2S6B7LJVcxzAcRNhtbs9znEswb8Gtob8Bmrk5zmT3Hp) | ix 3 Withdraw `NotEnoughAccountKeys` | Withdraw ix omitted the remaining `deposit_reserve_infos` after the token program | Append all `obligation.deposits` reserves after the token-program slot (`d7d960d`, Phase 6I-I — see #3) |
| 3 | [`2mLa5bQv…PuCpfY`](https://solscan.io/tx/2mLa5bQvTN5daWkC5ehiDDbQvGpyyTZ8qkphs9jUGx8b191Xq54XH1HxVWETr67FYJay6oc3NPnfqAo9tFPuCpfY) | ix 3 Withdraw `Custom(9) InvalidTokenProgram` | The 6I-I fix put `Clock` at slot 11, but the deployed processor expects the SPL Token program at slot 11 | Slot 11 = `Tokenkeg…VQ5DA`; no Clock; deposit reserves appended from slot 12 (`e4195fa`, Phase 6I-J) |
| 4 | `5W3PzXnR…NMNRVg` | ix 3 Withdraw `ReadonlyDataModified` | Slot 4 `lending_market` was readonly in our builder, but the deployed processor writes to `LendingMarket` as part of its rate-limiter update during withdraw | Slot 4 `lending_market` writable (`8d59697`, Phase 6I-K) |

### Final corrected withdraw-ix layout (post-6I-K, mainnet-verified)

- no `Clock` account
- slot 4 `lending_market` writable
- slot 11 SPL Token program (`Tokenkeg…VQ5DA`) readonly
- slot 12+ writable `deposit_reserve` entries in `obligation.deposits` order
- signer set unchanged: wallet at slots 9 and 10

### Process lesson — protocol parity must be source-grade, not SDK-grade

- For protocol-level instruction builders, the source of truth is
  the deployed program's `processor.rs`, not its
  `sdk/src/instruction.rs` helpers. The SDK helper docs and earlier
  prompts trusted inferred layouts; the processor enforces a
  different layout in practice (e.g., `lending_market` writable in
  withdraw because of the rate-limiter write — invisible from the
  SDK).
- Prompts for protocol-level builders must explicitly require a
  processor-parity table: every account slot, with `writable` /
  `signer` flags, cross-checked against `processor.rs` *and* against
  the account list of a known-good live tx of that exact
  instruction. Treat `ReadonlyDataModified` and `InvalidAccountOwner`
  as protocol-level signals, not generic Solana errors.
- For the Solend mainnet processor specifically, the parity audit
  source is
  [solendprotocol/solana-program-library `mainnet` branch](https://github.com/solendprotocol/solana-program-library/tree/mainnet/token-lending),
  with `token-lending/program/src/processor.rs` taking precedence
  over `token-lending/sdk/src/instruction.rs`.

## Round-trip closed

The same `C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW` wallet that
deposited 5 USDC on May 7 via Phase 6G
([`yvDsGCJ1…pgq9iM`](https://solscan.io/tx/yvDsGCJ1bMReWxvScPdkrhgZms7aJ9CNwWQuTaEAVNGuKN5attBDpJ7B4ZgVRJPUAtAC628GZaQRLDxRapgq9iM))
withdrew its full balance plus accrued interest back to the same USDC
ATA on May 8. Funds were always user-owned on chain (Phase 6H
scanner confirmed `obligation.owner == user wallet`); this proof
confirms they are also recoverable through ClawSolana's policy-gated
withdraw flow, not just through direct on-chain Solend client tools.

## Scope — what this proves and does not prove

**This proof demonstrates** the **Stage 1 LLM-driven withdraw-all
flow on mainnet**: a single human-approved, JIT-signed withdraw
turn, with the user holding the only signing authority for the
session-wallet slot.

**This proof does NOT demonstrate** the **Stage 2 W4 automated
daemon execution** path. Stage 2 W4 introduces a `Stage2Executor`
with CAS lease, watcher-triggered conditional execution, and
delegated-authority signing — that path has its own substrate
(`crates/gateway/src/stage2_executor.rs`, `programs/clawsol-authority/`)
and its own (separate, not-yet-mainnet-proven) proof story. Do not
read this artifact as evidence for Stage 2 conditional execution.

## Cross-references

- **Deposit-side counterpart (Phase 5G):** [`LLM_SOLEND_MAINNET_E2E_PHASE5G.md`](LLM_SOLEND_MAINNET_E2E_PHASE5G.md) — same chat → tool → approval → JIT → Phantom → daemon → mainnet flow, on the deposit pipeline.
- **Non-LLM Solend baseline (Phase 4C):** [`SOLEND_USDC_DEPOSIT_PHASE4C_E2E.md`](SOLEND_USDC_DEPOSIT_PHASE4C_E2E.md) — Solend execution rail correct end-to-end without any LLM in the loop.
- **Phase 5 milestone seal:** [`PHASE5_CLOSEOUT.md`](PHASE5_CLOSEOUT.md) — security model, fail-closed defense record, slice-by-slice components that this withdraw flow builds on.
- **Originating deposit (May 7):** [`yvDsGCJ1…pgq9iM`](https://solscan.io/tx/yvDsGCJ1bMReWxvScPdkrhgZms7aJ9CNwWQuTaEAVNGuKN5attBDpJ7B4ZgVRJPUAtAC628GZaQRLDxRapgq9iM) — the 5 USDC principal recovered by this withdraw.

## Safety confirmations

- The LLM proposed only — it never approved, never signed, never submitted, never broadcast.
- The human-approval step explicitly transitioned the workflow via `ApprovalStore::decide`.
- The user's Phantom signed only the user wallet slot of the partially-built transaction.
- The backend never signed the user's wallet slot.
- The submit path verified message-hash immutability across signing.
- The four failed attempts each fail-closed on chain (user funds were never debited; only the per-tx fee was paid).
- On-chain `Finalized` was observed before this proof was written.
- No private-key bytes, provider credentials, or HTTP auth headers were printed, logged, or persisted by the harness.
- No serialised transaction payload or raw byte array is included in this document.

## Evidence

- **Final Solscan tx:** [`3tMpTSEn…EZPJZ`](https://solscan.io/tx/3tMpTSEnmvszjSwgow43KMd4dLqnZwwSvnTbHkcPsCiyfizLf2h7kcMc1M7LwokHLC3SBPzpTfykjsc48z8EZPJZ)
- **Daemon log of the recovery run:** `data/clawd-phase6i-k-live.log`
- **Fix commits:** `57f135f` (Phase 6I-H), `d7d960d` (Phase 6I-I), `e4195fa` (Phase 6I-J), `8d59697` (Phase 6I-K)
- **Source narrative:** [`../WEEKLY_UPDATES.md`](../WEEKLY_UPDATES.md) — "May 8 — Solend withdraw-all mainnet recovery + processor-parity lesson"
