# T1 Evidence Pack — 7 fail-closed defenses + success tx baseline

Generated 2026-05-28 by Agent 1 (Evidence Miner). Working file only; not in docs/ freeze scope. Source of truth for Agent 2's draft.

## Repo + branch context

- Local branch: `integrate-phase5c-lite` (HEAD `5c37e1a`)
- Public target repo: https://github.com/jeffreycheung521hk/Solfrontier2026 (slimmed phase5c-lite redesign; intended to ship without `docs/proofs/` and without `docs/WEEKLY_UPDATES.md`)
- Archive / hackathon-submission repo: https://github.com/jeffreycheung521hk/testingcrypto2 (judging-frozen at `44a807d` until 2026-06-23)
- Local remotes (verified via `git remote -v`):
  - `origin` → `testingcrypto2`
  - `target` → `Solfrontier1` (sealed at `7bc42da`)
  - `staging2026` → `Solfrontier2026`
- **CONFLICT — see "Conflicts / open questions" §** the current state of `staging2026/main` and `staging2026/integrate-phase5c-lite` (verified 2026-05-28 via `git ls-tree`) **still contains** `docs/proofs/` and `docs/WEEKLY_UPDATES.md`, and does **not yet contain** `SECURITY_BOUNDARIES.md`, `DEMO_EVIDENCE.md`, `PHASE5C_LITE_MAINNET_PROOF.md`, or `REVIEWER_QUICK_PATH.md`. Either the slim is not yet pushed, or the redesign docs haven't landed yet. Agent 2 should treat the "Solfrontier2026 has X but not Y" claim as the **future intended state**, not the current observable state.

## Success tx baseline

All four success-tx ellipsis-truncated forms in memory expand to full base58 strings observed in the local repo. Sources are docs in the current working tree.

| Phase | Full tx signature | Slot | Source (local file:line) | Verification status |
|---|---|---|---|---|
| Phase 4C (non-LLM Solend baseline, 2026-04-25) | `2QqSfDq53a34WDXVkvZeyW59eFJcxZtkQtjZw25CoUizEmzDjahusKvBT8eNb4XdpKYwrgGcTyBwYXgth5wpxALs` | 415,475,589 | `docs/proofs/INDEX.md:20`; `docs/proofs/PHASE5_CLOSEOUT.md:22` | verified against repo |
| Phase 5G (first LLM-guided mainnet deposit, 2026-04-25) | `4M4ezLgm1mFpGmUpLJdDAVhfXYwUxjS2ZMkjKprBiWzsfgNudPkhEvBr6GdJbh1zBscKLF6kpUBhZg7tAm3ePy3y` | 415,571,964 | `docs/proofs/LLM_SOLEND_MAINNET_E2E_PHASE5G.md:9`; `docs/proofs/INDEX.md:18` | verified against repo |
| Phase 6I-K (LLM-guided withdraw-all, 2026-05-08) | `3tMpTSEnmvszjSwgow43KMd4dLqnZwwSvnTbHkcPsCiyfizLf2h7kcMc1M7LwokHLC3SBPzpTfykjsc48z8EZPJZ` | 418,395,346 | `docs/proofs/LLM_SOLEND_WITHDRAW_MAINNET_E2E_PHASE6I.md:33`; `docs/proofs/INDEX.md:16` | verified against repo |
| W5i (first autonomous Solend deposit, 2026-05-12) | `takmz89b5hLHgJ9jHVBbqJogxdvu8xuMdUgZLEpdHhTD7zvRswrrk1UnxfZf7pbsaM4fygqvDFwFbph5YH2jTv5` | 419,200,198 | `docs/WEEKLY_UPDATES.md:752`; `README.md:17` | verified against repo |

Solscan links (canonical):
- Phase 4C: https://solscan.io/tx/2QqSfDq53a34WDXVkvZeyW59eFJcxZtkQtjZw25CoUizEmzDjahusKvBT8eNb4XdpKYwrgGcTyBwYXgth5wpxALs
- Phase 5G: https://solscan.io/tx/4M4ezLgm1mFpGmUpLJdDAVhfXYwUxjS2ZMkjKprBiWzsfgNudPkhEvBr6GdJbh1zBscKLF6kpUBhZg7tAm3ePy3y
- Phase 6I-K: https://solscan.io/tx/3tMpTSEnmvszjSwgow43KMd4dLqnZwwSvnTbHkcPsCiyfizLf2h7kcMc1M7LwokHLC3SBPzpTfykjsc48z8EZPJZ
- W5i: https://solscan.io/tx/takmz89b5hLHgJ9jHVBbqJogxdvu8xuMdUgZLEpdHhTD7zvRswrrk1UnxfZf7pbsaM4fygqvDFwFbph5YH2jTv5

