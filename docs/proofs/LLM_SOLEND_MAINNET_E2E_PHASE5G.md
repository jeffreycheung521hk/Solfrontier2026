# Phase 5G — LLM-Guided Solend USDC Deposit Mainnet Proof

**Date (UTC):** 2026-04-25T13:14:54Z
**Git commit:** `7bc42da0d6b52c89a8862a9ed85b98ed5706d9c9`
**Git working tree:** dirty (uncommitted changes present at proof time)

## On-chain outcome

- **Public on-chain hash:** `4M4ezLgm1mFpGmUpLJdDAVhfXYwUxjS2ZMkjKprBiWzsfgNudPkhEvBr6GdJbh1zBscKLF6kpUBhZg7tAm3ePy3y`
- **Finalized slot:** 415571964
- **Last valid block height (at submit):** 393666166
- **Lifecycle states observed:** awaiting_signature → submitted → finalized

## LLM provider

- **Provider:** openai
- **Model:** gpt-4o-mini
- **Tool surface:** narrowed to `solend_deposit_usdc` only (strict schema, amount-only)
- **HTTP timeout:** 15 s
- **max_tokens:** 200
- **System prompt:** alignment-override constant from `chat_wiring::ALIGNMENT_SYSTEM_PROMPT` (no user content)

## Deposit parameters

- **Protocol:** Solend V1
- **Reserve mint:** `EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v` (USDC)
- **Solend reserve pubkey:** `BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw`
- **Amount (raw, 6 decimals):** 1000
- **Amount (UI):** 0.001000 USDC
- **RPC cluster:** https://mainnet.helius-rpc.com
- **Session wallet pubkey:** `BPfDMmeMBmCbMC1rWh7hwigMBoKGBrKwXxSeUu9hhs5L`
- **Source USDC ATA:** `7LFdKcSV7JQYi3or5y9phHVPjhGigu5DDjUakAFbBmk3`

## Control-plane identifiers

- **Approval request id:** `25bf7195-4664-4181-9ec9-7186a8d11393`
- **Signing request id:** `061725ed-4be5-42ae-9b99-fb05ed669b20`

## Proof flow

1. Pre-run state sanitisation: fresh in-process Solend stores; zero pending state.
2. RPC freshness check via `get_slot` succeeded.
3. Fail-fast balance guards: SOL ≥ 0.01 SOL; USDC ATA mint/owner verified; USDC raw ≥ amount.
4. Controlled keypair bound to session via `ExternalWalletStore::bind_wallet`.
5. `wire_chat_handler_with_registry` constructed a real provider chat handler (15 s timeout, 200 max_tokens) attached to the production Solend tool registry.
6. One natural-language chat turn was sent through `ChatHandler::handle_chat`. The provider returned exactly one tool call: `solend_deposit_usdc { amount: 1000 }`.
7. The handler dispatched to the production Solend tool, which produced `awaiting_approval` (no auto-approve, no auto-sign).
8. Test harness explicitly approved via `ApprovalStore::decide(Approve)` + `approval_routing::route_approval_outcome` — the human-approval seam.
9. Resume task reassembled the Solend snapshot, re-ran the lending policy evaluator, executed the 4C-3 preflight simulator, and produced a partially-signed transaction in `SolendSigningStore::create_signing_handoff`.
10. `GatewaySolendSignatureHandler::retrieve` returned the partial transaction; the controlled keypair signed only the session-wallet slot. Message body and obligation slot were not modified.
11. `GatewaySolendSignatureHandler::submit` ran the 4C-5 verification pipeline (atomic consume, message-hash immutability, session + obligation signature checks, blockhash freshness) and broadcast.
12. `SolendConfirmationTracker` polled `getSignatureStatuses` until `confirmation_status = "finalized"`; the lifecycle store transitioned `Submitted → Confirming → Finalized`.
13. This proof document was written programmatically only after the `Finalized` variant was observed.

## Cross-references

- **Phase 4C non-LLM proof:** [`SOLEND_USDC_DEPOSIT_PHASE4C_E2E.md`](SOLEND_USDC_DEPOSIT_PHASE4C_E2E.md) — same Solend pipeline, direct tool invocation.
- **Phase 5F live chat dry run:** [`LLM_SOLEND_CHAT_DRY_RUN_PHASE5E.md`](LLM_SOLEND_CHAT_DRY_RUN_PHASE5E.md) — same chat route, stub Solend tool.

## Safety confirmations

- The LLM proposed only — it never approved, never signed, never submitted, never broadcast.
- The test harness explicitly performed the approval step via `ApprovalStore::decide`.
- The controlled keypair signed only the session-wallet slot of the partially-signed transaction.
- The backend never signed the session-wallet slot.
- The submit path verified message-hash immutability across signing.
- On-chain `Finalized` was observed before this proof was written.
- No private-key bytes, provider credentials, or HTTP auth headers were printed, logged, or persisted by the harness.
- No serialised transaction payload or raw byte array is included in this document.
- No automatic retry occurred. Exactly one provider call was issued and exactly one broadcast was attempted.

## Default-skip posture

- The harness default-skips with zero network calls unless every gate is set: `CLAW_LIVE_LLM_SOLEND_E2E=1`, `CLAW_LIVE_LLM_SOLEND_E2E_GO=1`, `CLAW_LIVE_SOLEND_E2E=1`, `CLAW_LIVE_SOLEND_E2E_BROADCAST=1`, plus `CLAW_CHAT_PROVIDER` and the matching API key.
- Amount cap: `CLAW_LIVE_SOLEND_E2E_AMOUNT_RAW` hard-capped at `10000` raw USDC.
