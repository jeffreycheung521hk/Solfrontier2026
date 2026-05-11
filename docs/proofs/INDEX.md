# Proof Index

Every mainnet finalization, every live LLM provider call, every fail-closed defense incident in this project has a corresponding artifact in this directory. **Each proof document was written programmatically only after its own success criterion was observed.** Nothing here is a draft, a mock, or aspirational.

> **Scope reminder.** ClawSolana is a hackathon prototype. Each proof below is a verified live mainnet (or live-LLM) result at controlled-test-wallet amounts. The artifacts demonstrate the control-plane mechanics; they are not a representation that the system is production-deployed or ready to handle real treasury balances.

The default `cargo test` path performs **zero** network calls. All live harnesses default-skip behind double opt-in env gates (master + execution). CI is safe by construction.

## Mainnet & Live-LLM Proof Ledger

Most-recent first. Click any tx hash to verify on Solscan.

| Artifact | What It Proves | Status | Date | Public Evidence | Proof Doc |
|---|---|---|---|---|---|
| **Stage 2 W5b** — controlled-wallet Solend deposit substrate (live) | Live controlled-wallet Solend deposit substrate end-to-end on mainnet — `RefreshReserve` + `DepositReserveLiquidityAndObligationCollateral` in same tx. **Does NOT** prove Stage 2 clawsol-authority `ExecuteAction` live withdraw. | Finalized | 2026-05-11 | [`5BiNNVcK…aLwa`](https://solscan.io/tx/5BiNNVcKfrRkzx16kM3aTYfEYwP4uCk9grSZ2Jvsig2RQEs4Lgw8uv61hqzzBa2Dqzx5EWHCmbqpdo9w8rDSaLwa) — slot 418,985,346 | [`STAGE2_W5B_SOLEND_DEPOSIT_MAINNET.md`](STAGE2_W5B_SOLEND_DEPOSIT_MAINNET.md) |
| **Phase 6I-K** — LLM-guided Solend withdraw mainnet | Withdraw-side counterpart to Phase 5G: NL → OpenAI → tool dispatch → human approval → JIT signing handoff → Phantom → daemon submit → mainnet `Finalized`; round-trip closed against the same wallet's May 7 deposit | Finalized | 2026-05-08 | [`3tMpTSEn…EZPJZ`](https://solscan.io/tx/3tMpTSEnmvszjSwgow43KMd4dLqnZwwSvnTbHkcPsCiyfizLf2h7kcMc1M7LwokHLC3SBPzpTfykjsc48z8EZPJZ) — slot 418,395,346 | [`LLM_SOLEND_WITHDRAW_MAINNET_E2E_PHASE6I.md`](LLM_SOLEND_WITHDRAW_MAINNET_E2E_PHASE6I.md) |
| **Phase 5 Closeout** | Milestone seal — security model, fail-closed defense record, slice-by-slice components | (sealed) | 2026-04-26 | (doc) | [`PHASE5_CLOSEOUT.md`](PHASE5_CLOSEOUT.md) |
| **Phase 5G** — LLM-guided Solend mainnet | NL → live OpenAI → strict 1-turn handler → Solend → mainnet `Finalized`, with LLM holding zero signing authority | Finalized | 2026-04-25 | [`4M4ezLgm…Py3y`](https://solscan.io/tx/4M4ezLgm1mFpGmUpLJdDAVhfXYwUxjS2ZMkjKprBiWzsfgNudPkhEvBr6GdJbh1zBscKLF6kpUBhZg7tAm3ePy3y) — slot 415,571,964 | [`LLM_SOLEND_MAINNET_E2E_PHASE5G.md`](LLM_SOLEND_MAINNET_E2E_PHASE5G.md) |
| **Phase 5F** — live LLM chat dry run | Real OpenAI gpt-4o-mini call through `POST /sessions/:id/chat`; one tool call only; strict schema enforced | Stopped at `awaiting_approval` (intentional) | 2026-04-25 | (no on-chain — proposal-only dry run) | [`LLM_SOLEND_CHAT_DRY_RUN_PHASE5E.md`](LLM_SOLEND_CHAT_DRY_RUN_PHASE5E.md) |
| **Phase 4C** — non-LLM Solend mainnet baseline | Solend execution rail correct end-to-end without any LLM in the loop | Finalized | 2026-04-25 | [`2QqSfDq…pxALs`](https://solscan.io/tx/2QqSfDq53a34WDXVkvZeyW59eFJcxZtkQtjZw25CoUizEmzDjahusKvBT8eNb4XdpKYwrgGcTyBwYXgth5wpxALs) — slot 415,475,589 | [`SOLEND_USDC_DEPOSIT_PHASE4C_E2E.md`](SOLEND_USDC_DEPOSIT_PHASE4C_E2E.md) |
| **Slice 3G** — first controlled Solend deposit | First mainnet Solend USDC deposit (3-tx sequence: InitObligation → RefreshReserve → Deposit) with controlled wallet | All 3 txs Finalized | 2026-04-23 | [`4Ud5USKM…UL9W`](https://solscan.io/tx/4Ud5USKMrG3UJsERu9NPjokY1UuNs5X7PDZ55JTP39L68x5jxA6HtK6Cuk1SArB6kFPm1cfM2Pu2b9HPWxm8UL9W), [`3pVZoGNy…Spku`](https://solscan.io/tx/3pVZoGNyrs7oHBFMzouAA26gW9jKCcopzg1mUUoZGWtNn85FQZjFwqWydFg5Kc9JvieiFw7cFLGcD9ccCu76Spku), [`6aU5syvX…eS7N`](https://solscan.io/tx/6aU5syvXgg9Ps2r6HFuqMiA4mignBjUSGRbfFreRoUHdSyuadLQfoLdVbVUidJQHLWqmqjaEtFQsVWhAZa4eS7N) | [`SOLEND_USDC_DEPOSIT_SLICE3G.md`](SOLEND_USDC_DEPOSIT_SLICE3G.md) |
| **Jupiter A2-Execute** — LLM-driven Phantom swap | NL → OpenAI → daemon `complete_v0` → Phantom signs → daemon broadcasts | Finalized | 2026-04-18 | [`52bvUZ1e…D2dE1`](https://solscan.io/tx/52bvUZ1eEHjrbxRBQBssYWg7BrFNoAY1Zeynw44hr3BKTAe9zTHv9Qz4wQsNi7Jw6sbXpb8zehHafn1gFzLD2dE1) | [`JUPITER_LIVE_MAINNET_PROOF.md`](JUPITER_LIVE_MAINNET_PROOF.md) · [`a2_execute_tx_signature.txt`](a2_execute_tx_signature.txt) |
| **Jupiter A1-Execute** — daemon-path swap | Full daemon `complete_v0` + `send_raw_v0_transaction` live (in-process tool invoke, not LLM-driven) | Finalized | 2026-04-18 | [`Q5pbBmjj…1zK6H`](https://solscan.io/tx/Q5pbBmjjn7tbFKEyswzyFsAkYoGHVJe4TT2m2viGbBCa2j9LXrFQ8X8AeKjdXw3seom2YYezFdtS8jBbpA1zK6H) | [`JUPITER_LIVE_MAINNET_PROOF.md`](JUPITER_LIVE_MAINNET_PROOF.md) · [`a1_execute_tx_signature.txt`](a1_execute_tx_signature.txt) |

## Reading Order

For a reviewer with 5 minutes:
1. **[Phase 5 Closeout](PHASE5_CLOSEOUT.md)** — milestone seal, security model, fail-closed record
2. **[Phase 5G proof](LLM_SOLEND_MAINNET_E2E_PHASE5G.md)** — the LLM-guided deposit mainnet artifact
3. **[Phase 6I-K proof](LLM_SOLEND_WITHDRAW_MAINNET_E2E_PHASE6I.md)** — the LLM-guided withdraw mainnet artifact (round-trip closed)

For a reviewer with 30 minutes:
4. **[Phase 4C proof](SOLEND_USDC_DEPOSIT_PHASE4C_E2E.md)** — non-LLM baseline (Solend rail correctness)
5. **[Phase 5F proof](LLM_SOLEND_CHAT_DRY_RUN_PHASE5E.md)** — chat-route correctness without execution rail
6. **[`ARCHITECTURE.md`](../../ARCHITECTURE.md)** — system invariants this proof rail depends on

For a reviewer who wants the historical depth:
7. **[Slice 3G proof](SOLEND_USDC_DEPOSIT_SLICE3G.md)** — first mainnet Solend deposit (pre-LLM integration)
8. **[Jupiter live mainnet proof](JUPITER_LIVE_MAINNET_PROOF.md)** — earlier swap integration with Phantom bridge

## Other Artifacts in this Directory

| File | Purpose |
|---|---|
| `phantom_a1_bind.html`, `phantom_a1_sign_v0.html` | Standalone Phantom-bridge HTML used during the Jupiter A1 proof. Not integrated into the main frontend; preserved as evidence the V0 signing path works in-browser. |
| `phantom_a2_bind.html`, `phantom_a2_sign_v0.html` | Same, for the LLM-driven A2 path. |
| `phantom_sign_and_broadcast.html` | Combined sign-and-broadcast utility used for early Jupiter validation. |
| `jupiter_live_mainnet_shape.log` | Captured wire shapes from the Jupiter mainnet run for regression reference. |
| `jupiter_mainnet_unsigned_tx.b64` | Sanitized record of an unsigned V0 transaction layout (no key material; structure-only). |
| `a1_execute_tx_signature.txt`, `a2_execute_tx_signature.txt` | Plain-text records of the on-chain signatures, mirroring the Jupiter proof doc. |

## Safety Posture (Common to All Proofs)

- **Default-skip env gates** — every live harness requires an explicit master env var (`CLAW_LIVE_*=1`) plus an execution gate (`*_GO=1`). CI / `cargo test` performs zero network calls.
- **Programmatic proof-doc writes** — every proof document is generated by the harness *only* after its own success criterion was observed (`Finalized` from the confirmation tracker, or the strict-success branch from the live-provider dispatcher). Failure paths do not produce proof docs.
- **Public chain data only** — proof docs include public tx signatures, public wallet pubkeys, public approval/signing request ids, and on-chain slot numbers. They do **not** include private keys, API keys, bearer tokens, raw transaction payloads, serialized byte arrays, or any HTTP auth headers. Each proof writer runs a final substring scan against a deny-list before writing.
- **One attempt per harness invocation** — no automatic retry on any failure path. A failed run produces a typed failure outcome, not a re-broadcast.

For the full fail-closed defense record (three documented mainnet incidents, zero funds lost), see [`PHASE5_CLOSEOUT.md`](PHASE5_CLOSEOUT.md) §5.

## Cross-References

- [`../../ARCHITECTURE.md`](../../ARCHITECTURE.md) — System invariants (INV-1 through INV-8) the safety boundary depends on
- [`../../DEBT.md`](../../DEBT.md) — Technical debt ledger; D-2 daemon attrition is the relevant entry for the runtime/wiring layout
- [`../../README.md`](../../README.md) — Top-level project entry
- [`../../PITCH.md`](../../PITCH.md) — Hackathon pitch with cross-links into this directory
