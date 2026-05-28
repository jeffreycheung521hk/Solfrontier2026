# Seven fail-closed lessons on Solana mainnet — what 60 days shipping solo taught me about Solend processor parity

## The moment

At slot 418,395,346 on May 8, 2026, Solana's mainnet finalized [`3tMpTSEn…EZPJZ`](https://solscan.io/tx/3tMpTSEnmvszjSwgow43KMd4dLqnZwwSvnTbHkcPsCiyfizLf2h7kcMc1M7LwokHLC3SBPzpTfykjsc48z8EZPJZ), the transaction that closed an LLM-guided Solend withdraw-all round-trip on a controlled test wallet. It was the eighth instruction-3 attempt against Solend's deployed token-lending processor that day. The previous four had each reverted on chain. The one before that was signed locally but never finalized on chain; our local confirmation tracker observed the drop, and there is no Solscan-linkable trace for it. Two more, weeks earlier, had been rejected before any signature was created at all.

Sixty days. Around 175 commits. Seven mainnet-finalized success transactions across the build. Seven fail-closed incidents along the way. Zero user funds lost.

I came into Solana in late 2025 as a retail trader. I entered crypto in 2022, after FTX, with a Binance account and a Solflare wallet, then learned my way through Orca and Raydium liquidity pools before deciding the gap I kept seeing in autonomous DeFi tooling was worth eight weeks of solo nights to prototype. I had done one or two prior Chainlink hackathons on oracle-guard themes; this is my first Solana hackathon and my first Solana builder relationships. I had no warm intros to Helius or Jito or Squads going in.

This post is about the failures, not the successes. I'm publishing them because they were more useful than the successes, and because every Solana builder I've talked to since has hit at least one of the same parity walls.

A short honesty paragraph up front, because this is a hackathon post and overclaim is its own failure mode. Of the 7 incidents I'll walk through, **3 left full on-chain failed-tx traces you can verify on Solscan, 2 were caught pre-submission by code-level guardrails before any tx existed, and 2 are code-supported with the failed-tx signatures truncated in our internal record.** All 7 share one outcome: zero user funds lost, only per-tx fees paid on the on-chain reverts. This is hackathon scope, not production-deployed, not audited. The mainnet proofs validate the control-plane mechanics; they are not a green light to point at live treasury balances.

## 30-second architecture

ClawSolana is an off-chain transaction control plane for Solana DeFi. The LLM proposes; the control plane governs; the human (or, in one tightly bounded autonomous mode, a polling watcher within hard pre-set caps) signs. The signing boundary is structural: it lives in the Rust type system, not in a system prompt.

The pipeline is a compile-time typestate, in roughly this shape:

```
user intent
   │
   ▼
TransactionProposal ──(simulate)──► SimulatedTransaction
                                         │
                                  (policy evaluator)
                                         │
                                         ▼
                                  ApprovedTransaction ──(sign)──► SignedTransaction ──► RPC
```

Each arrow is a method on a specific Rust type. You cannot call `.sign()` on a `TransactionProposal` because the method does not exist on that type. The compiler rejects the program, not the runtime. The LLM-driven tool dispatcher holds a single capability: `ProposeSigning`. `SignTransaction` and `SendTransaction` are not in the LLM's capability set, ever. Audit is append-only by construction: `AuditRepository` has no `update` and no `delete` methods, and every propose, simulate, policy decision, approval, and signature is written before execution.

This matters because of Solana's 400-millisecond slots. A human cannot react to a rogue transaction once it's in flight. The only viable enforcement layer is pre-submission: typed errors, policy rejects, schema validation, and capability checks have to fire before the message hash is signed. Everything in this post is downstream of that constraint.