## Defense Sequence 1 — Phase 5G Deposit (3 defenses, pre-submission catches)

All three defenses are described in `docs/proofs/PHASE5_CLOSEOUT.md` §5 ("Fail-Closed Defense Record", lines 82–90) and summarised in `README.md` lines 82–86 ("Sequence 1 — Phase 5G deposit path"). None has a single dedicated failed-tx Solscan link in the repo — these were caught *before* unsafe action.

### D1 — find_program_address panic from 33-byte PDA seed

- **Phase:** Phase 4-blocked (root cause); fix in Phase 4C-8F; re-proven in 4C-8G/5G
- **Tx:** No tx, caught pre-submission. Propagation would have panicked `find_program_address` on a seed > `MAX_SEED_LEN` (32 bytes) *before* any approval was created and before any RPC submit.
- **Solscan:** N/A
- **Slot:** N/A
- **Error code / rejection reason:** Panic in `solana_program::pubkey::Pubkey::find_program_address` — seed bytes longer than 32 violate `MAX_SEED_LEN`; the function panics rather than returning a typed error.
- **Root cause:** Original propose-stage code derived a PDA using a 33-byte seed (memory: `project_phase4_blocked_seed_bug` cites `solend_deposit.rs:119` as the historical site). The fixed-current builder derives `lending_market_authority` with a 32-byte seed (`lending_market.to_bytes()`) — see `crates/gateway/src/integrations/solend/deposit.rs:167-170` (deposit) and `crates/gateway/src/integrations/solend/withdraw.rs:253-256` (withdraw). The historical 33-byte seed is no longer present in the tree (squashed under Phase 4C-8F).
- **Fix:** Phase 4C-8F (squashed into seal commit `7bc42da` `feat(gateway): end-to-end Solend V1 USDC deposit path with live-mainnet proof`); not findable as a standalone commit. See `docs/proofs/PHASE5_CLOSEOUT.md:86`. Current correct code: `crates/gateway/src/integrations/solend/deposit.rs:167-170`.
- **Lesson (1 line):** PDA seed length is a structural Solana invariant (`MAX_SEED_LEN=32`) — validate at propose time, not at broadcast time, because the SDK panics rather than returning an error.
- **Publish-safety label:** **safe to publish.** No tx → no Solscan link required. The lesson is real, the closeout doc backs it, the current code shows the fix shape.

### D2 — Zero-priority-fee tx dropped from cluster mempool

- **Phase:** Phase 4C-8G submit attempt; calibrated in 4C-8G-Fix
- **Tx:** No public Solscan-resolvable tx in the proof docs. The signature `3PUtp…21QT` is mentioned **only as truncated** at `crates/gateway/src/integrations/solend_tx_plan.rs:89` ("first 4C-8G live mainnet attempt produced a valid signature (`3PUtp…21QT`) that was accepted by the RPC but never landed on chain"). It is RPC-accepted but never finalized — `getSignatureStatuses(searchTransactionHistory: true)` returned `null` across the full block-validity window. **Solscan may or may not resolve a never-landed signature** depending on indexer policy.
- **Solscan:** N/A (signature truncated in repo; not expanded)
- **Slot:** N/A (never landed)
- **Error code / rejection reason:** Off-chain — tx dropped from leader-queue due to zero priority fee during mainnet congestion. No on-chain error code; lifecycle transitioned to a non-success terminal via `getSignatureStatuses` returning `null`.
- **Root cause:** Solend deposit plan did not include a `ComputeBudgetInstruction::set_compute_unit_price`; under cluster congestion the leader dropped the transaction before block inclusion. Cited at `crates/gateway/src/integrations/solend_tx_plan.rs:86-112`.
- **Fix:** Phase 4C-8G-Fix added priority fee = **50,000 µ-lamports per CU**, prepended as the FIRST instruction of `setup_instructions` so it is covered by the message-hash signature (`crates/gateway/src/integrations/solend_tx_plan.rs:112` constant `SOLEND_DEPOSIT_PRIORITY_FEE_MICRO_LAMPORTS = 50_000`, used at `:334`). Tests at `:1230`, `:1235`, `:1266`, `:1296`, `:1319`. Like D1, the fix is squashed into the `7bc42da` seal.
- **Lesson (1 line):** No priority fee + congestion = silent drop, no on-chain error — your confirmation tracker must treat "valid signature, never landed" as a typed terminal failure, not a retry-with-backoff condition.
- **Publish-safety label:** **needs softening.** The truncated tx `3PUtp…21QT` is in source as a comment, not a clickable Solscan link. Recommend: describe the defense generically with the lesson, omit the truncated signature, OR get Jeff to expand it from local logs before publish.

