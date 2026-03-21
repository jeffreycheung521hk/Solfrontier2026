# ClawSolana — Roadmap

**Product:** Policy-gated transaction control plane for Solana.

**Created:** 2026-03-19 | **Updated:** 2026-03-21
**Baseline:** Session 15 complete (218 tests, zero failures, durable-first orchestration + wallet ownership proof + Phantom bridge)
**Companion:** [ARCHITECTURE_CONTROL.md](ARCHITECTURE_CONTROL.md) — system invariants, plane model, failure modes

---

## Core Control Plane Phases

The control plane is the product core. These phases define the progression from prototype to production infrastructure.

### Phase 0 — Production Readiness & V1 Foundation *(near complete)*

**Goal:** Stabilize the control plane for safe devnet/testnet operation.

| Item | Status |
|------|--------|
| P0-1: Atomic completion (`DashMap::remove()` consume-on-attempt) | ✅ Done |
| P0-2: Request expiry reaper (30s interval, 120s TTL) | ✅ Done |
| P1-1: Durable pending state (SQLite lifecycle, startup recovery, durable-first) | ✅ Done |
| P1-1 hardening: mark-terminal-before-remove, `abort_pending()`, purge | ✅ Done |
| N1–N8 foundation audit (200+ tests) | ✅ Done |
| P0-3: Session-request binding check | Open |
| P1-2: Rate limiting | Open |
| P1-3: `bind_wallet` challenge-response | ✅ Done |
| N12: Context trimming (tool_use/tool_result pairs) | Open |

**Exit Criteria:** All P0 fixed, durable pending state operational, 200+ tests passing.

---

### Phase 1 — Execution Completeness *(immediate next)*

> The control plane is validated. The highest-value next step is completing the execution path. Without real wallet connectivity and on-chain submission, the control plane governs nothing downstream.

#### 1.1 — Wallet Ownership Proof ✅
- [x] Challenge-response for `bind_wallet` — `wallet_challenge.rs` + 9 tests
- [x] `POST /sessions/:id/wallet-bind-challenge` → server generates session-bound nonce
- [x] `POST /sessions/:id/wallet-bind-confirm` → pubkey + signature + nonce
- [x] Server ed25519 verifies before binding
- [x] Persist challenge state (session_id, nonce, expires_at, consumed)
- [x] Cross-session hijack protection (session_id verified on confirm)

#### 1.2 — Phantom Browser Bridge ✅
- [x] Minimal HTML/JS page — `bridge/index.html`
- [x] Connect Phantom → bind via challenge-response → signTransaction → POST back
- [x] Phantom only; no WalletConnect
- [x] CORS enabled on API server for local bridge

#### 1.3 — External Signer Formalization
- [ ] `SignerType::External` from deferred to live in daemon config
- [ ] Signing tool routes to external pending path for external wallets
- [ ] Session binding + orchestrator routing works end-to-end

#### 1.4 — On-Chain Submission
- [ ] `sendTransaction` after verification passes
- [ ] Confirmation tracking: submitted → confirmed → finalized / failed
- [ ] Basic retry: same signed bytes, max retries, blockhash expired = fail

#### 1.5 — Operational Safety
- [ ] Rate limiting (per token, per session, 429 response)
- [ ] Session-request binding (P0-3)
- [ ] Context trimming fix (tool_use/tool_result pair-safe)

#### 1.6 — End-to-End Test
- [ ] Session → bind (with challenge) → propose → approve → external sign → verify → submit → confirmed on devnet

**Exit Criteria:** A real browser wallet connects, signs a pending transaction, and it lands on devnet with confirmation tracking.

---

### Phase 2 — Policy Engine Expansion

> **Prerequisite:** Phase 1 complete. Without real execution, policy rules cannot be validated end-to-end.

**Goal:** Evolve the policy layer from "approve / require-human / block" into a configurable, composable rule engine for organizational transaction governance.

