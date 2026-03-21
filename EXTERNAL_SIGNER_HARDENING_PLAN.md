# External Signer Hardening Plan

## Purpose

This document defines the safe, step-by-step hardening plan for the **External Signer Bridge** in ClawSolana.

The system is already **functionally complete** for:

- External signer bridge
- Session-wallet binding
- Finalized transaction handoff
- Signed transaction verification
- Verified submit path

This plan focuses on **correctness, security, and observability hardening** — not feature expansion.

---

## Operating Principles

### Stability over speed

- No large refactors
- No multi-phase changes in a single patch
- One pass = one concern
- Each pass must be independently safe
- Each pass must end in a valid, test-passing state

### Always test-gated

Every pass must:

```
cargo check
cargo test
```

No exceptions.

### Do not regress existing flows

- Local signer flow must remain unchanged
- Approval loop must remain intact
- Typestate invariants must remain intact

---

## Core Invariant (Must Never Break)

> The transaction signed by the wallet must be **EXACTLY** the same transaction that was:
>
> 1. Finalized
> 2. Simulated
> 3. Policy-checked
> 4. Approved (if required)

This invariant is already enforced via:

- Finalized `tx` handoff in `AwaitingApproval`
- Exact `message_data()` comparison in verification

**All hardening must preserve this.**

---

## Current State Summary

External signer pipeline already supports:

- Session-wallet binding
- Finalized transaction handoff (`unsigned_tx_b64`)
- Awaiting wallet signature flow
- Signed tx submission endpoint
- Message-bytes verification
- Expected signer verification
- Audit logging (partial)
- SSE events (partial)
- Spend recording on signature acceptance

**Remaining work is targeted hardening only.**

---

## PASS 1 — Correctness Hardening

### Goal

Ensure verification logic is strictly correct and future-proof.

### Tasks

#### 1. Fix signature index bug (GAP A)

**Problem:**

```rust
let signature = signed_tx.signatures[0]
```

This assumes signer is at index 0 (fee payer).

**Fix:**

```rust
let signature = signed_tx.signatures[signer_index]
```

**Reason:**

- Future multi-signer support
- Correct signer attribution
- Avoid silent mis-reporting

#### 2. Add test: changed blockhash must be rejected

Create test:

- Same instructions
- Same accounts
- Different `recent_blockhash`

**Expected:**

- `message_data()` mismatch
- Verification fails

#### 3. Add test: compute budget tampering must be rejected

Create test:

- Original finalized tx
- Client modifies:
  - Compute unit limit, **OR**
  - Compute unit price, **OR**
  - Inserts extra compute budget instruction

**Expected:**

- `message_data()` mismatch
- Verification fails

### Acceptance Criteria

- [ ] `cargo check` PASS
- [ ] `cargo test` PASS
- [ ] New tests added and passing
- [ ] No change to external APIs
- [ ] No change to pipeline behavior

### Safe Stopping Point

After PASS 1:

- Verification is correct
- System remains fully operational
- **Safe to pause deployment here**

---

## PASS 2 — Security & Observability Hardening

### Goal

Ensure all rejection paths are fully auditable and traceable.

### Tasks

#### 1. Fix missing audit logs on rejection (GAP G)

**Currently:**

- Success path logs to `AuditRepository`
- Rejection paths do **NOT**

**Add audit logs for:**

- Message mismatch rejection
- Wrong signer rejection
- Malformed tx rejection
- Expired request rejection

#### 2. Fix missing transaction_id in SSE (GAP B)

**Currently:**

```rust
transaction_id: Uuid::nil()
```

**Fix:**

- Retrieve `transaction_id` from stored request / proposal
- Propagate it into rejection events

**Result:**

- SSE consumers can correlate rejection to transaction

### Acceptance Criteria

- [ ] `cargo check` PASS
- [ ] `cargo test` PASS
- [ ] Rejection events now appear in:
  - SSE
  - Audit logs
- [ ] No behavior change in success path

### Safe Stopping Point

After PASS 2:

- System is security-auditable
- Suitable for staging / controlled production use

---

## PASS 3 — Operability Hardening

### Goal

Improve developer and operational usability.

### Tasks

#### 1. Resolve `pending_for_session()` ambiguity (GAP E)

**Current:**

```rust
vec![] // TODO
```

**Choose ONE:**

| Option | Description |
|--------|-------------|
| **A** (preferred for V1) | Explicitly document: not supported, endpoint returns empty by design. Rename or annotate API accordingly. |
| **B** | Implement session index, return actual pending requests. |

### Acceptance Criteria

- [ ] `cargo check` PASS
- [ ] `cargo test` PASS
- [ ] Behavior is explicit and documented
- [ ] No silent "fake API" behavior

### Safe Stopping Point

After PASS 3:

- System is developer-friendly
- No hidden incomplete features

---

## Deferred (Do NOT Implement in This Round)

### GAP F — Expiry / Timeout / GC

**Do NOT implement now.**

**Reason:** Requires:

- Background cleanup
- Oneshot lifecycle handling
- Possible race conditions
- Increases patch complexity significantly

**Action:** Document clearly in:

- `HANDOFF.md`
- `PROGRESS.md`

Mark as: *"Deferred to next hardening milestone"*

---

## Test Strategy

### Required Coverage

#### Unit tests

- Signature index correctness
- Message equality checks
- Rejection paths

#### Integration tests

- External signer happy path
- Tampering scenarios
- Human approval + external signer flow

#### Regression tests

- Local signer path unchanged
- Approval flow unchanged
- Spend tracking unchanged

---

## Execution Protocol

### For Claude Code

When executing this plan:

#### Pass 1

```
Implement PASS 1 from EXTERNAL_SIGNER_HARDENING_PLAN.md only.
Do not touch PASS 2 or PASS 3.
After implementation:
- run cargo check
- run cargo test
- stop and report:
  1. files changed
  2. tests added
  3. why change is correct
```

#### Pass 2

```
Proceed with PASS 2 from EXTERNAL_SIGNER_HARDENING_PLAN.md only.
```

#### Pass 3

```
Proceed with PASS 3 from EXTERNAL_SIGNER_HARDENING_PLAN.md only.
```

---

## Finalization Requirements

After all passes, update:

- `HANDOFF.md`
- `PROGRESS.md`
- `TASK.md`

Must include:

- External signer bridge = **COMPLETE**
- Hardening passes completed
- Known deferred items (expiry / GC)

---

## Final State Definition

The system is considered **production-ready (V1)** when:

- [ ] Finalized tx invariant is enforced
- [ ] Exact message matching is correct
- [ ] Signer verification is correct
- [ ] All rejection paths are audited
- [ ] SSE events are traceable
- [ ] No silent incomplete APIs
- [ ] All tests pass

---

## Summary

This plan ensures:

- No regression
- No rushed implementation
- Strict correctness
- Auditability
- Clean evolution toward Phantom / Orca / Solend integration

> **Next step:** `PHANTOM_BRIDGE_ORCA_EXECUTION_PLAN.md` — the layer connecting frontend wallet to DeFi protocol execution.
