# Changelog

## 2026-04-16 — Wire-shape alignment milestone (maintenance mode)

Frontend/live API boundary aligned to Rust wire shape. Key changes:

- **GET /approvals/:id** — direct single-approval endpoint; replaces
  list-then-filter workaround (`cbc271b`)
- **Wire-shape contract tests** — 7 Rust tests guard serde shape
  (bare-string vs tagged conditions/actions, field names, status codes);
  1 Playwright render-contract test guards policy page (`f6c9f85`)
- **3 review findings fixed** — approval→proposal link ID mismatch,
  PolicyCondition bare-string deserialization, TokenTransfer
  source/destination field names (`42f0d1b`, `faaf166`)
- **Phase 4 tests** — proptest fuzzer (found + fixed real u64 overflow
  in `transfer_amount_lamports`), concurrent quorum race tests (`3cfd2bb`)
- **CI + artifact infra** — GitHub Actions workflow, nextest JUnit,
  Playwright reporters, capture-run.sh (`a35648f`, `2854e70`)

**Total test coverage:** 374+ tests across 32+ binaries, 0 failing.

**Status:** Maintenance mode. No new scope until stabilised. Reopen
conditions: CI failure, live-mode bug, or contract drift signal.

**Tech debt filed:** #3 (approvals/:id semantics), #4 (live e2e smoke),
#5 (approval/proposal page contract coverage).
