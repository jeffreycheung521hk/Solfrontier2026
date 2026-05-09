# Stage 2 — Delegated Conditional Execution Spec

**Status:** Design only. Stage 2 is **not** in Hackathon scope and **not** built. See [`ROADMAP.md`](../ROADMAP.md) Phase 6 for the Hackathon scope (Stage 1 Tail, tamper-evident canonical intent commit). This document specifies the **delegated conditional execution** layer that builds on top of Stage 1 Tail's canonical intent foundation, with the required Stage 2 mitigations integrated.

**Last updated:** 2026-05-09.

**Companions:**
- [`ROADMAP.md`](../ROADMAP.md) — Phase 6 (Stage 1 Tail) and Long-term Vision (Stage 2)
- Stage 2 Canonical Intent + Stage Isolation Spec — in-chat artefact; promoted to a doc post-Hackathon

---

## 1. Executive Summary

Stage 2 introduces **conditional autonomous execution within hard, on-chain bounds**. The user authorises once; a daemon watches conditions; when conditions match, the on-chain program independently re-verifies parameters and a delegated keypair signs and submits — strictly within a pre-committed, on-chain bound contract.

### 1.1 Delegated-Position-Only Model (HARD CONSTRAINT)

The Stage 2 demo MUST NOT guard, manage, read, modify, or withdraw from a user's existing main-wallet Solend obligation. The architecture is **delegated-position-only**:

1. User main wallet transfers a capped amount of USDC + minimum gas SOL to the delegated wallet at authorisation time.
2. Delegated wallet creates and owns a fresh Solend obligation account.
3. Delegated wallet deposits the funded USDC into the named Solend reserve.
4. When the watcher detects a triggering APY (or, for swaps, price) condition, the delegated wallet signs the action ixs.
5. Funds are swept back to the user's main wallet (either by execution path completion or by revoke).

The user's existing positions are **never touched**. The user's main-wallet keys are **never delegated**. The blast radius of a Stage 2 compromise is bounded by the funded amount in the delegated wallet at any moment in time, with the user's main wallet always intact.

### 1.2 Trust Model

Stage 1 Tail provides the tamper-evident intent commit (canonical hash, frontend re-hash, atomic record_intent + action). Stage 2 layers on **strong on-chain action-binding** via:

- **Same-tx pre/post brackets** for Jupiter execution (program reads pre/post balances and enforces caps + min_out before tx commits).
- **Direct delegated-wallet Solend execution** under program-verified authorisation (program is intent custodian; delegated wallet executes within program-enforced bounds).

---

## 2. Demo Scope

### 2.1 In Scope

- Two action types: Solend deposit/withdraw on **delegated obligation only**; Jupiter conditional buy.
- Two condition types: Solend USDC supply APY threshold; Pyth multi-feed price thresholds.
- One authorisation lifetime: 24–72 hours.
- One delegated wallet model: fresh keypair, funded once, swept back on revoke or completion.
- Atomic-tx safety bracket for Jupiter (pre_check + Jupiter ixs + post_check).
- Replay-protected one-shot execution.
- Watcher health surface + Watcher Offline UX.

### 2.2 Out of Scope (Hard Boundaries)

- Guarding / managing / withdrawing user's existing main-wallet Solend obligation. **(See § 5.1.)**
- Borrow / repay / liquidation flows.
- Cross-protocol composition (e.g. Solend → Jupiter in one trigger).
- Health-factor monitoring.
- Multi-position planner / rebalancer.
- Strong CPI / PDA-escrow Jupiter (deferred to v1.5+).
- Withdraw exact arbitrary amount from a larger position. **(See § 5.3.)**
- KMS / HSM for delegated wallet keys.
- Automatic top-up of delegated wallet from user main wallet (manual top-up only).

---

## 3. Authorization PDA

The Authorization PDA stores the user-signed, on-chain bound contract. Layout follows the canonical-intent v2 schema introduced in the Stage 2 Canonical Intent + Stage Isolation Spec, plus the additional fields required for autonomous execution below.

### 3.1 Replay / Completion Protection (Mitigation 6)

The PDA MUST include replay/completion protection. Required fields:

- `completed: bool` — set true on first successful execution.
- `execution_nonce: u64` — fixed nonce; incremented or marked once-spent on execution.
- `last_execution_slot: u64` — for audit visibility and ordering.

