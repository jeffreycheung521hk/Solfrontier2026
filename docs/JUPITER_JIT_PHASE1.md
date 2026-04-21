# Jupiter JIT Phase 1 — Release Note

**Date**: 2026-04-17  
**Milestone**: Jupiter JIT Phase 1  
**Scope**: Intent-first token swap routing with JIT execution

---

## What Was Added

### Core execution path
- `submit_jupiter_swap` tool: submits a `SwapIntent` (mints, amount, slippage_bps, wallet_pubkey) through the approval control plane
- `HttpJupiterJitExecutor`: builds a V0 `VersionedTransaction` from Jupiter `/swap/v2/build` AFTER approval
- `assemble_v0_transaction`: deterministic assembly of Solana V0 VersionedTransaction with Address Lookup Tables
- V0 signing in `LocalKeypairSigner::sign_versioned()`
- `ExternalWalletAdapter::complete_v0()`: 5-layer verification for externally-signed V0 transactions

### Durable state
- `pending_jupiter_swaps` table (migration `0010_pending_jupiter_swaps.sql`)
- `SwapIntent` — declarative record: no transaction bytes, no blockhash, no ALTs
- `SwapPolicyConfig` — intent-level policy: mint allowlists, amount cap, slippage cap, required role
- `PendingJupiterRepository::expire_stuck()` — startup recovery for mid-flight rows

### State machine
8 states: `pending_approval`, `approved_waiting_jit`, `jit_building`, `jit_build_failed`, `awaiting_wallet_signature`, `submitted`, `rejected`, `expired`

### Security
- `SignatureError::BlockhashModified` — dedicated error variant for wallet-modified blockhash
- `rebuild_required: bool` — surfaces in `POST /wallet-signatures` response
- Strict V0 verification: program ID, instruction data, AND account index arrays are checked
- ed25519 verification over `message.serialize()` (correct V0 signing bytes)

### Config
```toml
[jupiter]
enabled = false  # safe default; set true to activate
base_url = "https://api.jup.ag"

[jupiter.swap_policy]
allowed_input_mints = []
allowed_output_mints = []
max_input_amount = 0
max_slippage_bps = 0
required_approver_role = ""
```

---

## What Was Changed

- `ExternalWalletAdapter::complete()` now detects V0 vs legacy format automatically
- `GatewayDaemon::run()` dispatches to `send_raw_v0_transaction` for V0 signed bytes (previously always used `send_raw_transaction`)
- Daemon startup now calls `expire_stuck()` for `approved_waiting_jit` / `jit_building` rows
- `WalletSignatureOutcome` gains `rebuild_required: bool` (`#[serde(default)]` for wire compatibility)
- Execution agent persona updated with Jupiter workflow

---

## Design Choices (and why)

### Why JIT build instead of pre-built-and-signed?

Solana transactions commit to a `recent_blockhash` with a ~2-minute validity window,
and Jupiter routes/quotes drift within seconds. A human approval gate may take
minutes. If we built and froze the transaction *before* approval, the vast majority
of approved transactions would expire on the blockhash or execute against a stale
route. The only coherent design is to approve a **SwapIntent** (mints, amount,
slippage cap) and build the transaction Just-In-Time *after* approval with a fresh
quote + blockhash. Claw approves an intent, never frozen bytes.

### Why Jupiter does NOT use the `ProgramNotInAllowlist` policy rule

The existing `ProgramNotInAllowlist` rule inspects top-level transaction instructions
and rejects any program ID not on the allowlist. Jupiter routes legitimately dispatch
through many DEX programs (Whirlpool, Raydium, Phoenix, Meteora, etc.) chosen
dynamically per quote. A top-level program allowlist would either block the swap
entirely or require whitelisting the entire DEX surface (which defeats the purpose).

This is a **deliberate design choice**, not a gap. Jupiter's security boundary is
enforced at the **intent layer**: `SwapPolicyConfig` (mint allowlists, amount cap,
slippage cap, required approver role) + the approval workflow + JIT simulation. The
top-level program allowlist remains the correct guard for non-aggregated transfers
and direct protocol interactions.

### Why no Squads / Trigger / Limit Order / `/order` / `/execute` in Phase 1

- **Squads V4 multisig**: a good fit for cold-custody treasury operations, but adds
  a proposal-then-execute latency layer that conflicts with Jupiter's fast-stale
  quote/blockhash window. Phase 1 targets the live-swap hot path via self-custody
  + Claw-controlled execution boundary.
- **Trigger / Limit / scheduled / `/order` / `/execute`**: these introduce a
  background-evaluation surface (price triggers, schedules, recurring execution)
  that is genuinely new scope, not a natural extension of Phase 1's intent-approval
  flow. They are explicitly out of Phase 1.

