# Changelog

## 2026-04-17 — Jupiter JIT Phase 1

Intent-first token swap routing with JIT execution wired into the existing approval control plane.

### Added

- **`submit_jupiter_swap` tool** — submits a `SwapIntent` (mints, amount, slippage_bps,
  wallet_pubkey) through policy evaluation and the approval workflow before any transaction
  is built
- **`HttpJupiterJitExecutor`** — builds a V0 `VersionedTransaction` from Jupiter
  `/swap/v2/build` ONLY after operator approval; never before
- **`assemble_v0_transaction`** — deterministic V0 assembly with Address Lookup Tables
  (ALTs sorted by key, instruction order: compute_budget → setup → swap → cleanup → other)
- **V0 signing in `LocalKeypairSigner::sign_versioned()`** — ed25519 over
  `message.serialize()` (correct V0 signing bytes)
- **`ExternalWalletAdapter::complete_v0()`** — 5-layer verification for externally-signed
  V0 transactions (format, ed25519, program IDs, instruction data, account index arrays)
- **`SignatureError::BlockhashModified`** — dedicated error variant; surfaces as
  `rebuild_required: true` in `POST /wallet-signatures` response
- **`pending_jupiter_swaps` table** (migration `0010_pending_jupiter_swaps.sql`) with
  8-state machine: `pending_approval`, `approved_waiting_jit`, `jit_building`,
  `jit_build_failed`, `awaiting_wallet_signature`, `submitted`, `rejected`, `expired`
- **`SwapIntent`** — declarative record: no transaction bytes, no blockhash, no ALTs
- **`SwapPolicyConfig`** — intent-level policy: mint allowlists, amount cap, slippage cap,
  required approver role
- **`PendingJupiterRepository::expire_stuck()`** — startup recovery: rows in
  `approved_waiting_jit`/`jit_building` at restart are expired with
  `last_error = "daemon restart"`
- **`[jupiter]` config section** in `config/default.toml` and
  `config/phantom-e2e.example.toml` with full policy documentation
- **Execution persona updated** — `submit_jupiter_swap` workflow + `rebuild_required`
  handling added to agent SYSTEM_PROMPT

### Changed

- `ExternalWalletAdapter::complete()` auto-detects V0 vs legacy format
- `GatewayDaemon::run()` dispatches to `send_raw_v0_transaction` for V0 signed bytes
- Daemon startup calls `expire_stuck()` for mid-flight Jupiter swap rows
- `WalletSignatureOutcome` gains `rebuild_required: bool` (`#[serde(default)]` for wire
  compatibility)

### Not included in Phase 1

- DCA / `/order` endpoint — future milestone
- Trigger-based execution — future milestone
- Squads V4 multisig integration — deferred
- Phantom blockhash refresh tolerance — awaiting interop evidence
- Full HTTP daemon E2E integration tests — unit + executor layer coverage only
- Helius enhanced simulation — future enhancement

**New tests for Phase 1:** ~120 across all layers (types, state-store, gateway).

---

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
