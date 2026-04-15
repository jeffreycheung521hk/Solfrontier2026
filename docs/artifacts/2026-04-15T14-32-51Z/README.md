# Test inventory capture (P3 concurrent + P4 proptest additions)

- Timestamp (UTC): 2026-04-15T14-32-51Z
- Source: `cargo test -- --list --format terse` across the 5 workspace
  packages.
- Total tests: **374** across 32 binaries.
- Delta vs [`2026-04-15T08-45-59Z/`](../2026-04-15T08-45-59Z/): **+6 tests**
  from two new integration test files.

## What changed

1. [`crates/risk-engine/tests/p4_policy_proptest.rs`](../../../crates/risk-engine/tests/p4_policy_proptest.rs)
   — 3 property-based tests (proptest, 256+128+128 cases).
   - `evaluate_never_panics` — no input causes PolicySet::evaluate to panic
   - `first_match_wins` — matched_rule_index is always the earliest matching rule
   - `human_approval_verdict_has_legitimate_origin` — RequiresHumanApproval
     verdict must come from a RequireHumanApproval action or the fail-closed default
2. [`crates/gateway/tests/p3_quorum_concurrent.rs`](../../../crates/gateway/tests/p3_quorum_concurrent.rs)
   — 3 multi-threaded `tokio` tests hammering `ApprovalStore::decide`:
   - 3-of-5 quorum race → exactly 3 progress, 2 already_decided, state Approved
   - Same-op-id spam → 1 accepted, 4 DuplicateOperator, state Pending
   - Concurrent approve + reject × 16 iterations → never observed Approved,
     always Rejected or Pending

## Bug fixed along the way

The proptest **evaluate_never_panics** caught an integer overflow in
[`crates/risk-engine/src/policy.rs`](../../../crates/risk-engine/src/policy.rs)
at `transfer_amount_lamports`: summing multiple huge `transfer_lamports`
values with `.sum()` would panic in debug builds on u64 overflow. Fixed
by switching to `fold(0, saturating_add)` — semantically correct because
an overflowing total still exceeds any sane `AmountExceedsLamports`
threshold.

## How to regenerate

See [`../2026-04-15T08-45-59Z/README.md`](../2026-04-15T08-45-59Z/README.md)
for the per-binary enumeration procedure. No changes to the procedure —
running it today just picks up the two new test binaries.