#### 3.1.1 One-Shot Demo Rules (v1 Default)

After a successful execution that consumes any non-zero `used_amount_raw`:

- `completed` is set to `true`.
- A second `execute_action_v2` against the same PDA fails with `AlreadyCompleted`.
- The PDA can still be `revoke_authorization`'d (which sweeps any residual delegated-wallet balance back to user) and `close_authorization`'d (which reclaims rent).

#### 3.1.2 Multi-Use Rules (Future, Out of v1 Demo Scope)

For future multi-use rules:

- `execution_nonce` MUST be monotonically increasing across executions.
- `used_amount_raw` MUST strictly increase on each execution.
- Per-trigger cap and daily-count cap are enforced by additional fields not specified here.
- Multi-use is explicitly **not built** in v1.

### 3.2 Other PDA Fields

Refer to the Stage 2 Canonical Intent + Stage Isolation Spec for the full layout: `schema_version`, `intent_id`, `user`, `executor`, `funding_authority`, `valid_from_slot`, `expires_at_slot`, `created_at_slot`, `revoked`, `one_shot`, `condition_logic`, `conditions[]`, `action`, `max_input_amount_raw`, `used_amount_raw`, `destination`, `slippage_bps`, `canonical_intent_hash`, `bump`.

---

## 4. Delegated Wallet & Gas Pre-Funding (Mitigation 5)

The delegated wallet MUST be funded sufficiently at authorisation time to cover all expected execution gas, plus a buffer.

### 4.1 Configuration

All thresholds are **config values**, not hardcoded protocol constants:

```toml
[delegated_wallet.gas_policy]
demo_min_balance_sol  = 0.05    # demo suggested minimum SOL balance
warning_threshold_sol = 0.02    # alert threshold
fee_buffer_sol        = 0.005   # required headroom per execution
```

### 4.2 Invariants

- **Fail closed:** Execution MUST fail closed if delegated wallet SOL balance after planned tx fees would fall below `fee_buffer_sol`. The program / daemon refuses to submit; no partial-success.
- **No silent drain:** Daemon MUST NOT silently top up the delegated wallet from the user's main wallet. Top-ups require an explicit user-signed top-up tx.
- **Alerting:** When delegated balance falls below `warning_threshold_sol`, the watcher emits a UI alert and an audit event. Demo UI shows a "low gas" badge on the authorisation panel.

### 4.3 Top-Up Policy

For the v1 demo, top-up is **manual only**: the user must sign a top-up tx if the delegated wallet runs low. Automatic top-ups are explicitly out of v1 scope.

The top-up policy MUST be explicit and visible in the authorisation creation UI:

> "This delegated wallet will be funded with X USDC and Y SOL. ClawSol cannot add more funds to this wallet without your signature. If gas runs low, you will be alerted and may sign a top-up."

---

## 5. Solend Execution Path (Mitigations 1, 9)

### 5.1 Ownership Boundary (Mitigation 1)

**Hard constraint, restated.** The Stage 2 Solend execution path MUST operate exclusively on a Solend obligation account **owned by the delegated wallet**. The user's main-wallet Solend obligation (if any) is never touched — not read, not modified, not referenced as an input account in any Stage 2 ix.

This boundary is enforced at three layers:

1. **Authorisation layer:** the `target_obligation` field in the action MUST equal the delegated wallet's freshly-created obligation pubkey. The program rejects any authorisation whose `target_obligation.owner` is not the `executor` (delegated wallet).
2. **Program layer:** `execute_solend_action_v2` re-verifies obligation ownership against the executor at execution time. Mismatch → `WrongObligationOwner`.
3. **Watcher layer:** daemon refuses to construct any tx that references an obligation it did not create through the authorisation flow.

### 5.2 Execution Lifecycle