#### 2.1 — Granular Policy Rules
- [ ] Per-session policy overrides
- [ ] Destination allowlist / denylist (program IDs, wallets)
- [ ] Amount thresholds (auto-approve below X, escalate above Y)
- [ ] Program-level restrictions

#### 2.2 — Approval Matrix
- [ ] Multi-role approval (agent → risk officer → treasury admin)
- [ ] Conditional routing (amount > threshold → escalate)
- [ ] Quorum-based approval for high-value transactions

#### 2.3 — Signer Constraints
- [ ] Per-wallet signing policies
- [ ] Signing session time-bounds

#### 2.4 — Policy Observability
- [ ] Policy evaluation audit trail (which rules fired, why)
- [ ] Policy violation alerting

**Exit Criteria:** Operators can configure policy rules (spend limits, program allowlists, approval escalation) without code changes.

---

### Phase 3 — Enterprise Control Plane

**Goal:** Scale from single-operator to organizational treasury and multi-signer workflows.

#### 3.1 — Multi-Signer Orchestration
- [ ] Transaction routing to different signers based on policy
- [ ] Sequential multi-sig support

#### 3.2 — Enterprise Signer Integration
- [ ] MPC signer adapters (Fireblocks, Fordefi, Turnkey)
- [ ] Hardware wallet support (Ledger)

#### 3.3 — Treasury Workflows
- [ ] Budget allocation per agent / team
- [ ] Organizational approval hierarchy
- [ ] Periodic reporting and reconciliation

#### 3.4 — Programmable Policy Engine
- [ ] Policy-as-code (configuration DSL)
- [ ] Policy versioning and rollback
- [ ] Policy simulation

#### 3.5 — Agent-Safe Execution Environment
- [ ] Sandboxed agent sessions with resource limits
- [ ] Emergency kill switch

**Exit Criteria:** Multiple AI agents across multiple wallets, with per-agent policy boundaries, multi-level approval, and enterprise signer integration.

---

## Execution Adapters & Protocol Integrations

> Execution adapters are downstream integrations that operate **under** the control plane. They produce `TransactionProposal`s that enter the pipeline. They are not part of the core phase progression — they are applications built on top of the control plane.

### Orca Whirlpools (first protocol)

**Current:** Read-only tools (`orca_get_whirlpool`, `orca_get_quote`, `orca_check_pool`) — live.

**Planned:**
- [ ] `DeFiAction` trait + `ProtocolAdapter` abstraction
- [ ] `orca_swap` — build swap instruction with slippage
- [ ] `orca_open_position` / `orca_close_position` — CLMM LP management
- [ ] Full CL tick-array-aware quote (replace constant product approximation)
- [ ] `TransactionComposer` — combine multiple actions into atomic bundles
- [ ] Pool state analysis tools (tick distribution, IL, PnL)

### Future Protocol Adapters

- [ ] Solend (lending: deposit, withdraw, borrow, repay, health factor monitoring)
- [ ] Jupiter (swap aggregation: best route across all DEXs)
- [ ] Cross-protocol risk engine (net exposure, health factor simulation, rebalancing planner)

### Future Extensions

These are longer-term capabilities that build on the core control plane + execution adapters:

- **Autonomous observers** — cron/scheduler, on-chain event listeners, emergency escape pod
- **Intent-based UX** — natural language → DeFi action compilation, Telegram/voice channels
- **Portfolio dashboard** — aggregated cross-protocol position view, historical PnL

---

## Principles

1. **Human signs, AI proposes.** No phase ever introduces auto-signing. The user always holds the keys.
2. **Compile-time safety first.** Every pipeline stage uses typestate enforcement. If it compiles, it's safe.
3. **Fail-closed on ambiguity.** If the AI isn't sure, it asks. If a policy check fails, the action is blocked.
4. **Audit everything.** Every proposal, simulation, policy decision, approval, and signature — logged with full trail.
5. **Control plane first, adapters second.** The pipeline and policy engine are the product. Protocol integrations are downstream applications.
