# ClawSolana — Session Handoff

**Last updated:** 2026-03-21
**State:** `cargo check` PASS, `cargo test` PASS (200+ tests, zero failures)
**Current phase:** Entering Phase 1 — Execution Completeness
**Product positioning:** Policy-gated transaction control plane for Solana

---

## Current State Summary

ClawSolana is a transaction control plane, not just a signing tool. The core control plane — typestate pipeline, policy enforcement, capability boundaries, durable pending lifecycle, audit trail — is implemented and tested. What remains is completing the **execution path**: real wallet connectivity, on-chain submission, and confirmation tracking.

**Why Phantom integration is critical now:** The control plane's value proposition is that it sits between intent sources and signing authorities. Without a real signing authority connected, the system is a validated control plane with no downstream path. Phantom integration transforms ClawSolana from "policy engine with simulated signing" into "policy engine governing real wallet operations." This is the single highest-value step to prove the architecture works end-to-end.

### Completed Beyond N1–N8

| Work | Session | Status |
|------|---------|--------|
| Signature Orchestrator skeleton | 13 | Live — adapter dispatch, pending lifecycle, exactly-once |
| STEP 5: Orchestrator integration | 13+ | SubmitForSigningTool + resume + handler routed through orchestrator |
| STEP 6: Orchestrator integration tests | 13+ | 13 tests — local/external × auto/human, race, expiry |
| P0-1: Atomic completion | 13+ | Resolved via `DashMap::remove()` consume-on-attempt |
| P0-2: Request expiry reaper | 13+ | 30s interval, 120s TTL, `WalletSignatureExpired` events |
| P1-1: Durable pending state | 13+ | SQLite lifecycle state (`pending`/`consumed`/`expired`/`rejected`) |
| Durable-first create semantics | 13+ | No `request_id` exposed without durable backing |
| Atomic signature+meta persist | 13+ | `persist_signature_and_meta()` single SQLite transaction |
| `abort_pending()` rollback | 13+ | Distinct from natural `expire()` — persistence failure cleanup |
| Terminal row purge | 13+ | Startup + periodic (6h default), 14-day configurable retention |
| N1–N8 foundation audit | 13+ | 38 targeted tests, no regressions found |

### Current Invariants

1. **No `request_id` without durable backing** — any externally visible pending state has SQLite row
2. **Durable-first create** — DB write succeeds BEFORE in-memory state + response
3. **Mark-terminal-before-remove** — DB row marked terminal BEFORE in-memory removal
4. **Consume-on-attempt** — `complete()` atomically removes pending entry before verification
5. **`abort_pending()` ≠ `expire()`** — rollback cleanup vs natural TTL, observably distinct
6. **Fail-closed on persist failure** — no in-memory-only downgrade on durable write failure
7. **Human signs, AI proposes** — no agent role grants `SignTransaction` or `SendTransaction`

### Known Open Gaps

| Gap | Priority | Notes |
|-----|----------|-------|
| **P0-3:** Session-request binding | High | Submit endpoint doesn't verify session ownership |
| **P1-2:** Rate limiting | Medium | No rate limiting on API endpoints |
| **P1-3:** Wallet binding ownership proof | High | `bind_wallet` accepts bare pubkey, no challenge-response |
| **P1-4:** Durable event log | Low | Events are fire-and-forget broadcast |
| **N12:** Context trimming pairs | Medium | `trim_if_needed()` can orphan tool_use without tool_result |
| **N14:** On-chain submission | High | `sendTransaction` + confirmation tracking not implemented |
| **External wallet E2E** | High | Verification layer done; browser bridge not finished |

---

## Recommended Next Engineering Path

### Phase A — External Wallet Connectivity (highest value)

1. **Wallet ownership proof** — challenge-response for `bind_wallet`
   - `POST /sessions/:id/wallet-bind-challenge` → nonce
   - `POST /sessions/:id/wallet-bind-confirm` → pubkey + signature + nonce
   - Server ed25519 verify before binding

2. **Phantom browser bridge** — minimal client page
   - Connect wallet, pull pending requests, signTransaction, POST back
   - Phantom only, no WalletConnect

3. **External signer formalization** — `SignerType::External` from deferred to live
   - Config declares external wallet, routing works end-to-end

4. **End-to-end connectivity test** — session → bind → propose → approve → sign → verify

### Phase B — On-Chain Execution

5. **sendTransaction** — submit verified signed tx to Solana RPC
6. **Confirmation tracking** — poll/websocket, status: submitted → confirmed → finalized
7. **Basic retry policy** — same signed bytes rebroadcast, max retries, blockhash expired = fail

### Phase C — Hardening

8. Rate limiting / abuse controls
9. Context trimming fix (tool_use/tool_result pair-safe)
10. Metrics / operational visibility
11. `bind_wallet` persistence (if needed for session continuity)

---

## Architecture Quick Reference

```
Control Plane                              Execution Plane
─────────────                              ───────────────
SubmitForSigningTool                       SignatureOrchestrator
  → simulate → policy → approval            → adapters: [external, local]
  → build SignatureRequest                   → pending: DashMap (lifecycle-tracked)
  → durable persist FIRST                    → submit() / complete() / expire()
  → then in-memory + response                → abort_pending() for rollback

DurablePendingState (SQLite)               Adapters
  → pending_approvals table                  → LocalWalletAdapter (sync sign)
  → pending_signatures table                 → ExternalWalletAdapter (async pending)
  → pending_completion_meta table
  → lifecycle: pending → consumed/expired/rejected
```

---

## To Resume Next Session

```bash
cd /path/to/ClawSolana
cargo check          # should be green
cargo test           # 200+ tests, all green
cat TASK.md          # current priorities
cat ROADMAP.md       # Phase A/B/C path
```