### D3 — Pyth USDC oracle staleness — rejected by lending policy evaluator

- **Phase:** Phase 4C-8G-Staleness Calibration
- **Tx:** No tx, caught pre-submission. The proposal was rejected at the policy stage **before** any approval was created (`docs/proofs/PHASE5_CLOSEOUT.md:88`).
- **Solscan:** N/A
- **Slot:** N/A
- **Error code / rejection reason:** Off-chain policy reject — `RuleKind::MaxOracleStalenessMs` HardBlock fired in the lending policy evaluator because the Pyth USDC oracle's slot age exceeded the configured `max_publish_age` threshold.
- **Root cause:** Live Pyth USDC publish age exceeded the configured stale-cutoff. Cited at `crates/gateway/src/lending/policy.rs:302` (rule enum), `:404-407` (evaluator dispatch), `:473-476` (the rule fn). Calibration commits squashed into `7bc42da` and post-seal.
- **Fix:** Phase 4C-8G-Staleness calibrated `max_publish_age` to 120,000 ms (cited in `docs/proofs/PHASE5_CLOSEOUT.md:88`). The policy is **code, not LLM-controlled** — the assistant cannot argue out of it.
- **Lesson (1 line):** Oracle staleness must be a structural policy rule (compile-time enum, deterministic evaluator), not an LLM-tunable parameter — otherwise prompt injection trivially loosens it.
- **Publish-safety label:** **safe to publish.** Pre-submission catch; doc-backed; policy code is in-tree and stable.

## Defense Sequence 2 — Phase 6I-K Withdraw Processor Parity (4 defenses, each reverted on-chain)

All four are documented at `docs/proofs/LLM_SOLEND_WITHDRAW_MAINNET_E2E_PHASE6I.md:113-118` (4-row table) and `docs/WEEKLY_UPDATES.md:246-269` (numbered list with per-incident detail). All four occurred on 2026-05-08 against the deployed Solend mainnet processor `So1endDq…CpAo`. **User funds were never debited; only the per-tx fee was paid on each failed attempt.**

### D4 — Custom(5) InvalidAccountOwner on RefreshObligation

- **Phase:** Phase 6I-H
- **Tx:** `4YnQFUTmPQEKpDoJRuUS6optFUs8pyxQuFknjFzRAUooWy88YjPQGeAFr9Ncv8JwZPV7xdp2cEjHm2Djk6Q7gnTv` (instruction 2 reverted)
- **Solscan:** https://solscan.io/tx/4YnQFUTmPQEKpDoJRuUS6optFUs8pyxQuFknjFzRAUooWy88YjPQGeAFr9Ncv8JwZPV7xdp2cEjHm2Djk6Q7gnTv
- **Slot:** Not recorded in repo (failed tx, slot not load-bearing — Solscan resolves the slot if needed)
- **Error code:** `LendingError::InvalidAccountOwner` = `Custom(5)` on instruction 2 `RefreshObligation`. Solend logged: `"Deposit reserve provided for collateral 0 is not owned by the lending program"` and `"Input account owner is not the program address"` (cited verbatim in commit `57f135f`).
- **Root cause:** `build_refresh_instructions` in `crates/gateway/src/integrations/solend/refresh.rs` emitted `RefreshObligation` with `[obligation, sysvar::clock, ...referenced_reserves]`. The deployed Solend processor reads `Clock` via `Clock::get()` syscall, NOT as an account, and consumes accounts as `[obligation, ...deposit_reserves, ...borrow_reserves]`. The extra Clock at slot 1 shifted the reserve list by one; Solend then dereferenced the wrong account as a reserve and got `Custom(5)`.
- **Fix:** Commit `57f135f` `fix(solend): remove clock from refresh-obligation accounts` (Phase 6I-H). Layout corrected to `[obligation, ...referenced_reserves]` — no Clock account.
- **Lesson (1 line):** A Solana program that reads `Clock::get()` syscall internally does **not** want Clock in its account list — including it shifts every subsequent slot and produces an account-ownership error that looks unrelated.
- **Publish-safety label:** **safe to publish.** Full base58 tx, Solscan-resolvable, commit hash and code path both confirmed in repo.

### D5 — NotEnoughAccountKeys on Withdraw

