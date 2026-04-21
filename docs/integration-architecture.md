# Integration Architecture: ClawSolana Jupiter JIT Phase 1

> **Status**: Implemented — Phase 1  
> **Date**: 2026-04-17  
> **Milestone**: Jupiter JIT Phase 1 (wire-shape + external wallet V0)

---

## 1. What Was Built

ClawSolana is an **off-chain policy control plane**. Phase 1 extends it with policy-gated JIT token swap routing via the Jupiter Aggregator.

### Core design principle: intent-first

The approval workflow approves a **SwapIntent** (which tokens, how much, slippage cap), not a frozen transaction. The actual transaction is built Just-In-Time **after** approval using a fresh Jupiter quote and build response. This means:

- The operator sees stable, human-readable parameters: `USDC → SOL, 100 USDC, max 1% slippage`
- The transaction bytes are never stored in the database
- On every JIT build attempt, a fresh blockhash and route are fetched from Jupiter

### Architecture diagram

```
User prompt: "swap 100 USDC to SOL"
         │
         ▼
┌─────────────────────────────────────────────────────┐
│  ClawSolana Gateway (off-chain control plane)        │
│                                                     │
│  1. submit_jupiter_swap tool                        │
│     → Creates SwapIntent (intent_id, mints, amount, │
│       slippage_bps, wallet_pubkey)                  │
│     → Evaluates SwapPolicyConfig                    │
│       (mint allowlists, amount cap, slippage cap)   │
│     → Persists to pending_jupiter_swaps DB          │
│                                                     │
│  2. Approval workflow (if required)                 │
│     → Operator approves via POST /approvals/:id     │
│     → Oneshot signal unblocks resume task           │
│                                                     │
│  3. JIT executor (HttpJupiterJitExecutor)           │
│     → GET /v6/quote (fresh route)                   │
│     → POST /swap/v2/build (fresh tx instructions)  │
│     → Assembles V0 VersionedTransaction with ALTs   │
│     → simulateTransaction (verify before signing)   │
│     → Routes to SignatureOrchestrator               │
│                                                     │
│  4a. Local path (LocalWalletAdapter)                │
│     → Keypair signs V0 tx synchronously             │
│     → sendTransaction (V0) → state: submitted       │
│                                                     │
│  4b. External path (ExternalWalletAdapter)          │
│     → Returns Pending + V0 tx bytes                 │
│     → state: awaiting_wallet_signature              │
│     → User signs with Phantom                       │
│     → POST /wallet-signatures → verified + submitted│
└─────────────────────────────────────────────────────┘
```

---

## 2. State Machine

```
pending_approval
    │ (operator approves)
    ├──► approved_waiting_jit
    │        │ (JIT build starts)
    │        ▼
    │    jit_building
    │        │
    │        ├──► submitted                    (local signer, success)
    │        ├──► awaiting_wallet_signature     (external wallet)
    │        │        │ (Phantom signs + POSTs)
    │        │        └──► submitted           (external wallet, success)
    │        └──► jit_build_failed             (build / simulate / sign error)
    │
    ├──► rejected                              (operator rejects, terminal)
    └──► expired                              (lease timeout or daemon restart, terminal)
```

**Terminal states**: `submitted`, `rejected`, `expired`, `jit_build_failed`

---

## 3. V0 Transaction and ALTs

Jupiter builds transactions using Address Lookup Tables (ALTs) to reduce the static account key count. Claw assembles these into a Solana V0 `VersionedTransaction` using:
- `addresses_by_lookup_table_address` from the build response
- `blockhash_with_metadata.blockhash` as the recent blockhash
- Instruction ordering: `compute_budget` → `setup` → `swap` → `cleanup` → `other`

The ALTs are sorted deterministically by key for reproducible assembly.

---

## 4. Blockhash Strict Mode

**Policy (Phase 1)**: External wallets MUST NOT modify the `recent_blockhash` field of the V0 transaction.

**Rationale**:
- In V0 + ALT transactions, the blockhash anchors the ALT resolution slot
- Allowing the wallet to substitute a different blockhash would let an external party alter the effective execution window of an already-approved transaction
- Phase 1 takes the strict position: any blockhash change is rejected

**Signal**: When a wallet returns a V0 transaction with a modified blockhash, `POST /sessions/:id/wallet-signatures` returns:
```json
{
  "accepted": false,
  "rebuild_required": true,
  "error": "wallet modified blockhash; rebuild required — resubmit a fresh JIT build from the approved intent"
}
```

The caller should re-trigger the JIT build from the original `intent_id`. The intent record is durable and the approval verdict is preserved, so no re-approval is needed (unless the intent has expired).

**Future compatibility mode**: If Phantom is found to always refresh blockhashes in practice, a narrow opt-in mode may be added behind a config flag (`[jupiter] allow_wallet_blockhash_refresh = false`). This is explicitly NOT the default and requires evidence gathering first.

---

## 5. Daemon Restart Recovery

Rows in `approved_waiting_jit` or `jit_building` at daemon restart have no in-memory resume task. On startup, the daemon calls `PendingJupiterRepository::expire_stuck()` to mark these rows `expired` with `last_error = "daemon restart"`.

This is a deliberate conservative policy: rather than attempting a stale JIT rebuild, the system surfaces a clean `expired` status. Callers can re-submit from the original intent parameters if needed.

---

## 6. What Is NOT in Phase 1

| Capability | Status | Notes |
|-----------|--------|-------|
| DCA / recurring swaps (`/order`) | Not started | Future milestone |
| Trigger-based execution | Not started | Future milestone |
| Squads V4 multisig integration | Deferred | Not the main path for Phase 1 |
| Phantom blockhash refresh tolerance | Deferred | Needs Phantom interop evidence first |
| Full HTTP daemon integration tests | Partial | Unit + executor coverage; daemon-level integration deferred |
| Helius enhanced simulation | Not started | Future enhancement |

---

## 7. Config Reference

```toml
[jupiter]
enabled = false         # master switch; false = tool not registered
base_url = "https://api.jup.ag"

[jupiter.swap_policy]
allowed_input_mints  = []    # empty = allow all
allowed_output_mints = []    # empty = allow all
max_input_amount     = 0     # 0 = no cap; in base token units
max_slippage_bps     = 0     # 0 = no cap; 100 = 1%
required_approver_role = ""  # "" = any operator
```

---

## 8. Audit Event Reference

| Event name | Trigger | Severity |
|-----------|---------|---------|
| `wallet_signature_received` | External wallet signature accepted | Info |
| `wallet_signature_rejected` | Verification failed (generic) | Warning |
| `wallet_blockhash_modified_rebuild_required` | Wallet changed blockhash | Warning |
| `transaction_submitted` | sendTransaction succeeded | Info |
| `transaction_send_failed` | sendTransaction RPC error | Warning |
| `tracking_persistence_failed` | Lifecycle tracker DB write failed | Warning |

---

## 9. Known Follow-ups

- [ ] **Phantom interop**: Confirm whether Phantom V0 signTransaction may refresh blockhash in practice (evidence gathering required before relaxing strict mode)
- [ ] **Restart recovery v2**: On startup, consider re-queuing `approved_waiting_jit` rows via a fresh JIT build attempt rather than expiring them (more complex, lower priority)
- [ ] **HTTP E2E tests**: Integration test covering the full HTTP path (`POST /sessions/:id/chat` → `POST /approvals/:id` → `POST /wallet-signatures`) without mocking the daemon handler
- [ ] **Metrics**: Add per-swap-outcome metrics to `MetricsRegistry` (submitted_count, build_failed_count, blockhash_modified_count)