1. **Authorisation time:** user signs `create_authorization_v2`. Same atomic tx transfers `funding_amount_usdc` and `funding_amount_sol` from user main wallet to the delegated wallet.
2. **Position creation:** delegated wallet signs an init-obligation tx and deposits `funding_amount_usdc` into the named Solend reserve. (May be combined into the create-authorisation tx as a follow-on ix in the v1 demo.)
3. **Condition watch:** daemon polls the Solend reserve account; computes APY off-chain using the official Solend rate model.
4. **Trigger:** when off-chain compute predicts the APY condition holds, daemon submits `execute_solend_withdraw_v2`, signed by the delegated wallet (executor).
5. **Program verification:** program checks (a) PDA exists and is not revoked / completed / expired; (b) `executor == authz.executor`; (c) reads Solend reserve account; (d) computes APY using the on-chain formula version pinned in `authz`; (e) re-evaluates the condition.
6. **Withdraw:** delegated wallet signs the Solend withdraw ixs in the same tx; obligation withdraws to delegated USDC ATA.
7. **Sweep:** funds are swept from the delegated USDC ATA to the user's main-wallet USDC ATA — either as a follow-on ix in the same tx or as a separate sweep tx (operator-configurable; default = same tx).

### 5.3 Withdraw Cap Simplification (Mitigation 9)

The demo MUST NOT implement "withdraw exactly N USDC from an arbitrary larger position." Instead:

- The delegated obligation is funded with **at most `max_input_amount_raw`** at authorisation time.
- Withdraw is always **withdraw-all** on the delegated obligation.
- Withdraw-all respects the cap by construction, because the obligation is bounded at the front door.

#### 5.3.1 Rationale

Withdrawing an exact USDC amount from a Solend reserve requires correct conversion between cToken (collateral token) shares and underlying liquidity, factoring exchange-rate drift since deposit. This is non-trivial and a known correctness pitfall (rounding direction, off-by-one between shares and underlying, exchange-rate update timing). The funded-cap-then-withdraw-all model avoids the conversion entirely: we know what we put in, we withdraw everything, the math is whatever Solend says it is.

The user's protection comes from the **cap on what was deposited**, not from a precision assertion at withdraw time.

---

## 6. Jupiter Execution Path (Mitigations 2, 3)

### 6.1 Weak Binding Is Insufficient (Mitigation 2)

For Stage 2 Jupiter execution, weak binding (record-only, no action-payload verification) is **NOT acceptable**. Stage 1 Tail's record_intent is forensic only. Stage 2 must add real-time action-payload verification.

The minimum acceptable mitigation is the **same-transaction pre/post balance bracket**:

```text
ix[0]    ClawSol::pre_check_v2
         - verifies authorisation (PDA exists, not revoked, not completed, not expired)
         - asserts executor == authz.executor (delegated wallet signs)
         - re-evaluates conditions on-chain
         - asserts amount cap and min_out
         - records pre_in_balance and pre_out_balance from token accounts
         - sets pending_check flag

ix[1..N] Jupiter swap instructions (setup, swap, cleanup)
         - normal Jupiter v6 instruction sequence
         - source token account is delegated wallet's input ATA
         - destination is the action.destination_token_account from PDA

ix[N+1]  ClawSol::post_check_v2
         - reads post_in_balance and post_out_balance
         - asserts (pre_in - post_in) <= action.max_input_amount_raw
         - asserts (pre_in - post_in) <= remaining_cap
         - asserts (post_out - pre_out) >= action.min_output_amount_raw
         - asserts destination_account matches PDA destination
         - updates used_amount_raw
         - if one_shot: completed = true, last_execution_slot = Clock.slot
         - clears pending_check flag
```

If any ix in this bracket fails — `pre_check`, any Jupiter ix, or `post_check` — **the entire tx rolls back atomically**. The Jupiter swap does not commit unless all of pre_check, Jupiter, and post_check succeed.

### 6.2 Strong CPI / PDA-Escrow Jupiter Is Deferred

A stronger model — ClawSol CPIs into Jupiter as a PDA signer that owns the input token account — is deferred to v1.5+. The pre/post bracket is the v1 acceptable mitigation. Anything weaker than the bracket is rejected as a Stage 2 design.

### 6.3 Tx Size / ALT Strategy (Mitigation 3)

Jupiter conditional execution MUST measure transaction size and account-key budget **before any live test**. This is an engineering precondition, not a runtime check.

#### 6.3.1 Reporting Requirements (Agent I)

Before live tests, Agent I MUST report:

- **Static account count** — non-ALT accounts referenced in the tx.
- **ALT lookup tables used** — list of address lookup table pubkeys.
- **Serialized v0 tx byte length** — measured size after all signing.
- **Remaining packet margin** — bytes remaining vs the 1232-byte hard cap.
- **Whether pre/post bracket fits** — boolean: does the full 3-ix bracket + Jupiter route + ALT references fit within v0 tx limits?

