# External Signer Bridge Simulation

> **Status (2026-03-21):** The bridge simulation tests described here are still valid and passing. Since this document was written, the signing path now routes through `SignatureOrchestrator` rather than direct `ExternalWalletStore`. The simulation approach (in-process axum Router + tower oneshot) remains the same pattern.

## Purpose

This document describes how the external signer bridge is tested **without a real wallet** (Phantom, Ledger, etc.). The backend is wallet-provider agnostic. "External signer" means any client-side signing workflow.

## Architecture

```
                      ┌─────────────────────────────────┐
                      │        ClawSolana Backend        │
                      │                                  │
Agent calls           │  SubmitForSigningTool             │
submit_for_signing    │    → simulate()                  │
(external wallet)     │    → evaluate_policy()           │
                      │    → park finalized unsigned tx   │
                      │    → return unsigned_tx_b64       │
                      │                                  │
          ┌──────────►│  POST /wallet-signatures          │
          │           │    → deserialize signed tx        │
          │           │    → verify message bytes match   │
          │           │    → verify expected signer       │
          │           │    → verify ed25519 signature     │
          │           │    → complete or reject           │
          │           └─────────────────────────────────┘
          │
          │           ┌─────────────────────────────────┐
          │           │     External Signer (Client)     │
          │           │                                  │
          └───────────│  GET /wallet-signatures           │
                      │    → extract unsigned_tx_b64     │
                      │    → base64 decode               │
                      │    → bincode deserialize         │
                      │    → sign with local keypair     │
                      │    → bincode serialize           │
                      │    → base64 encode               │
                      │    → POST /wallet-signatures     │
                      └─────────────────────────────────┘
```

## Test Coverage

### Unit tests (`n10a_external_wallet.rs`)

| Test | What it proves |
|------|----------------|
| `session_wallet_binding` | Bind/unbind/session isolation |
| `park_complete_lifecycle` | Full park → sign → complete → receive |
| `message_bytes_match_accepted` | Signing doesn't change message bytes |
| `modified_instructions_rejected` | Tampered instructions detected |
| `changed_blockhash_rejected` | Changed blockhash detected |
| `compute_budget_tampering_rejected` | CU instruction tampering detected |
| `wrong_wallet_signature_rejected` | Unsigned tx at signer position rejected |
| `double_complete_rejected` | One-time-actionable enforcement |
| `park_reject_lifecycle` | Rejection yields None |
| `multiple_wallets_per_session` | Multi-wallet support |
| `expected_signer_retrieval` | Correct pubkey from store |
| `get_parked_tx_bytes` | Parked bytes match original |
| `pending_for_session_returns_matching` | Session-filtered listing |
| `happy_path_external_signer_roundtrip` | Store-level submit_signed_transaction |
| `reject_on_message_mismatch` | Store-level rejection on tampered msg |
| `reject_on_signer_mismatch` | Store-level rejection on unsigned |
| `reject_on_invalid_signature` | Corrupted ed25519 signature rejected |
| `verify_signed_tx_happy_path` | Pure verification passes |

### HTTP integration tests (`n10b_bridge_simulation.rs`)

| Test | What it proves |
|------|----------------|
| `bridge_roundtrip_happy_path` | Full HTTP: bind → park → list → sign → submit → accepted → pending empty |
| `bridge_reject_tampered_message` | HTTP: tampered tx → 400 BAD_REQUEST |
| `bridge_reject_wrong_signer` | HTTP: unsigned tx → 400 BAD_REQUEST |

## Verification Invariants

The `verify_signed_tx()` function enforces 4 checks in order:

1. **Message bytes identical** — `original_tx.message_data() == signed_tx.message_data()`
2. **Expected signer in account_keys** — signer must be in the transaction
3. **Non-default signature** — signature slot must not be all zeros
4. **Ed25519 cryptographic verification** — `Signature::verify(pubkey, message)`

Any failure rejects the submission and removes the pending entry.

## How to Run

```bash
# Run all external signer tests
cargo test --test n10a_external_wallet
cargo test --test n10b_bridge_simulation

# Run the full suite
cargo test
```

## Manual Testing Against Running Daemon

To test against a running daemon with curl:

```bash
# 1. Set variables
TOKEN="your-api-token"
BASE="http://127.0.0.1:7070"
SESSION_ID="your-session-id"

# 2. Bind an external wallet
curl -X POST "$BASE/sessions/$SESSION_ID/bind-wallet" \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"pubkey": "YOUR_WALLET_PUBKEY"}'

# 3. Agent calls submit_for_signing targeting the external wallet
#    → returns { status: "awaiting_wallet_signature", unsigned_tx_b64: "..." }

# 4. List pending signatures
curl "$BASE/sessions/$SESSION_ID/wallet-signatures" \
  -H "Authorization: Bearer $TOKEN"

# 5. Extract unsigned_tx_b64, sign externally, submit
curl -X POST "$BASE/sessions/$SESSION_ID/wallet-signatures" \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"request_id": "REQUEST_ID", "signed_tx_b64": "SIGNED_TX_B64"}'
```

## What is NOT Tested Here

- Real Phantom browser wallet interaction (frontend concern)
- RPC transaction submission to Solana (deferred)
- Transaction confirmation/finalization (deferred)
- Request expiry/timeout (deferred, documented in HANDOFF.md)
- Orca/Solend DeFi execution (separate milestone)

## Next Steps

1. **Phantom frontend integration** — connect browser wallet to the bind + sign + submit flow
2. **RPC submission** — after verification passes, submit the signed tx to Solana
3. **Confirmation tracking** — monitor tx confirmation status via websocket
