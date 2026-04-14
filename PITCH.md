# ClawSolana — Policy-Driven Approval Control Plane for Solana

**Positioning**: Turn every Solana transaction into an auditable, governable, enterprise-grade asset action.

---

## The problem today

Treasury and protocol teams operating on Solana face three structural gaps:

- **Multisigs are blunt.** They authorize *who* signs, not *what* gets signed. A 3-of-5 multisig can't say "USDC over 50k needs treasury + CFO; SOL transfers under 1k auto-approve." Approvers either rubber-stamp or block everything.
- **Policy is token-blind.** Existing tooling reads lamports, not SPL semantics. A legacy `Transfer` (no mint in the instruction) silently bypasses any rule that depends on knowing *which* token is moving — a long-standing blind spot for USDC controls.
- **Approval flow has no SLA.** Requests sit in queues with no enforced expiry, no terminal-state audit, no distinction between "rejected" and "the approver went to lunch." Compliance teams can't reconstruct what actually happened.

The result: institutional capital stays on Ethereum or in custodians, because Solana lacks a policy layer their auditors will sign off on.

---

## What ClawSolana provides

### 1. A policy layer purpose-built for SPL token semantics
Rules can target specific mints, set per-token thresholds, enforce mint allowlists, and reject legacy `Transfer` instructions outright so policy always sees the mint.
**Business consequence**: prevents blind-token transfers; gives USDC and other stablecoin issuers a control surface that actually understands their asset.

### 2. Multi-stage approval chains with quorum
Rules route to risk → treasury → CFO with M-of-N quorum at each stage and operator deduplication. First-match-wins ordering is regression-locked.
**Business consequence**: reduces approval bottlenecks; humans only touch what policy flags, not every transaction.

### 3. Lease-enforced approval
Every request carries a TTL that's actively enforced. Expiry is its own terminal state with its own audit trail, distinct from rejection.
**Business consequence**: eliminates the "stuck ticket" failure mode and the "fired at 3am" failure mode in a single mechanism. Compliance gets a clean answer for every request.

### 4. Durable state and audit
Pending approvals and workflows recover across restarts. Every decision, expiry, and rejection produces an audit row.
**Business consequence**: creates audit evidence suitable for institutional treasury ops, SOC2 review, and regulatory reporting.

### 5. External wallet flow with simulation gating
Phantom-style propose → simulate → approve → sign. Failed simulations never enter the queue.
**Business consequence**: approvers never waste cycles on transactions that would have reverted on-chain.

---

## Why Solana / USDC / Solend should sponsor

| Sponsor | Business consequence we deliver |
|---|---|
| **Solana Foundation** | A policy/approval layer institutional treasuries can underwrite — a rail for capital that currently stays off Solana |
| **Circle / USDC** | USDC-aware controls with the legacy-Transfer blind spot closed — directly addressable in compliance conversations with issuers and merchants |
| **Solend** | Vault and treasury ops with multi-approver routing that doesn't bottleneck operations; lease-enforced SLAs on every approval |
| **Jito / Marinade / any LST** | Audit-grade evidence and separation of duties for validator commission and treasury rebalance |

---

## One-line pitch

> **"Stripe Radar for Solana treasury operations — policy-first, not ML scoring. You own the rules, the keys, and the audit trail."**

---

## Technical appendix

For reviewers who want to verify the claims:
- Lease enforcement: `tokio::time::timeout` in `resume_after_approval`, lease seconds threaded from `config.policy.approval_lease_seconds`
- Token decoding: `is_legacy_token_transfer` flag set during SPL Token instruction summarization; `LegacyTokenTransferPresent` is the first guard rule
- Durable layer: SQLite via sqlx + migrations; multi-stage workflows reconstructed from persisted verdict's `approval_chain`. The event bus itself is in-process broadcast (no durable replay — durability lives in SQLite)
- Per-wallet policy: config-time policy store, not runtime / on-chain dynamic update
- Test coverage: 459 passing, regression tests for rule ordering, lease enforcement, legacy transfer bypass, expired audit