Full architecture map and ADRs are at [`ARCHITECTURE.md`](https://github.com/jeffreycheung521hk/Solfrontier2026/blob/t1-post-snapshot/ARCHITECTURE.md) in the public Solfrontier2026 repo.

## Defense Sequence 1 — Phase 5G deposit path (3 incidents)

The first sequence happened during the Phase 5G work in late April: getting a natural-language Solend USDC deposit to finalize end-to-end on mainnet for the first time. The success tx for that phase is [`4M4ezLgm…Py3y`](https://solscan.io/tx/4M4ezLgm1mFpGmUpLJdDAVhfXYwUxjS2ZMkjKprBiWzsfgNudPkhEvBr6GdJbh1zBscKLF6kpUBhZg7tAm3ePy3y), finalized at slot 415,571,964. Three incidents preceded it. The first and third were caught before any transaction existed; the second is an off-chain mempool-drop diagnostic that surfaced through our local confirmation tracker.

### D1: PDA seed length panic, caught pre-submission

**Symptom.** During Phase 4-blocked debugging, the builder's propose stage would have panicked inside `solana_program::pubkey::Pubkey::find_program_address` if the deposit message had ever reached the simulator. The function panics (it does not return a `Result`) when any seed exceeds `MAX_SEED_LEN` (32 bytes).

**Root cause.** The early Solend deposit instruction-builder was deriving a PDA from a 33-byte seed. The intended seed was a `LendingMarket` pubkey (32 bytes); a stray byte was being concatenated before the call. The combined slice violated Solana's structural seed-length invariant, and the SDK reaction to that is to panic rather than return a typed error.

**Fix.** Phase 4C-8F. The current shape of the code derives `lending_market_authority` directly from `lending_market.to_bytes()` (a clean 32-byte seed) at [`deposit.rs:167-170`](https://github.com/jeffreycheung521hk/Solfrontier2026/blob/t1-post-snapshot/crates/gateway/src/integrations/solend/deposit.rs#L167-L170) in the public repo. The 33-byte composition is gone. The fix landed squashed into a seal commit; the standalone delta is not separately addressable on the public branch, but the post-fix code is in tree.

**Lesson.** PDA seed length is a structural Solana invariant that you cannot defer to broadcast time. The SDK call panics rather than producing a `Result`. We caught this on a propose-stage simulate, before any approval object was created. If we had been deferring validation to a runtime check at signing time, the failure mode would have been a process crash, not a typed reject, and a control-plane that crashes mid-flow is a control-plane you cannot reason about. Validate seed lengths in the type system, or in the proposal builder, never later.

### D2: Zero-priority-fee mempool drop, off-chain confirmation-tracker catch

**Symptom.** During the Phase 4C-8G submit work, a deposit transaction was signed, accepted by the RPC, and assigned a valid signature. It then never landed on chain. `getSignatureStatuses` with `searchTransactionHistory: true` returned `null` across the full block-validity window. The transaction was simply absent from any confirmed block.

This one is the softest claim in the post and I want to be explicit about why: I do not have a Solscan-resolvable failed tx to link here. The signature is recorded in source only as a truncated comment, the transaction was never finalized so the indexer state is at best empty, and I'm classifying this as an off-chain confirmation-tracker observation, not an on-chain defense. It is a real failure mode the local tracker caught; it is not a thing you can verify by clicking a Solscan link.

**Root cause.** The Solend deposit tx plan did not include a `ComputeBudgetInstruction::set_compute_unit_price` instruction. Under mainnet leader congestion, the cluster's leader-queue logic dropped the transaction before block inclusion. There is no on-chain error to point at because the transaction was never included; the lifecycle simply transitioned to a non-success terminal through the local confirmation tracker observing the block-validity window elapse with no inclusion.

**Fix.** Phase 4C-8G-Fix added a priority fee of 50,000 micro-lamports per CU, prepended as the very first instruction of `setup_instructions` so it is part of the message hash and therefore covered by the signature. The constant lives at [`solend_tx_plan.rs:86-112`](https://github.com/jeffreycheung521hk/Solfrontier2026/blob/t1-post-snapshot/crates/gateway/src/integrations/solend_tx_plan.rs#L86-L112) as `SOLEND_DEPOSIT_PRIORITY_FEE_MICRO_LAMPORTS`, with an inline comment block above the declaration that cites the original drop, the leader-queue-contention hypothesis, and the design choice to put the priority-fee instruction inside the planned message (not at submit time) so the message hash covers it. It's consumed during plan assembly. The confirmation tracker now treats "valid signature, never landed" as a typed terminal failure, not a retry-with-backoff condition.

**Lesson.** A confirmation tracker that retries on "signature exists, never landed" is a double-submit risk waiting to happen. You have to give the missing-from-cluster path a typed terminal state of its own, distinct from "explicit revert" and from "still pending". On Solana, congestion-induced silent drops are an everyday operating condition, not an exception; the control plane has to model that explicitly.

### D3: Pyth USDC oracle staleness, rejected by policy evaluator

**Symptom.** During the Phase 4C-8G staleness-calibration work, a deposit proposal was rejected at the policy stage with a `RuleKind::MaxOracleStalenessMs` HardBlock. No approval was ever created. No signature was ever requested from a wallet.

**Root cause.** The live Pyth USDC publish age exceeded our configured stale-cutoff. The policy evaluator at `crates/gateway/src/lending/policy.rs` carries a closed enum of rule kinds (`MaxOracleStalenessMs` is rule line 302 in that file), and the dispatcher at lines 403–410 routes evaluation to a [deterministic rule function at lines 473–477](https://github.com/jeffreycheung521hk/Solfrontier2026/blob/t1-post-snapshot/crates/gateway/src/lending/policy.rs#L473-L477). The whole evaluator is plain Rust code, statically dispatched, with no path for the LLM to alter rule kinds or thresholds. A proposal that fails a HardBlock cannot become an `ApprovedTransaction`.

**Fix.** Phase 4C-8G-Staleness calibrated the `max_publish_age` threshold to 120,000 ms after measuring real publish-age distributions during normal cluster operation. The rule itself stayed structurally identical; only the threshold value moved, and it moved in a deployed config, not in an LLM context.

**Lesson.** Oracle staleness has to be a structural policy rule, not a tunable parameter the model can argue with. If `max_publish_age` is a number in a prompt, then prompt injection is a single-message attack surface against your most safety-critical gate. Make it a compile-time enum variant with a deterministic evaluator and a closed config file, and the LLM cannot loosen it no matter how creatively it's asked. Respect to the Pyth team here: the underlying confidence and publish-age primitives are the right primitives; the bug was on our side, in how we initially exposed the threshold.

## Defense Sequence 2 — Phase 6I-K withdraw processor parity (4 incidents)

The second sequence happened on a single afternoon: May 8, 2026, attempting to close the LLM-guided Solend withdraw-all round-trip against the deployed Solend mainnet processor at `So1endDq…CpAo`. Four consecutive on-chain reverts, each triggered by a different account-meta mismatch between our instruction builder and what the deployed processor actually reads. User funds were never debited on any of the four. The success tx that finally closed the round-trip is [`3tMpTSEn…EZPJZ`](https://solscan.io/tx/3tMpTSEnmvszjSwgow43KMd4dLqnZwwSvnTbHkcPsCiyfizLf2h7kcMc1M7LwokHLC3SBPzpTfykjsc48z8EZPJZ).

This sequence is the technical core of the post because it is the cleanest demonstration of one specific lesson: **for protocol-level instruction builders, the deployed program's `processor.rs` is the source of truth, and the published SDK helpers are advisory.** Each of the four incidents is a different shape of that same drift.

### D4: `RefreshObligation` `Custom(5) InvalidAccountOwner`

**Symptom.** The first attempt's instruction 2 (`RefreshObligation`) reverted with `LendingError::InvalidAccountOwner` = `Custom(5)`. Solend's program logs were specific: "Deposit reserve provided for collateral 0 is not owned by the lending program", followed by "Input account owner is not the program address". Failed tx: [`4YnQFUTm…Q7gnTv`](https://solscan.io/tx/4YnQFUTmPQEKpDoJRuUS6optFUs8pyxQuFknjFzRAUooWy88YjPQGeAFr9Ncv8JwZPV7xdp2cEjHm2Djk6Q7gnTv).

**Root cause.** Our `build_refresh_instructions` function was emitting `RefreshObligation` with an account list of `[obligation, sysvar::clock, ...referenced_reserves]`. The deployed Solend processor reads `Clock` via the `Clock::get()` syscall, internally, and does not consume it from the account list. Its actual layout for `RefreshObligation` is `[obligation, ...deposit_reserves, ...borrow_reserves]`. By inserting a Clock account at slot 1, we shifted every subsequent reserve by one position. Solend then dereferenced the wrong account as a reserve, found it was not owned by the lending program, and returned `Custom(5)`. The error message was about ownership, but the underlying defect was layout.

**Fix.** Commit `57f135f` `fix(solend): remove clock from refresh-obligation accounts` (Phase 6I-H). The account list became `[obligation, ...referenced_reserves]`, no Clock, no sysvar. Post-fix source: [`refresh.rs:178-215`](https://github.com/jeffreycheung521hk/Solfrontier2026/blob/t1-post-snapshot/crates/gateway/src/integrations/solend/refresh.rs#L178-L215) — the `if let Some(o) = obligation` block, with an inline Phase 6I-H comment block that cites the same failed tx `4YnQFUTm…` and explains the layout drift in code-resident form so a future contributor cannot reintroduce the Clock slot without first deleting the comment.

**Lesson.** A Solana program that reads `Clock::get()` syscall internally does not want Clock in its account list. Including it shifts every subsequent slot and produces an error message that looks unrelated to the actual defect. When you see an account-ownership error on the second instruction of a multi-instruction tx, the first thing to check is whether the first instruction's account list is one position longer than the processor expects.

### D5: Withdraw `NotEnoughAccountKeys`

**Symptom.** With `RefreshObligation` corrected, the next attempt's instruction 3 (`WithdrawObligationCollateralAndRedeemReserveCollateral`) reverted with `InstructionError(3, NotEnoughAccountKeys)`. Failed tx: [`25REcnNp…zmT3Hp`](https://solscan.io/tx/25REcnNpUTbqx5eLV2L7LcN39vEMP7RZUZ9sNFZcURrBr2S6B7LJVcxzAcRNhtbs9znEswb8Gtob8Bmrk5zmT3Hp).

**Root cause.** Our withdraw-ix builder was emitting a 12-account layout: `[0..10] base accounts, [11] SPL Token program`. That matched the SDK helper's named slots. What it did not match was the deployed processor's actual behavior. Inside `_redeem_reserve_collateral`, the processor calls `update_borrow_attribution_values(&mut obligation, deposit_reserve_infos)`, which iterates `obligation.deposits` and consumes one writable account per deposit via `next_account_info()` starting at slot 12 and counting up. With zero accounts past slot 11, the very first `next_account_info()` call returned `NotEnoughAccountKeys`. The SDK helper's account list ended where the processor was just getting started.

**Fix attempt 1 (wrong).** Commit `d7d960d` `fix(solend): add Clock at slot 11 of withdraw ix (mainnet 13-account layout)` (Phase 6I-I) added a `sysvar::clock` at slot 11. The SDK's published `token-lending/sdk/src/instruction.rs` documents a 13-account layout with Clock at slot 11, so this looked correct against the helper. It was not correct against the processor. It surfaced D6 immediately.

**Lesson.** When a Solana program iterates a variable-length list of accounts (`next_account_info` in a loop), the SDK helper's named slots are necessary but not sufficient. You must append the dynamic tail in the exact order the processor expects, and the only ground truth for that order is the processor itself. The SDK helpers list the *known* slots; they tend not to list the *iterated* ones. Post-fix source: [`withdraw.rs:297-304`](https://github.com/jeffreycheung521hk/Solfrontier2026/blob/t1-post-snapshot/crates/gateway/src/integrations/solend/withdraw.rs#L297-L304) — the `for r in &inputs.deposit_reserves_in_order { accounts.push(...) }` loop that appends one writable account per obligation deposit at slots 12+, landed in commit `e4195fa` (Phase 6I-J, see D6 below).

### D6: Withdraw `Custom(9) InvalidTokenProgram`

**Symptom.** The Phase 6I-I attempt's instruction 3 reverted with `LendingError::InvalidTokenProgram` = `Custom(9)`. Solend logged: "Lending market token program does not match the token program provided". Failed tx: [`2mLa5bQv…PuCpfY`](https://solscan.io/tx/2mLa5bQvTN5daWkC5ehiDDbQvGpyyTZ8qkphs9jUGx8b191Xq54XH1HxVWETr67FYJay6oc3NPnfqAo9tFPuCpfY).

**Root cause.** The previous mis-fix put `sysvar::clock` at slot 11 because the SDK helper documented a 13-account layout with Clock at slot 11. Reading the deployed `processor.rs` carefully, slot 11 is `token_program_id` (the SPL Token program), read immediately after the `user_transfer_authority` signer at slot 10. The processor never reads Clock from the account list at all; it uses the syscall, same as in `RefreshObligation`. So when the program dereferenced slot 11 expecting `Tokenkeg…VQ5DA` and instead found `SysvarC1ock…`, it rejected with `Custom(9)`. The SDK helper documentation and the deployed processor disagreed on what slot 11 is.

**Fix.** Commit `e4195fa` `fix(solend): append withdraw deposit reserves after token program` (Phase 6I-J). The corrected layout is `[0..10] base accounts, [11] SPL Token program (readonly), [12..N+11] one writable account per `obligation.deposits` entry, in obligation-deposit order`. No Clock anywhere in the account list. Post-fix source: [`withdraw.rs:267-296`](https://github.com/jeffreycheung521hk/Solfrontier2026/blob/t1-post-snapshot/crates/gateway/src/integrations/solend/withdraw.rs#L267-L296) — the base account list with the Phase 6I-J comment block above it explicitly noting that the Phase 6I-I attempt to add Clock at slot 11 was wrong and pointing to the module doc-comment above where both live failures are named so a future contributor cannot reintroduce the Clock slot without first deleting the rejection record. The commit body of `e4195fa` itself contains a slot-by-slot parity table cross-referencing both `token-lending/program/src/processor.rs` and `token-lending/sdk/src/instruction.rs` so the divergence is visible at a glance.

**Lesson.** When SDK helper and deployed processor disagree on account layout, the deployed processor wins, every time. The helper docs can be stale or aspirational; the deployed code is what executes. Audit the canonical-client constructor (`new_with_meta`-style helpers) and the on-chain handler side by side, and when they conflict, build for the handler. The Solend team is not negligent here: published SDK code that lags a deployed processor is the norm across the Solana ecosystem when a protocol is iterated faster than its public client. Treating that as expected behavior is part of mainnet hygiene.

### D7: Withdraw `ReadonlyDataModified`

**Symptom.** With the dynamic deposit-reserves tail appended correctly, the next attempt's instruction 3 still reverted, this time with `ReadonlyDataModified` on instruction 3 Withdraw. I have to be honest about the evidence here: the failed-tx signature for this attempt was truncated in our internal record (it appears in three places in the repo as `5W3PzXnR…NMNRVg`, never with a full base58 expansion), and I attempted to recover it from local logs before publishing this post and could not. So I cannot link a Solscan tx for D7. The root cause and fix are both well-supported by commit `8d59697` and by the post-fix source at [`crates/gateway/src/integrations/solend/withdraw.rs:283-289`](https://github.com/jeffreycheung521hk/Solfrontier2026/blob/t1-post-snapshot/crates/gateway/src/integrations/solend/withdraw.rs#L283-L289), but the on-chain anchor I'd normally point you to is missing for this one.

**Root cause.** Our builder was passing slot 4 (`lending_market`) as `new_readonly`. The deployed processor's `_redeem_reserve_collateral` function calls `LendingMarket::pack(lending_market, lending_market_info.data.borrow_mut())?` to perform rate-limiter and accumulated-fees writes during a withdraw, meaning slot 4 must be writable, because the program intends to mutate it. The SDK helper docs did not flag this slot as writable. Readonly metadata combined with a write attempt produces `ReadonlyDataModified` at the runtime level.

**Fix.** Commit `8d59697` `fix(solend): align withdraw metas with mainnet processor` (Phase 6I-K). Slot 4's `AccountMeta` was changed from `new_readonly(inputs.lending_market, false)` to `AccountMeta::new(inputs.lending_market, false)`: same pubkey, writable flag flipped. The post-fix source at [`withdraw.rs:283-289`](https://github.com/jeffreycheung521hk/Solfrontier2026/blob/t1-post-snapshot/crates/gateway/src/integrations/solend/withdraw.rs#L283-L289) carries an inline comment block explaining the prior failure mode immediately above the now-writable `AccountMeta::new(...)` call so a future contributor doesn't undo the fix while "cleaning up". The commit body itself contains a full Solend SDK vs ClawSolana builder parity table for both `WithdrawObligationCollateralAndRedeemReserveCollateral` and `RefreshObligation`.

**Lesson.** `ReadonlyDataModified` is a protocol-level signal: a program is writing to an account you marked read-only. Always cross-check writable flags against the processor's `*::pack(..., data.borrow_mut())` calls, not against SDK helper comments. The `borrow_mut()` calls are the ground truth for which accounts the program will attempt to mutate; if a helper marks one of those slots read-only, the helper is wrong, not the processor.

### Withdraw account-ordering table, end-to-end

To make the drift concrete, here is the slot-by-slot evolution of our `WithdrawObligationCollateralAndRedeemReserveCollateral` account list across all four iterations, against what the deployed Solend processor actually reads:

| Slot | Account | Pre-6I-H | 6I-H | 6I-I | 6I-J | 6I-K (mainnet-verified) | Solend `processor.rs` source-of-truth |
|---|---|---|---|---|---|---|---|
| 0 | `source_collateral` | writable | writable | writable | writable | writable | writable |
| 1 | `destination_collateral` | writable | writable | writable | writable | writable | writable |
| 2 | `withdraw_reserve` | writable | writable | writable | writable | writable | writable |
| 3 | `obligation` | writable | writable | writable | writable | writable | writable |
| 4 | `lending_market` | **readonly** | readonly | readonly | readonly | **writable (D7 fix)** | **writable** |
| 5 | `lending_market_authority` | readonly | readonly | readonly | readonly | readonly | readonly |
| 6 | `destination_liquidity` | writable | writable | writable | writable | writable | writable |
| 7 | `reserve_collateral_mint` | writable | writable | writable | writable | writable | writable |
| 8 | `reserve_liquidity_supply` | writable | writable | writable | writable | writable | writable |
| 9 | `obligation_owner` | signer | signer | signer | signer | signer | signer |
| 10 | `user_transfer_authority` | signer | signer | signer | signer | signer | signer |
| 11 | (slot 11) | **SPL Token, no slot 12+** → D5 | SPL Token, no slot 12+ → D5 | **`sysvar::clock`** → D6 | SPL Token readonly | SPL Token readonly | SPL Token readonly |
| 12..N+11 | per-deposit `deposit_reserves` (writable each, in `obligation.deposits` order) | **absent** → D5 | absent → D5 | absent | **present (D5 real fix)** | present | present (per `update_borrow_attribution_values`) |

For `RefreshObligation` the drift was simpler: pre-6I-H had `sysvar::clock` at slot 1, which shifted reserves by one and triggered D4; 6I-H removed Clock and the remaining slots aligned cleanly with the processor's source of truth, since the processor reads Clock via syscall, not from the account list.

**processor.rs is the source of truth. SDK helpers are advisory.**

That sentence is the entire point of this section. It cost me four reverted on-chain attempts in a single afternoon to internalize it, and I'm publishing the receipts so the next solo builder doesn't have to pay those four per-tx fees to learn the same thing.

## Five generalized lessons

After 60 days, these are the five takeaways I'd hand to my Phase 4 self if I could.

**1. `processor.rs` outranks SDK docs, every time.** When a deployed program's `processor.rs` and its published SDK helper disagree on account layout, account writability, or signer flags, the processor is what executes and the SDK is what got written-once-and-drifted. This is not a moral failure of the SDK author; it's a structural consequence of protocols being iterated faster than their public clients. The fix is to read the on-chain handler before you trust the helper, and to keep a slot-by-slot parity table in your repo for any non-trivial instruction.

**2. Pre-submission enforcement is the only viable layer on 400 ms slots.** A human cannot react to a rogue transaction in 400 milliseconds. Neither can most monitoring stacks. If your control points are downstream of `sendTransaction`, you have already lost the race. That's why the typestate pipeline, the closed schema, the capability checks, and the policy evaluator all fire before any signature is requested. Anything that can wait until "after broadcast" might as well wait until "after settlement", which is too late.

**3. Compile-time safety beats runtime check beats prompt-engineered guardrail.** The strongest invariant is one the compiler refuses to compile. Next strongest is one the runtime returns a typed error for. Weakest is one the system prompt asks the model nicely to honor. ClawSolana's signing boundary lives in the type system on purpose: `SignTransaction` and `SendTransaction` are not capabilities the LLM-driven tool dispatcher carries, and no amount of prompt creativity gives it those capabilities. If your safety story depends on the model behaving, you don't have a safety story; you have a hope.

**4. A correctly-rejected failure is more instructive than a one-time success.** All four Phase 6I-K reverts taught more about Solend's actual on-chain semantics than the Phase 5G success had. Each gave a specific, attributable error code, a specific line of the processor to read, and a specific shape of fix. The success transaction at the end is necessary; it is not where the learning lives. Build something whose failure modes are loud and typed, and the failures become your primary documentation.

**5. Honest scope is a feature, not a hedge.** This is hackathon scope. It is not production-deployed. It has not been audited by a third party. The mainnet proofs were run with controlled test wallets and per-transaction caps in the harness; pointing this at a real treasury today would be irresponsible, and I say so in the project README. Naming the boundary explicitly is the only thing that lets the parts inside the boundary be taken seriously.

## What I'd do differently next time

If I were starting Phase 4 again on May 28, I would change three things.

I would write the Solend integration tests against the deployed mainnet program ID, sandboxed through a fork or a careful devnet mirror, before any mainnet tx, not against localnet builds of the SDK. The four Phase 6I-K reverts cost me real per-tx fees in production because my localnet harness was satisfied by SDK-helper layouts that the deployed processor rejected. A test layer that talks to the actual on-chain program would have caught D4 through D7 in CI, not on mainnet.

I would derive my instruction-builder constraints from the deployed `processor.rs` source, not from the SDK's `instruction.rs` helpers, from day one. The parity table I now keep in the repo for Solend withdraw (slot, account, writable flag, signer flag, processor evidence line) should be the artifact you produce *first* for any non-trivial third-party instruction, not the artifact you produce after four reverts have made it expensive to skip.

The open question I'd put to other Solana builders, especially anyone who's tangled with Solend, Save, Kamino, or other forks of `solana-program-library` token-lending: how do we standardize "processor parity" as a publishable test layer? Right now every team that integrates with a major lending or AMM protocol reverse-engineers the same parity table privately and pays for the lessons separately. A shared, machine-readable layout spec, generated from the deployed processor and round-tripped through a real test harness, would save the next wave of solo builders the same four per-tx fees I paid in May.

## Closing

I came into this from outside the Solana builder world. I entered crypto in 2022 after FTX collapsed, as a retail trader on Binance, Solflare, Orca, Raydium, the same path a lot of people walked. I did a couple of Chainlink hackathons earlier on guard-rails-for-autonomous-systems themes; this is my first Solana hackathon and I had no warm intros to Helius, Jito, Squads, or the Foundation going in. Sixty days, around 175 commits, seven mainnet-finalized success transactions, seven fail-closed incidents, zero user funds lost. This post is the long version of what those seven incidents taught me.

What I'm looking for from publishing this: other Solana builders who have hit similar processor-parity issues against Solend, Save, Kamino, Marinade-style or Jupiter-side integrations, and would compare notes. I'm not pitching for funding here and I'm not pitching for a job here. I'm trying to find the small handful of people who've debugged the same shape of problem and would rather not each pay the per-tx fees alone.

The Frontier 2026 submission is in judging freeze until 2026-06-23, so the original repo with the full weekly engineering log and proof index is read-only until then.

Contact: `[X handle TBD]`. Public repo with the slimmed phase5c-lite redesign, including [`ARCHITECTURE.md`](https://github.com/jeffreycheung521hk/Solfrontier2026/blob/t1-post-snapshot/ARCHITECTURE.md) and [`ROADMAP.md`](https://github.com/jeffreycheung521hk/Solfrontier2026/blob/t1-post-snapshot/ROADMAP.md): `github.com/jeffreycheung521hk/Solfrontier2026`. The original frozen Frontier submission, with the full weekly engineering log and proof index, lives at `github.com/jeffreycheung521hk/testingcrypto2`.

---

## Title alternatives for Jeff to choose from

1. **Seven fail-closed lessons on Solana mainnet — what 60 days shipping solo taught me about Solend processor parity** *(locked working title; Jeff-approved; broadest narrative scope, mentions both the 7-incident frame and the SEO core "Solend processor parity".)*

2. **`processor.rs` is the source of truth: seven Solana mainnet lessons from a solo Solend integration** *(tighter, SEO-forward on the central technical claim; trades the 60-days-solo personal frame for a punchier code-first hook.)*

3. **Seven fail-closed incidents from sixty days of solo Solend integration on Solana mainnet** *(most conservative; drops "lessons" and "processor parity" from the title to keep the headline factual; loses the SEO core phrase but is the least likely to be read as an overclaim by a skeptical Solend or Solana engineer skimming the title.)*
