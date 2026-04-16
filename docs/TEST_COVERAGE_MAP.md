# Test coverage map

What each test layer proves, what it does NOT prove, and where it lives.
This document exists to prevent the most common misread: confusing
**render contract** with **live e2e**, or confusing **wire-shape contract**
with **full-pipeline integration**.

---

## Layer 1 — Rust wire-shape contract

**File:** `crates/gateway/tests/p5_wire_shape_contracts.rs` (7 tests)

**What it proves:** The exact JSON that `GET /policy/rules` and
`GET /approvals/:id` return matches the field names, types, and shapes
the TypeScript frontend depends on.

| Test | Contract guarded |
|---|---|
| `policy_condition_unit_variants_are_bare_strings` | `ProgramNotInAllowlist`, `DestinationInDenylist`, `SimulationNotPassed`, `LegacyTokenTransferPresent`, `Always` serialize as bare JSON strings — NOT `{ "type": "..." }` objects |
| `policy_condition_tagged_variants_have_correct_field_names` | `AmountExceedsLamports`, `CostExceedsSol`, `DailySpendExceedsSol` use `threshold` (not `sol`); `TokenAmountExceeds` has `mint` + `threshold`; `MintNotInAllowlist` has `allowed_mints` |
| `policy_action_approve_is_bare_string` | `PolicyAction::Approve` serializes as `"Approve"`, not `{ "type": "Approve" }` |
| `policy_action_tagged_variants_have_correct_fields` | `RequireHumanApproval` has `type` + `reason` + `required_approver_role`; `RequireApprovalChain` has `type` + `reason` + `stages[].{role, description, min_approvals}`; `Reject` has `type` + `reason` |
| `get_approval_by_id_returns_request_id_and_workflow` | `request.id` matches the URL parameter; `request.id ≠ request.transaction_id`; `workflow.state` and `workflow.stages` are present |
| `get_approval_returns_404_for_unknown_id` | Unknown UUID → HTTP 404 |
| `get_approval_returns_400_for_bad_id` | Non-UUID string → HTTP 400 |

**What it does NOT prove:**
- That the frontend can *render* these shapes without crashing (→ Layer 2)
- That a real daemon + real policy config produces these shapes (→ Layer 3)
- Any full-pipeline behavior (simulate → policy → sign)

---

## Layer 2 — Playwright showcase render contract

**File:** `frontend/tests/e2e/dashboard.showcase.spec.ts` (4 tests)

**What it proves:** The Next.js frontend, given fixture-shaped data
(matching the wire shapes from Layer 1), renders every page without
crashing and produces recognisable UI text.

| Test | Contract guarded |
|---|---|
| `dashboard renders from fixtures with showcase badge` | Dashboard page mounts, SHOWCASE badge visible, stat cards present |
| `policy page renders fixture rules` | Policy heading visible, rule name `usdc-high-value-chain` appears |
| `policy page renders all condition + action variants from fixtures` | Unit condition (`LegacyTokenTransferPresent` → "legacy SPL Token Transfer detected"), tagged condition (`TokenAmountExceeds` → "USDC transfer"), unit action (`Approve` → "approve" badge), tagged actions (`RequireApprovalChain` → "chain" badge + risk/treasury/cfo roles; `RequireHumanApproval` → "human" badge; `Reject` → "reject" badge) |
| `audit page renders fixture events` | Audit table shows `human_approved` and `approval_lease_expired` |

**What it does NOT prove:**
- That the live gateway returns these shapes (→ Layer 1 + Layer 3)
- That navigation between pages works with real IDs (→ Layer 3)
- This is **not** live e2e. The showcase project hits the fixture-backed
  Next instance on `:3001`. No daemon involved.

---

## Layer 3 — Playwright live smoke / live e2e

**File:** `frontend/tests/e2e/dashboard.live.spec.ts` (7 tests)

**What it proves:** The Next.js frontend, connected to the **real daemon**
on `:3000` via the gateway on `:7070`, renders seeded live data correctly.
These are genuine end-to-end tests through the full stack.

| Test | Contract guarded |
|---|---|
| `case2` | LIVE badge visible, seeded wallet pubkey appears |
| `case3` | Dashboard stat cards match seed counts (2 pending, 1 wallet, multi-stage label) |
| `case4` | Proposal page with real `request.id` — graceful degradation when full proposal blob is absent |
| `case5` | Approval chain page with real chain workflow — risk/treasury stage names visible |
| `case6` | Lease countdown ticks down on a freshly-seeded workflow |
| `case7` | Audit page surfaces all three seeded event types |
| `case8` | Policy page shows rules in config order + per-wallet tab |

**What it does NOT prove:**
- Wire-shape exactness at the JSON level (→ Layer 1)
- Render of condition/action variants not present in the seed config
  (→ Layer 2's fixture-based coverage is broader)
- The full proposal blob (instructions, token transfer details) in live
  mode — the gateway does not yet expose this via `GET /approvals/:id`

---

## What is NOT covered (tech debt)

| Gap | Issue | Why not now |
|---|---|---|
| `/approvals/:id` returns only active approvals; decided ones 404 | [#3](https://github.com/jeffreycheung521hk/Solfrontier1/issues/3) | Design question, not a bug |
| True live e2e covering approval→proposal navigation with `fetchApproval` direct endpoint | [#4](https://github.com/jeffreycheung521hk/Solfrontier1/issues/4) | case4/case5 cover the pages individually; cross-page navigation contract is lower risk |
| Render-contract tests for approval chain page and proposal review page | [#5](https://github.com/jeffreycheung521hk/Solfrontier1/issues/5) | Policy page is the highest-risk render surface (most enum variants); approval/proposal are simpler |
| Wallet-adapter mocking (Phantom sign flow) | — | CI cannot run browser extensions; manual-only for now |
| Visual regression / screenshot tests | — | Text-based selectors are sufficient for contract drift; pixel tests add maintenance cost |
| Toxiproxy / RPC chaos | — | Requires infra not present in the test harness |