These measurements are recorded in a `docs/proofs/STAGE2_JUPITER_TX_SIZE_REPORT.md` (post-Hackathon) before any live mainnet rehearsal.

#### 6.3.2 Hard Requirements

- **ALT support is mandatory for Jupiter.** No path that omits ALTs.
- **If too large to fit, fail gracefully.** The conditional Jupiter buy MUST remain **preview-only** (UI-only, no live tx) until size constraints are solved.
- **No degraded-safety fallback.** Any path that ships fewer than the full 3-ix bracket is a violation of this spec. Specifically: it is forbidden to drop `pre_check` or `post_check` to make the tx fit.
- **Document the constraint visibly:** if Jupiter is preview-only because of tx-size constraints, the demo UI MUST display this status and the recorded measurement.

---

## 7. Oracle Freshness & Integrity (Mitigation 8)

### 7.1 Pyth (Stage 2 Multi-Feed Conditions)

Pyth `PriceUpdateV2` consumption MUST enforce:

- **Feed identity verification.** Verify the account's `feed_id` matches the authorisation's recorded feed identifier — not just the account pubkey. (An attacker who controls account creation could in principle hand a same-pubkey account with a different `feed_id`; checking the inner `feed_id` field closes this.)
- **Max publish-time age:** reject if `Clock.unix_timestamp - publish_time > max_publish_time_age_seconds`. Configurable per feed; default 60s.
- **Max confidence/price ratio:** reject if `(conf * 10_000) / abs(price_mantissa) > max_confidence_bps`. Configurable per condition.
- **No floats.** All comparison and normalisation is fixed-point integer math (`i64` / `i128`). No `f32` / `f64` anywhere in the program or in the daemon's parity computation. Float types are forbidden by clippy lint in the relevant crates.
- **Exponent normalisation.** When comparing threshold and price, normalise to a common exponent space; reject on `i128` multiplication overflow rather than wrapping.

### 7.2 Solend Reserve

Solend supply-APY conditions MUST enforce:

- **Reserve pubkey fixed in authorisation.** The reserve account pubkey is part of the canonical intent hash; not parameterised at execute time. An execute attempt against a different reserve fails on the canonical-hash recomputation.
- **APY formula version fixed.** The formula version is part of the schema. If Solend changes the rate model (kink, optimal utilisation, max rate), the schema_version bumps; old authorisations become non-executable until re-authorised by the user.
- **Reserve state from official Solend mainnet source.** The decoder uses Solend's public reserve account layout. No proxy / wrapper / off-chain index trusted; the program reads the reserve account directly.
- **Program/daemon parity tests required.** Off-chain APY computation in the daemon and on-chain APY comparison in the program MUST agree exactly. CI runs both against recorded reserve fixtures and asserts bit-equal results. Parity divergence is a release blocker.

### 7.3 Failure Mode

On any oracle freshness or integrity failure: **fail closed**. No fallback feed. No silent substitution. No "stale-but-close-enough" mode. Daemon retries on next poll; UI flags the rule as "stale oracle, awaiting fresh data" and emits an audit event.

---

## 8. Revoke & Sweep (Mitigation 4)

### 8.1 `revoke_authorization` Behaviour

`revoke_authorization` MUST:

1. **Set `revoked = true`** on the authorisation PDA. Future `execute_*` ixs check `revoked` first and reject with `AuthorizationRevoked`.
2. **Sweep delegated wallet USDC back to user main wallet.** Full balance of the delegated wallet's USDC ATA, signed by program as PDA authority (or by delegated wallet as authority, depending on funds-model decision; see Stage 2 substrate spec § 11).
3. **Sweep excess SOL back to user** according to the configured reserve policy:

```toml
[delegated_wallet.revoke_policy]
reserve_sol_for_close   = 0.002   # leave enough for cleanup tx fees
sweep_remainder_sol     = true    # sweep all SOL above reserve to user
close_token_accounts    = true    # close empty USDC ATA after sweep, return rent
```