- **Phase:** Phase 6I-I (note: 6I-I's specific code change — Clock-at-slot-11 — was itself wrong; the *correct* fix landed in 6I-J)
- **Tx:** `25REcnNpUTbqx5eLV2L7LcN39vEMP7RZUZ9sNFZcURrBr2S6B7LJVcxzAcRNhtbs9znEswb8Gtob8Bmrk5zmT3Hp` (instruction 3 reverted)
- **Solscan:** https://solscan.io/tx/25REcnNpUTbqx5eLV2L7LcN39vEMP7RZUZ9sNFZcURrBr2S6B7LJVcxzAcRNhtbs9znEswb8Gtob8Bmrk5zmT3Hp
- **Slot:** Not recorded in repo
- **Error code:** `InstructionError(3, NotEnoughAccountKeys)` on instruction 3 `WithdrawObligationCollateralAndRedeemReserveCollateral`.
- **Root cause:** Withdraw ix was built with a 12-account layout `[0..10] base, [11] spl_token_program` only. The deployed Solend handler calls `update_borrow_attribution_values(&mut obligation, deposit_reserve_infos)`, which iterates `obligation.deposits` and consumes one writable account per deposit via `next_account_info()` starting at slot 12+. With zero accounts past slot 11, the first `next_account_info()` call returned `NotEnoughAccountKeys`. (Detail cited verbatim in commit `e4195fa`.)
- **Fix:** *Initial* mis-fix at commit `d7d960d` `fix(solend): add Clock at slot 11 of withdraw ix (mainnet 13-account layout)` (Phase 6I-I) — this was wrong and broke D6. *Real fix* lives in commit `e4195fa` (Phase 6I-J), see D6.
- **Lesson (1 line):** When a Solana program iterates a variable-length list of accounts (`next_account_info` in a loop), the SDK helper's named slots are not enough — you must append the dynamic tail in the exact order the processor expects.
- **Publish-safety label:** **safe to publish.** Full base58 tx, Solscan-resolvable. Be sure to frame 6I-I as "the initial fix attempt was itself wrong, which surfaced D6" — that nuance is the whole point of the lesson.

### D6 — Custom(9) InvalidTokenProgram (slot 11 collision)

- **Phase:** Phase 6I-J
- **Tx:** `2mLa5bQvTN5daWkC5ehiDDbQvGpyyTZ8qkphs9jUGx8b191Xq54XH1HxVWETr67FYJay6oc3NPnfqAo9tFPuCpfY` (instruction 3 reverted)
- **Solscan:** https://solscan.io/tx/2mLa5bQvTN5daWkC5ehiDDbQvGpyyTZ8qkphs9jUGx8b191Xq54XH1HxVWETr67FYJay6oc3NPnfqAo9tFPuCpfY
- **Slot:** Not recorded in repo
- **Error code:** `LendingError::InvalidTokenProgram` = `Custom(9)` on instruction 3 Withdraw. Solend logged: `"Lending market token program does not match the token program provided"`.
- **Root cause:** The Phase 6I-I mis-fix put `sysvar::clock` at slot 11 (because `token-lending/sdk/src/instruction.rs` *documents* a 13-account layout with Clock at slot 11). The deployed mainnet `processor.rs` reads `token_program_id` immediately after `user_transfer_authority` at slot 11, not Clock — so the program found `sysvar::clock` where it expected `Tokenkeg…VQ5DA` and rejected with `Custom(9)`.
- **Fix:** Commit `e4195fa` `fix(solend): append withdraw deposit reserves after token program` (Phase 6I-J). Layout: `[0..10] base, [11] SPL Token program readonly, [12..N+11] one writable account per obligation.deposits entry in obligation-deposit order` — no Clock anywhere.
- **Lesson (1 line):** When SDK helper and deployed processor disagree, the deployed `processor.rs` wins — the helper docs are stale or aspirational. Audit the canonical-client constructor AND the on-chain handler side by side.
- **Publish-safety label:** **safe to publish.** Full base58 tx, Solscan-resolvable, commit `e4195fa` body cites both source files explicitly.

### D7 — ReadonlyDataModified (slot 4 lending_market writability)

- **Phase:** Phase 6I-K
- **Tx:** **`5W3PzXnR…NMNRVg` — truncated only; full base58 not present in repo.** Mentioned at `docs/proofs/LLM_SOLEND_WITHDRAW_MAINNET_E2E_PHASE6I.md:118`, `docs/WEEKLY_UPDATES.md:264`, and `crates/gateway/src/integrations/solend/withdraw.rs:288` — every reference is the truncated form.
- **Solscan:** N/A (cannot construct without full sig — Solscan needs the full base58 string)
- **Slot:** Not recorded in repo
- **Error code:** `ReadonlyDataModified` on instruction 3 Withdraw.
- **Root cause:** Slot 4 `lending_market` was passed as `new_readonly` in the ClawSol builder. The deployed processor's `_redeem_reserve_collateral` calls `LendingMarket::pack(lending_market, lending_market_info.data.borrow_mut())?` to perform rate-limiter / accumulated-fees writes — i.e., it writes to `LendingMarket`. Readonly metadata + write attempt = `ReadonlyDataModified`. The SDK helper docs did **not** flag this slot as writable. Code cite: `crates/gateway/src/integrations/solend/withdraw.rs:283-289` (current correct code with `AccountMeta::new(inputs.lending_market, false)` on line 289 + comment explaining the prior failure on lines 283-288).
- **Fix:** Commit `8d59697` `fix(solend): align withdraw metas with mainnet processor` (Phase 6I-K). Slot 4 changed from `new_readonly` to `new` (writable). Commit body contains a full Solend SDK vs ClawSol builder parity table for both `WithdrawObligationCollateralAndRedeemReserveCollateral` and `RefreshObligation`.
- **Lesson (1 line):** `ReadonlyDataModified` is a protocol-level signal: a program is writing to an account you marked read-only. Always cross-check writable flags against the processor's `*::pack(..., data.borrow_mut())` calls, not against SDK helper comments.
- **Publish-safety label:** **needs softening.** No Solscan link can be constructed. Either Jeff expands `5W3PzXnR…NMNRVg` from local logs (`data/clawd-phase6i-k-live.log`, cited in proof doc), or the post describes D7 without a clickable failed-tx link and points readers to commit `8d59697` for the audit table.

## Withdraw account-ordering table (Phase 6I-H through 6I-K)

Source: commit body of `8d59697` (Phase 6I-K parity audit) + `docs/proofs/LLM_SOLEND_WITHDRAW_MAINNET_E2E_PHASE6I.md:120-126` (post-6I-K layout). This table shows the slot-by-slot withdraw-ix account order as it evolved through each fix.

`WithdrawObligationCollateralAndRedeemReserveCollateral` (Solend instruction tag 15):

| Slot | Account | Pre-6I-H builder | 6I-H builder | 6I-I builder | 6I-J builder | 6I-K builder (mainnet-verified) | Solend processor.rs source-of-truth |
|------|---------|-------|-------|-------|-------|-------|---|
| 0 | source_collateral | writable | writable | writable | writable | writable | writable |
| 1 | destination_collateral | writable | writable | writable | writable | writable | writable |
| 2 | withdraw_reserve | writable | writable | writable | writable | writable | writable |
| 3 | obligation | writable | writable | writable | writable | writable | writable |
| 4 | lending_market | **readonly** | readonly | readonly | readonly | **writable (D7 fix)** | **writable** |
| 5 | lending_market_authority | readonly | readonly | readonly | readonly | readonly | readonly |
| 6 | destination_liquidity | writable | writable | writable | writable | writable | writable |
| 7 | reserve_collateral_mint | writable | writable | writable | writable | writable | writable |
| 8 | reserve_liquidity_supply | writable | writable | writable | writable | writable | writable |
| 9 | obligation_owner | signer | signer | signer | signer | signer | signer |
| 10 | user_transfer_authority | signer | signer | signer | signer | signer | signer |
| 11 | (slot 11) | **SPL Token (no slot 12)** → D5 | **SPL Token (no slot 12)** → D5 | **sysvar::clock** → D6 | SPL Token readonly | SPL Token readonly | SPL Token readonly |
| 12..N+11 | deposit_reserves (per obligation.deposits, writable each) | **absent** → D5 | **absent** → D5 | **absent** → also broke | **present (D5 fix)** | present | present (per `update_borrow_attribution_values`) |

`RefreshObligation` (Solend instruction tag 7):

| Slot | Account | Pre-6I-H builder | 6I-H builder (mainnet-verified) | Solend processor.rs source-of-truth |
|------|---------|-------|-------|---|
| 0 | obligation | writable | writable | writable |
| 1 | (slot 1) | **sysvar::clock** → D4 | first referenced_reserve | first referenced_reserve |
| 2..N | referenced_reserves | shifted by one → D4 | in obligation-deposit order | in obligation-deposit order (Clock read via `Clock::get()` syscall, not as account) |

Final corrected withdraw-ix layout (post-6I-K, mainnet-verified by `3tMpTSEn…EZPJZ`), cited from `docs/proofs/LLM_SOLEND_WITHDRAW_MAINNET_E2E_PHASE6I.md:121-126` and `crates/gateway/src/integrations/solend/withdraw.rs:277-310`:

- no `Clock` account anywhere
- slot 4 `lending_market` **writable**
- slot 11 `Tokenkeg…VQ5DA` (SPL Token program), readonly
- slot 12+ writable `deposit_reserve` entries in `obligation.deposits` order
- signer set: wallet at slots 9 and 10

## Cross-citation map for Agent 2

For each fact, the following routing rules apply once Solfrontier2026 ships its slim form:

- **Solscan tx links** — always resolve (repo-independent). Use freely.
- **Files that will exist in Solfrontier2026 per the execution plan** (`ARCHITECTURE.md`, `SECURITY_BOUNDARIES.md`, `DEMO_EVIDENCE.md`, `PHASE5C_LITE_MAINNET_PROOF.md`, `REVIEWER_QUICK_PATH.md`, `ROADMAP.md`) — safe to deep-link with `https://github.com/jeffreycheung521hk/Solfrontier2026/blob/main/<FILE>`. **CAVEAT: as of 2026-05-28 only `ARCHITECTURE.md` and `ROADMAP.md` are confirmed present on `staging2026/main`**; the other four (`SECURITY_BOUNDARIES.md`, `DEMO_EVIDENCE.md`, `PHASE5C_LITE_MAINNET_PROOF.md`, `REVIEWER_QUICK_PATH.md`) are not yet on `staging2026/main`. Agent 2 should confirm presence before deep-linking.
- **`docs/proofs/*` and `docs/WEEKLY_UPDATES.md`** — per the execution plan, these will **NOT** exist in Solfrontier2026. **CAVEAT: as of 2026-05-28 they DO exist on `staging2026/main` and `staging2026/integrate-phase5c-lite`** — the slim-down hasn't happened yet. Agent 2 should follow the execution-plan policy (mention by name in prose only, no hyperlink), even if the slim isn't yet on the public remote, so the post doesn't depend on Jeff's timing.
- **Commit hashes (`57f135f`, `d7d960d`, `e4195fa`, `8d59697`)** — these are post-judging-freeze commits and exist on the staging2026 / integrate-phase5c-lite branches but **not** on `testingcrypto2 main` (judging-frozen at `44a807d`, pre-May 8). If Agent 2 wants clickable commit links, they must go to Solfrontier2026, not testingcrypto2. Safest form: cite the short hash + commit subject in prose, with no hyperlink, to avoid a broken-link risk.
- **`crates/gateway/src/integrations/solend/withdraw.rs:283-289`** and similar code cites — these files exist in Solfrontier2026 (verified on `staging2026/integrate-phase5c-lite`). Safe to deep-link if Agent 2 wants to show the post-fix code.
- **README.md** and **PITCH.md** at the root — exist on Solfrontier2026; safe to deep-link.

## Conflicts / open questions for Jeff before draft

1. **`5W3PzXnR…NMNRVg` (D7) has no full base58 expansion in the repo.** Only the truncated form appears, in three places: `LLM_SOLEND_WITHDRAW_MAINNET_E2E_PHASE6I.md:118`, `WEEKLY_UPDATES.md:264`, and `crates/gateway/src/integrations/solend/withdraw.rs:288`. Without the full signature, no Solscan link can be built. **Action needed:** Jeff to expand from local logs (`data/clawd-phase6i-k-live.log`, referenced in the proof doc) before publish, OR the post describes D7 without a clickable failed-tx link.
2. **`3PUtp…21QT` (D2) has no full base58 expansion in the repo.** Mentioned only as a code comment at `crates/gateway/src/integrations/solend_tx_plan.rs:89`. Same action question as #1, but note this tx **was never finalized**, so even with the full sig Solscan may not show useful state.
3. **Solfrontier2026 slim-down status.** The execution plan (`docs/redesign/_t1_execution_plan.md:30-33`) claims Solfrontier2026 has `SECURITY_BOUNDARIES.md` / `DEMO_EVIDENCE.md` / `PHASE5C_LITE_MAINNET_PROOF.md` / `REVIEWER_QUICK_PATH.md` and does NOT have `docs/proofs/` or `docs/WEEKLY_UPDATES.md`. Current `staging2026/main` (verified 2026-05-28 via `git ls-tree`) **has neither set of changes applied** — it still has `docs/proofs/` and `WEEKLY_UPDATES.md`, and lacks the four redesign docs. **Action needed:** Jeff to either land the slim-down on `staging2026/main` before publish, or Agent 2 must avoid deep-linking any of those four files (cite them by name only).
4. **Slot numbers for failed txs (D4–D7) not in repo.** All four failed txs have full or truncated signatures but no slot metadata in any doc. Solscan resolves slot from sig; the post can defer slot lookup to Solscan rather than asserting a number.
5. **Memory vs repo — fully consistent on all 4 success txs.** Memory's ellipsis-truncated forms expand exactly to the full base58 strings in `docs/proofs/INDEX.md` / `LLM_SOLEND_*` / `WEEKLY_UPDATES.md`. No conflicts.
6. **Phase 4-blocked PDA-seed historical site.** Memory says `solend_deposit.rs:119` panicked on a 33-byte seed (`project_phase4_blocked_seed_bug`). The current code at the closest analogue (`crates/gateway/src/integrations/solend/deposit.rs:167-170`) uses a 32-byte seed `lending_market.to_bytes()` — the historical bug is *not* visible in the tree (it was squashed under the Phase 4C-8F → 4C-8G → 7bc42da seal). The lesson is real; the historical line number is not directly resolvable to a public commit Agent 2 can link to.
7. **The "4-attempt fail-closed" framing on Phase 5G (3 incidents, not 4).** `WEEKLY_UPDATES.md:144-155` calls it a "4-attempt fail-closed record" because attempt 4 was the success — so the table reads "3 failures + 1 success = 4 attempts". `PHASE5_CLOSEOUT.md:82-90` lists 3 incidents. Both are correct; the task brief lists 3 defenses (D1-D3) for Phase 5G, which aligns with the PHASE5_CLOSEOUT framing. Recommend Agent 2 use the "3 defenses, all caught pre-submission" framing — it's cleaner and matches the seal doc.

## Publishability Decision Before Agent 2

Added 2026-05-28 after Jeff reviewed Agent 1's findings. This section is load-bearing for Agent 2 — it governs how D2 and D7 may appear in the draft.

### D2 — Zero-priority-fee mempool drop

- **Evidence state:** No on-chain tx evidence. The tx signature in repo (`3PUtp…21QT`, `crates/gateway/src/integrations/solend_tx_plan.rs:89`) is truncated; full base58 not recoverable from current tree. Even if recovered, the tx was never finalized, so Solscan would show empty/dropped state at best.
- **Reframe:** Treat as an **off-chain confirmation-tracker / mempool-drop diagnostic**, not a Solscan-verifiable mainnet defense.
- **Permitted framing in Agent 2 draft:** "Off-chain failure mode caught by our local confirmation tracker before the next submission attempt." Cite the priority-fee code path and the tracker, not Solscan.
- **Forbidden framing:** Any wording that implies D2 is on-chain proven, Solscan-verified, or carries a finalized tx.

### D7 — ReadonlyDataModified (slot 4 lending_market writability)

- **Evidence state:** Tx signature `5W3PzXnR…NMNRVg` is truncated in all three repo locations (`LLM_SOLEND_WITHDRAW_MAINNET_E2E_PHASE6I.md:118`, `WEEKLY_UPDATES.md:264`, `crates/gateway/src/integrations/solend/withdraw.rs:288`); no full base58 expansion exists in the tree. Root cause is well-supported by commit `8d59697` (Phase 6I-K fix) and the post-fix source at `crates/gateway/src/integrations/solend/withdraw.rs:283-289`.
- **Label:** **code-supported, tx-link missing.**
- **Reframe:** Treat as a **processor-parity debugging incident** with strong code-level evidence but no clickable failed-tx link, **unless Jeff recovers the full tx signature from local logs (`data/clawd-phase6i-k-live.log`) before drafting.**
- **Permitted framing in Agent 2 draft:** "Reverted on-chain with `ReadonlyDataModified` at slot 4; root cause and fix documented in commit `8d59697` and post-fix source at `withdraw.rs:283-289`." Cite the commit and source; do NOT fabricate or partially quote a Solscan link.
- **Forbidden framing:** Any wording that implies D7 carries a fully Solscan-verifiable failed-tx link.

### Anti-drift rule for Agent 2 (load-bearing)

> **Agent 2 MUST NOT describe D2 or D7 as Solscan-verifiable or fully on-chain proven unless Jeff provides full transaction signatures before drafting.**

If Agent 2 needs an on-chain anchor for these two incidents, it must point at the success tx (Phase 5G `4M4ezLgm…` for D2's context; Phase 6I-K `3tMpTSEn…` for D7's context), not at the failed tx.

### Three publish options for Jeff

**Option A — Keep 7-defense article with softened D2/D7.**
- Title must drop "verified on-chain" framing. Acceptable: `7 fail-closed lessons from shipping a Solana control plane on mainnet`. Unacceptable: anything claiming all 7 are Solscan-verified.
- D2 framed as off-chain diagnostic; D7 framed as code-supported processor-parity incident with missing tx link.
- Strongest career narrative (full arc from PDA-seed to ReadonlyDataModified).
- Mixed evidence weight — 5 strong, 2 softened.

**Option B — Drop D2/D7; write "5 verified fail-closed defenses".**
- Article scope: D1, D3, D4, D5, D6. All have full Solscan-verifiable tx OR pre-submission rejection with code evidence.
- Cleanest evidence story; every defense in the post has unambiguous on-chain or code-cited backing.
- Narrative loses the mempool-drop diagnostic and the slot-4-writability finale (which is, arguably, the strongest processor-parity lesson).

**Option C — Pivot to Phase 5c-lite mainnet proof article.**
- Article scope: the 2026-05-16 Phase 5c-lite end-to-end loop (funding tx `oF5XWW…K8Je9` slot 420,130,565 + Solend deposit `2jynvL…HUVnJ` slot 420,131,308), framed as a single coherent mainnet proof rather than a defense montage.
- Safest external posture: one tightly-bounded claim, two finalized txs, no truncated-sig caveats.
- Loses the "60 days, 7 defenses, 0 funds lost" framing.

### Recommendation

- **Best career narrative:** Option A, but title must say "lessons", not "verified on-chain defenses".
- **Safest technical article:** Option C, Phase 5c-lite proof article.
- **Most conservative evidence-only path:** Option B, 5 verified defenses.

Jeff to pick A / B / C before Agent 2 is dispatched. Agent 2 is on hold.

## Decision lock for Agent 2 (2026-05-28)

Jeff confirmed Option A with the following refinements. Agent 2 MUST follow these.

### Locked title

> **Seven fail-closed lessons on Solana mainnet — what 60 days shipping solo taught me about Solend processor parity**

This is the working title. Agent 2 may offer 2 tightened variants at the bottom of the draft (per execution plan), but A is default. **Forbidden alternatives:**
- Anything claiming "verified on-chain defenses" or similar (D2/D7 don't support it)
- "Almost broke Solend" framings — violates the "do not imply Solend has bugs; frame as parity, not bug" voice rule

### Locked front-loaded disclosure paragraph

Within the first ~200 words of the post (hook section), Agent 2 MUST surface an honest evidence-strength accounting in roughly this shape (Agent 2 may polish wording, may not change the counts):

> Of the 7 incidents, **3 left full on-chain failed-tx traces you can verify on Solscan, 2 were caught pre-submission by code-level guardrails before any tx existed, and 2 are code-supported with the failed-tx signatures truncated in our internal record.** All 7 share one outcome: zero user funds lost.

**The counts (3 / 2 / 2) are non-negotiable.** Mapping:
- 3 Solscan-verified failed tx: D4, D5, D6
- 2 pre-submission catches: D1, D3
- 2 softened: D2 (off-chain mempool drop), D7 (code-supported, tx-link truncated)

### D7 sig recovery — attempted, not recovered

2026-05-28: Searched `data/clawd-phase6i-k-live.log` (the log referenced in the proof doc) for `5W3PzXnR` and `ReadonlyDataModified` — no matches. Also searched the full `data/` directory (all phase6, phase6i-h/i/j/k logs) — no matches. The K log is the success attempt and does not contain the prior failed D7 attempt; no other log captures it either.

**Conclusion:** D7 stays "code-supported, tx-link missing" for the post. Agent 2 must apply the existing D7 forbidden-framing rule unchanged.

### What Agent 2 should treat as authoritative

In order of authority for any conflict:
1. This **Decision lock** section (locked title, locked disclosure, recovery result)
2. The **Publishability Decision Before Agent 2** section above (D2/D7 reframes, anti-drift rule, recommendation block)
3. The **per-defense blocks** earlier in this document (fields, citations, publish-safety labels)
4. The **execution plan** (`docs/redesign/_t1_execution_plan.md`) voice rules, structure, word counts
5. README.md and docs/proofs/* for any factual lookup not covered above

If two layers disagree, the higher-numbered (lower-priority) layer loses.