## What Is NOT Included

| Item | Reason |
|------|--------|
| DCA / `/order` endpoint | Future milestone |
| Trigger-based execution | Future milestone |
| Squads V4 multisig | Not the main path for Phase 1 |
| Phantom blockhash refresh tolerance | Awaiting interop evidence |
| Full HTTP daemon E2E integration tests | Unit + executor layer coverage only |
| Helius enhanced simulation | Future enhancement |

---

## `rebuild_required` Semantics

The `POST /sessions/:id/wallet-signatures` response carries a `rebuild_required: bool`
field that **must** be distinguished from `accepted: false`:

| `accepted` | `rebuild_required` | Meaning | Caller action |
|-----------|-------------------|---------|---------------|
| `true` | `false` | Signature verified and submitted | Nothing — wait for on-chain confirmation |
| `false` | `false` | Generic verification failure (bad signature, tampered instruction, wrong signer) | Do NOT retry with the same bytes. Surface the error to the user |
| `false` | `true` | Wallet modified V0 `recent_blockhash` (strict-mode rejection) | **Retry signal**: call `submit_jupiter_swap` again with the same parameters — a fresh JIT build will produce a new transaction |

**Why strict mode is the default**: In V0+ALT transactions the blockhash also anchors
the ALT resolution slot. Allowing an external signer to substitute the blockhash
lets them alter the effective validity window of an already-approved transaction.
Phase 1 takes the conservative position: any blockhash change is a signal to rebuild
from the approved intent, not to trust the modified bytes.

**Future compatibility mode**: If Phantom (or other wallets) are empirically found
to refresh blockhashes in normal operation, a narrow opt-in mode may be added behind
a config flag (`[jupiter] allow_wallet_blockhash_refresh = false`). This is
explicitly **NOT** the default and requires evidence gathering + a config opt-in
+ audit coverage before landing.

## Feature Scope Summary

| Area | In Phase 1 | Not in Phase 1 |
|------|-----------|----------------|
| Intent submission + policy | ✅ `submit_jupiter_swap`, `SwapPolicyConfig` | — |
| Approval workflow | ✅ Single-stage and multi-stage | — |
| JIT build | ✅ V0 VersionedTransaction + ALTs | — |
| Local signer path | ✅ `LocalWalletAdapter::sign_versioned` | — |
| External wallet path | ✅ `ExternalWalletAdapter::complete_v0` + strict blockhash | Compatibility mode for blockhash refresh |
| Retry signal | ✅ `rebuild_required` on V0 blockhash modification | — |
| Scheduled / recurring swaps | — | Trigger, Limit, DCA, `/order`, `/execute` |
| Multisig custody | — | Squads V4 integration |
| Generic action dispatcher | — | ActionOutcome, backend framework |

## Known Limitations

1. **Strict blockhash mode**: Phantom (and other browser wallets) MUST NOT modify `recent_blockhash` on V0 transactions. If they do, `rebuild_required: true` is returned. No compatibility mode exists yet.

2. **Restart recovery**: `approved_waiting_jit` and `jit_building` rows are expired (not retried) on daemon restart. A fresh `submit_jupiter_swap` request may be needed.

3. **No rebroadcast for Jupiter swaps**: The existing transaction rebroadcast loop covers legacy transfers only. V0 Jupiter swaps submitted via the external wallet path are not currently rebroadcast on drop.

---

## Test Coverage

| Layer | Location | Count |
|-------|----------|-------|
| SwapIntent types + policy | `crates/types/src/swap.rs` | 14 |
| Pending Jupiter repository | `crates/state-store/src/pending_jupiter.rs` | 10 |
| V0 tx assembly | `crates/gateway/src/integrations/jupiter_tx.rs` | 12 |
| Jupiter client + build API | `crates/gateway/src/integrations/jupiter.rs` | ~15 |
| Park store | `crates/gateway/src/integrations/jupiter_park.rs` | 5 |
| JIT executor invariants | `crates/gateway/src/integrations/jupiter_jit.rs` | 12 |
| Production executor + V0 RPC | `crates/gateway/src/integrations/jupiter_production.rs` | 9 |
| SubmitJupiterSwapTool | `crates/gateway/src/tools/jupiter_swap.rs` | 23 |
| External adapter V0 verification | `crates/gateway/src/orchestrator/tests.rs` | 19 |

**Total new tests for Phase 1**: ~120 across all layers

---

## Audit Events Introduced

| Event | When |
|-------|------|
| `wallet_blockhash_modified_rebuild_required` | Phantom changed blockhash |
| `wallet_signature_rejected` | Generic V0 verification failure |
| `wallet_signature_received` | Successful V0 signature |
| `transaction_submitted` | sendTransaction (V0 or legacy) succeeded |
