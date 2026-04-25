# Solend USDC Deposit — Phase 4C Full E2E Proof

**Date (UTC):** 2026-04-25T02:39:02Z
**Git commit:** `7bc42da0d6b52c89a8862a9ed85b98ed5706d9c9`
**Git working tree:** dirty (uncommitted changes present at proof time)

## On-chain outcome

- **Tx signature:** `2QqSfDq53a34WDXVkvZeyW59eFJcxZtkQtjZw25CoUizEmzDjahusKvBT8eNb4XdpKYwrgGcTyBwYXgth5wpxALs`
- **Finalized slot:** 415475589
- **Last valid block height (at submit):** 393569825
- **Lifecycle states observed:** awaiting_signature → submitted → finalized

## Deposit parameters

- **Protocol:** Solend V1
- **Reserve mint:** `EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v` (USDC)
- **Solend reserve pubkey:** `BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw`
- **Amount (raw, 6 decimals):** 1000
- **Amount (UI):** 0.001000 USDC
- **RPC cluster:** https://mainnet.helius-rpc.com
- **Session wallet pubkey:** `BPfDMmeMBmCbMC1rWh7hwigMBoKGBrKwXxSeUu9hhs5L`

## Control-plane identifiers

- **Approval request id:** `1fa8743d-2668-4026-9a9b-0d688db6e107`
- **Signing request id:** `1cff4107-d87c-43bb-b2b8-f5f4fdec36ab`

## Proof flow

1. `solend_deposit_usdc` tool invoked with `amount` only → `awaiting_approval`.
2. `ApprovalStore::decide(Approve)` + `approval_routing::route_approval_outcome` → 
signalled the parked Solend resume task.
3. Resume task reassembled the Solend snapshot against live RPC, re-ran the 
lending policy evaluator, executed the 4C-3 preflight simulator, and — on 
`Passed` — produced a partially-signed unsigned transaction in the 
`SolendSigningStore` via `create_signing_handoff`.
4. `GatewaySolendSignatureHandler::retrieve` returned the base64-encoded 
transaction to the test; the controlled test keypair signed the 
session-wallet slot without touching the obligation slot or the 
message body.
5. `GatewaySolendSignatureHandler::submit` ran the 4C-5 verification 
pipeline (atomic consume, message.hash immutability, session + obligation 
signature checks, blockhash freshness) and broadcast via 
`submit_signed_solend_transaction` → `ClawRpcSolendSender::send_raw_transaction`.
6. `SolendConfirmationTracker` polled `getSignatureStatuses` until 
`confirmation_status = "finalized"`; the lifecycle store transitioned 
`Submitted → Confirming → Finalized`.
7. This proof document was written programmatically only after the 
`Finalized` variant was observed through `retrieve(..)`.

## Safety posture

- **Default-skip env gate:** Test runs only when `CLAW_LIVE_SOLEND_E2E=1`. The CI 
default `cargo test` path performs zero network calls.
- **Broadcast gate:** Actual broadcast requires `CLAW_LIVE_SOLEND_E2E_BROADCAST=1` in 
addition to the master gate.
- **Amount cap:** `CLAW_LIVE_SOLEND_E2E_AMOUNT_RAW` hard-capped at `10000` raw USDC.
- **Finalized required:** `Confirmed` alone does not produce a proof. Only 
`confirmation_status = "finalized"` advances the lifecycle to terminal 
success and triggers this proof write.
- **No private key material in this document.** The controlled test keypair 
signed only the session-wallet slot of the partially-signed transaction 
returned by the backend. Private-key bytes are never printed or persisted 
by the test harness.
- **No raw transaction bytes in this document.** The `transaction_base64` 
returned by the retrieve endpoint is used only to sign; it is not written 
to the proof doc.
- **Verification path:** Submit goes through `submit_signed_solend_transaction` 
(4C-5) which enforces atomic consume, message-hash immutability, 
bit-equal obligation-signature check, session-wallet ed25519 verification, 
and blockhash freshness. Broadcast happens only after all checks pass.
- **No re-broadcast.** Timeout or Failed outcomes fail the proof closed; no 
retry, no re-sign, no re-handoff.