4. **Make future execute fail-closed via `revoked` flag.**
5. **Make concurrent execute fail-closed if sweep lands first.** If the sweep tx commits before a concurrent execute tx in slot ordering, the delegated wallet has insufficient funds; the execute fails on the underlying token transfer (insufficient balance), not silently. The execute does not fall back to a partial action.

### 8.2 Race Window — Honest Statement

If `execute` lands before `revoke` in chain transaction ordering, **revoke cannot undo the executed action**. Ordering is determined by leader transaction order; we have no mechanism to reverse a finalised tx.

This is acknowledged as a residual risk:

- **Bound:** maximum loss per race is `max_input_amount_raw` (per-trigger cap). The user's main wallet is unaffected.
- **Mitigation:** users are warned in the revoke UI that *"in-flight executions may complete before revoke; the revoke prevents future executions only and recovers any unused delegated funds."*
- **Audit:** all races are visible in the on-chain audit trail and `last_execution_slot` for forensic review.
- **Mainnet rehearsal:** revoke-vs-execute race must be exercised on devnet with synthetic timing before any mainnet authorisation cycle.

---

## 9. Daemon Health & Alerting (Mitigation 7)

### 9.1 Watcher Health Surface

The watcher MUST expose health state via an API endpoint and a frontend UI panel:

```text
GET /watcher/health

{
  "last_successful_tick_at": "2026-05-09T03:14:22Z",
  "last_error": null | { "rule_id": "...", "error": "...", "at": "..." },
  "per_rule_status": [
    {
      "rule_id": "abc123",
      "last_evaluated_at": "...",
      "last_evaluation_result": "false" | "true" | "stale_oracle" | "error",
      "current_state": "watching" | "ready" | "stale" | "error"
    }
  ]
}
```

The endpoint is read-only; it exposes the watcher's view of the world without permitting any state mutation.

### 9.2 Frontend "Watcher Offline" State

If `(now - last_successful_tick_at) > 5 minutes`:

- Frontend displays a prominent **"Watcher Offline"** banner on every authorisation panel.
- Banner offers two actions:
  1. **Revoke + Sweep** — immediate revocation and fund recovery.
  2. **Wait + Retry** — user explicitly chooses to wait for daemon recovery, with a visible counter showing time since last successful tick.
- Default action highlight is "Revoke + Sweep" (UX prefers safety over availability).

### 9.3 Force-Tick (Dev / Demo Only)

A `POST /watcher/force_tick` endpoint exists for development and demo recording purposes:

- **Compile-time gated** behind `#[cfg(feature = "demo-tools")]`.
- **MUST NOT** ship in production builds. Production binaries do not include the route handler at all — not behind an env flag, not behind a runtime check.
- Devnet-only. Production frontend has no UI affordance for force-tick.
- Documented in this spec so future operators know the mechanism exists in dev builds and to flag any accidental production exposure as a release-blocker.

---

## 10. Cross-Cutting Invariants

The following invariants apply across all Stage 2 paths:

- **No floats anywhere on-chain.** Fixed-point integer math only. Enforced by clippy.
- **No degraded-safety fallback.** If a safety check cannot run, the action fails closed. Never silently drop checks to make a tx fit, an oracle work, or a budget hold.
- **No silent automatic top-up of delegated wallet** from user main wallet. Manual user signature required for any top-up.
- **No reading or writing of user's existing main-wallet Solend obligation.** Delegated-position-only.
- **No execute path bypasses the program.** Even the daemon submits via the program ix; there is no "trusted daemon shortcut."
- **All thresholds and policy values are config**, not hardcoded constants embedded in program logic. The bound values themselves are committed in the canonical intent hash; only the *mechanism* lives in code.

---

## 11. Implementation Phasing

| Phase | Deliverable |
|---|---|
| S0 | Stage 1 Tail mainnet promotion (record_intent program, atomic tx rehearsals) — prerequisite for Stage 2 |
| S1 | Authorization PDA v2 schema (with replay-protection fields) on devnet |
| S2 | Delegated-wallet onboarding flow (controlled wallet for demo) + gas pre-funding |
| S3 | Solend delegated-position execution (deposit + withdraw-all under cap) on devnet |
| S4 | Pyth multi-feed conditions + on-chain re-verify on devnet |
| S5 | Jupiter pre/post bracket on devnet, with Agent I tx-size report attached |
| S6 | Revoke + sweep with race-window rehearsal on devnet |
| S7 | Watcher health surface + Watcher Offline UX |
| S8 | End-to-end devnet rehearsal: full Alice scenario with controlled wallet |
| S9 | Mainnet rehearsal under bound contract — single deliberate run after audit-style review |

