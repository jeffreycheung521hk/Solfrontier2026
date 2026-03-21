# Phantom Bridge and Orca Execution Plan

---

## Purpose

This document defines the next milestone after External Signer Hardening:

> Enable end-to-end flow:
>
> **AI Agent → Build Transaction → Review Pipeline → Phantom Wallet Sign → Verified Submit → Orca Execution**

This phase turns ClawSolana from:

- "secure transaction control plane"

into:

- **"agent-driven, user-approved on-chain execution system"**

---

## Scope

### Included

1. Phantom wallet bridge (frontend + backend contract)
2. External signer production flow (already hardened backend)
3. Orca execution (swap) — first write-capable protocol
4. End-to-end verified execution path

### Excluded (Future Phases)

- Solend execution (separate plan)
- Jupiter routing
- Multi-wallet batching
- Gas optimization / dynamic CU tuning (N10++)
- Mobile wallet support

---

## Core Invariant (Must Hold End-to-End)

> The transaction signed in Phantom MUST be identical to the transaction:
>
> - Constructed by the agent
> - Finalized by pipeline
> - Simulated
> - Policy-checked
> - Approved

No mutation allowed between:

```
Pipeline Finalization → Phantom Signing → Backend Verification → Submission
```

---

## High-Level Architecture

```
Agent (LLM)
   ↓
Tool: build_orca_swap_tx
   ↓
Unsigned Transaction
   ↓
Pipeline (simulate → policy → approval)
   ↓
AwaitingWalletSignature
   ↓
Frontend (Phantom)
   ↓
User signs
   ↓
Signed TX → Backend
   ↓
Verification (exact match)
   ↓
Submit to Solana
   ↓
Track result
```

---

# Phase 1 — Phantom Wallet Bridge (Frontend Contract)

## Goal

Allow frontend to:

- Connect Phantom
- Receive signing request
- Sign finalized tx
- Return signed tx

## Requirements

### 1. Wallet connection

Frontend must:

- Connect Phantom via `window.solana`
- Obtain:
  - `publicKey`
- Send to backend:

```json
POST /sessions/:id/bind-wallet
{
  "wallet_pubkey": "...",
  "provider": "phantom"
}
```

### 2. Receive signing request

Backend returns:

```json
{
  "status": "awaiting_wallet_signature",
  "wallet_signature_request_id": "...",
  "unsigned_tx_b64": "...",
  "simulation": {...},
  "policy": {...}
}
```

Frontend must:

- Decode base64 → Transaction
- Display:
  - Amount
  - Token
  - Protocol (Orca)
  - Slippage
  - Fees

### 3. Phantom signing

Use:

```javascript
await window.solana.signTransaction(tx)
```

### 4. Send signed tx back

```json
POST /wallet-signatures
{
  "wallet_signature_request_id": "...",
  "signed_tx_b64": "..."
}
```

## Constraints

- Frontend must NOT modify tx
- No recomposition of instructions
- No re-fetch blockhash
- No adding compute budget

## Acceptance Criteria

- [ ] Phantom connects
- [ ] Tx is signed successfully
- [ ] Backend accepts valid signed tx
- [ ] Backend rejects modified tx

---

# Phase 2 — Backend: External Signer (Production Flow)

## Goal

Stabilize real-world external signer flow (Phantom).

## Requirements

Already implemented but must be verified:

- Session-wallet binding
- `awaiting_wallet_signature` state
- Finalized tx handoff
- Exact message verification
- Signer verification
- Verified submit path

## Additional Hardening (if not done)

- Enforce request expiry (optional, not blocking)
- Replay protection
- Duplicate submission protection

---

# Phase 3 — Orca Execution (Write Capability)

## Goal

Enable actual on-chain execution via Orca.

## Principle

Agent does NOT execute swaps directly.

Agent can only:

> Construct transaction → pipeline enforces → user approves → user signs

## Step 1 — New Tool: `build_orca_swap_tx`

### Input

```json
{
  "input_mint": "...",
  "output_mint": "...",
  "amount_in": "...",
  "slippage_bps": 50
}
```

### Output

Unsigned Solana Transaction.

## Step 2 — Implementation Requirements

### 1. Use real Orca Whirlpool logic

Must include:

- Pool lookup
- Tick array traversal
- Sqrt price limit
- Correct accounts
- Oracle if required

### 2. Slippage enforcement

- Compute `min_out`
- Enforce via instruction

### 3. Safety guards

- Allowlist pools
- Deny unknown pools
- Validate mint pair

### 4. No signing inside tool

Tool returns:

- Unsigned transaction ONLY

## Step 3 — Integration with pipeline

Tool output must go into:

```rust
pipeline.run()
```

Then:

- Simulate
- Apply compute budget
- Policy check
- Approval

## Step 4 — UI display

Frontend must show:

- Token in/out
- Amount
- Slippage
- Estimated output
- Protocol: Orca

## Acceptance Criteria

- [ ] Agent can build swap tx
- [ ] Tx passes pipeline
- [ ] User signs in Phantom
- [ ] Swap executes successfully

---

# Phase 4 — End-to-End Integration Test

## Required Tests

### 1. Happy path

- Agent builds Orca tx
- Pipeline approves
- Phantom signs
- Backend verifies
- Tx submitted
- Tx confirmed

### 2. Tampering tests

- Change blockhash → reject
- Change CU → reject
- Change instructions → reject
- Wrong wallet → reject

### 3. Approval gating

- Tx requiring approval cannot reach Phantom before approval

### 4. Replay protection

- Same signed tx submitted twice → controlled behavior

---

# Security Model

## Threats

| Threat | Defense |
|---|---|
| Client modifies tx | Message-bytes match |
| Wrong wallet signs | Signer check |
| Replay attack | `request_id` + state |
| Fee manipulation | Compute budget locked |
| Protocol abuse | Allowlist |

---

# UX Design Principles

- User must see **what they sign**
- No blind signing
- No hidden fees
- No silent retries

---

# Non-Goals

Do NOT:

- Integrate Jupiter yet
- Support multi-hop swaps
- Implement routing
- Add complex UI
- Optimize fees dynamically

---

# Final State Definition

System is complete when:

- [ ] Phantom wallet signs finalized tx
- [ ] Backend verifies exact tx
- [ ] Orca swap executes on-chain
- [ ] All invariants hold
- [ ] All tampering is rejected

---

# Next Steps (After This Plan)

After success:

1. Solend execution plan
2. Jupiter routing
3. Dynamic fee model (N10++)
4. Multi-protocol orchestration

---

# Summary

This phase achieves:

- Real wallet integration (Phantom)
- Real protocol execution (Orca)
- End-to-end secure agent execution

This is the point where ClawSolana becomes:

> **A production-grade AI-driven on-chain execution system**
