# Milestone closeout — Wire-shape alignment

**Date:** 2026-04-16
**Status:** Maintenance mode. No new scope.

---

## What was done

Frontend/live API boundary aligned to Rust serde wire shapes. The system
now has three layers of contract tests guarding against drift, a direct
`GET /approvals/:id` endpoint replacing the list-then-filter workaround,
and a real bug fix (u64 overflow in policy engine) discovered by proptest.

## Commit log

| Commit | Summary |
|---|---|
| `a35648f` | Phase 1/2/3 test suite (p1/p2 Rust + Playwright) + CI workflow + capture-run infra |
| `2854e70` | C4: pinned test inventory artifact (368 → now 374 tests) |
| `3cfd2bb` | Phase 4: proptest fuzzer + concurrent quorum tests + **u64 overflow fix** in `transfer_amount_lamports` |
| `42f0d1b` | Fix approval→proposal link ID mismatch + PolicyCondition bare-string normalizer |
| `faaf166` | Fix TokenTransfer field names: `from`/`to` → `source`/`destination` |
| `cbc271b` | Add `GET /approvals/:id` endpoint, remove list-then-filter workaround |
| `f6c9f85` | Wire-shape contract tests (7 Rust) + policy page Playwright render-contract |
| `37bcd2c` | CHANGELOG.md |

## Contracts now locked

See [`TEST_COVERAGE_MAP.md`](./TEST_COVERAGE_MAP.md) for the full
three-layer breakdown. Summary:

- **Rust wire-shape** (7 tests): JSON field names, bare-string vs tagged
  enum serialization, `/approvals/:id` status codes
- **Playwright render-contract** (4 tests): fixture-backed policy page
  renders every condition/action variant without crash
- **Playwright live e2e** (7 tests): real daemon → real Next.js, seeded
  data, graceful degradation, lease countdown

## What is NOT covered

| Gap | Issue | Rationale for deferral |
|---|---|---|
| `/approvals/:id` active-only vs history-capable | [#3](https://github.com/jeffreycheung521hk/Solfrontier1/issues/3) | Design question |
| True live e2e for approval→proposal navigation | [#4](https://github.com/jeffreycheung521hk/Solfrontier1/issues/4) | Pages tested individually; cross-page is lower risk |
| Approval/proposal page render-contract tests | [#5](https://github.com/jeffreycheung521hk/Solfrontier1/issues/5) | Policy page is the highest-risk surface |
| Wallet-adapter mocking, visual regression, RPC chaos | No issue | Requires infra changes; out of scope for maintenance |

## Why maintenance mode now

1. Every contract the review identified as a live-mode boundary risk is
   now locked by at least one test.
2. The proptest-discovered overflow bug (`3cfd2bb`) has been fixed and
   regression-pinned.
3. Adding more tests at this point would have diminishing returns — the
   highest-value boundaries are covered.
4. The project needs observation under real usage, not more construction.

## Reopen conditions

Only reopen scope if one of these signals appears:

1. **CI turns red** — a test that was green at closeout starts failing
2. **Live-mode bug** — a user or developer hits a crash, 500, or
   incorrect data in the frontend when connected to the real daemon
3. **Contract drift** — a Rust serde change causes a wire-shape contract
   test to fail, or a frontend type change causes a Playwright test to
   fail

If none of these signals appear, the milestone stays sealed.

## One-line summary

> Wire-shape alignment + direct approval lookup + three-layer contract
> tests completed 2026-04-16. Maintenance mode until signal.