S0–S6 are sequential; S7 can run in parallel with S5–S6. S8 is a gate before S9.

---

## 12. Out of Scope (Explicit Re-statement)

For clarity and to prevent scope creep, the following are explicitly **NOT** in this spec and MUST NOT be implemented as part of v1 delegated conditional execution:

- Guarding or executing on the user's existing main-wallet Solend obligation.
- Borrow, repay, or liquidation flows on Solend.
- Multi-protocol composition (e.g. Solend → Jupiter chained in one trigger).
- Strong CPI Jupiter (PDA escrow signs Jupiter route directly) — deferred to v1.5+.
- Withdraw of an exact arbitrary amount from an arbitrarily larger position.
- Health-factor or net-exposure monitoring.
- Cross-position rebalancer.
- KMS / HSM for delegated wallet keys.
- Hardware-wallet-signed delegation flows.
- Automatic top-ups of delegated wallet without user signature.
- Anything that would let a compromised daemon or backend cause loss exceeding the delegated-wallet capped balance at the time of compromise.

---

## 13. Open Questions

Honest open items requiring live-rehearsal review or further design:

1. **Pre/post bracket tx-size feasibility.** Requires Agent I measurement (Mitigation 3). If the bracket does not fit, Jupiter conditional buy is preview-only until route or ALT strategy changes. No safety-degraded fallback is acceptable.
2. **Sweep-tx fee estimation.** Must include enough SOL reserve for two sweep operations (USDC ATA close + remaining SOL transfer). Reserve value lives in `revoke_policy.reserve_sol_for_close` and must be tuned against measured fees.
3. **Solend APY parity.** Off-chain (daemon) and on-chain (program) APY computation must match exactly. Floating-point avoidance and rounding-mode parity are real footguns. CI test required against recorded reserve fixtures.
4. **Pyth feed sponsorship continuity.** If a sponsored receiver lapses during a 48-hour authorisation, the watcher fails closed. Acceptable for v1; flagged for monitoring.
5. **Delegated-wallet onboarding UX for real users.** v1 demo uses a controlled wallet; the user-side flow for a real third-party Alice (consent / disclosure / recovery design) is post-v1 work.
6. **Replay-protection edge case.** If `execute_action` succeeds on-chain but the daemon crashes before recording success off-chain, the next daemon run sees `completed = true` and refuses; this is correct on-chain but operator UX needs a "verify on-chain status" affordance to avoid confusion.
7. **Race-window operator messaging.** The revoke vs in-flight-execute race window message should be reviewed by a non-technical user before demo recording.
8. **Funds custody model.** Stage 2 substrate spec recommends PDA escrow for funds (program signs as PDA authority); this spec assumes that decision but the final call lives in the substrate spec § 11.
9. **Solend collateral-token vs liquidity-token rounding.** § 5.3 sidesteps this by withdrawing all; if a future variant introduces partial withdraw, this becomes a first-class correctness concern.

---

## 14. Document Status

Stage 2 remains **design only** at the time of this writing. No live Stage 2 transactions have been executed on devnet or mainnet. Stage 1 Tail (canonical intent commit, tamper-evident, present-tense execution) is the Hackathon delivery; Stage 2 is the post-Hackathon credibility track.

This spec integrates the nine required mitigations as primary content. Each mitigation is referenced by number (e.g. "Mitigation 6") in the relevant section heading.

| Mitigation | Section |
|---|---|
| 1. Solend obligation ownership boundary | § 1.1, § 2.2, § 5.1, § 12 |
| 2. Jupiter weak-binding mitigation (pre/post bracket) | § 6.1 |
| 3. Tx size / ALT strategy | § 6.3 |
| 4. Revoke + sweep race mitigation | § 8 |
| 5. Gas pre-funding / execution reliability | § 4 |
| 6. Replay protection | § 3.1 |
| 7. Daemon health / alerting | § 9 |
| 8. Oracle freshness / integrity | § 7 |
| 9. Withdraw cap simplification | § 5.3 |

---

**End of spec.**
