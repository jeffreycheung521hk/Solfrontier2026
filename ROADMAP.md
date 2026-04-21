# ClawSolana — Roadmap

**Product:** Policy-gated transaction control plane for Solana.

**Created:** 2026-03-19 | **Updated:** 2026-04-04
**Baseline:** Session 20 complete (343 tests, zero failures, zero warnings, Rust 1.94.0)
**Companion:** [ARCHITECTURE.md](ARCHITECTURE.md) — system invariants, security, crate map

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
| P0-3: Session-request binding check | ✅ Done |
| P1-2: Rate limiting (C1) | ✅ Done — per-token sliding window, 429 + Retry-After |
| P1-3: `bind_wallet` challenge-response | ✅ Done |
| C4: `bind_wallet` persistence (cross-restart) | ✅ Done — SQLite write-through, startup recovery |
| N12: Context trimming (tool_use/tool_result pairs) | ✅ Done |

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

#### 1.3 — External Signer Formalization ✅
- [x] `SignerType::External` live in daemon config (`daemon.rs:254-267`)
- [x] Signing tool routes to external pending path for external wallets
- [x] Session binding + orchestrator routing works end-to-end

#### 1.4 — On-Chain Submission *(mostly complete)*
- [x] `sendTransaction` after verification passes
- [x] Confirmation tracking: submitted → confirmed → finalized / failed
- [x] Basic retry: signed_tx_bytes wired through tracker, RetryPolicy configurable via TOML

#### 1.5 — Operational Safety *(mostly complete)*
- [x] Rate limiting (C1) — ✅ Done (per-token sliding window, 60 rpm + 10 burst, 429 + Retry-After)
- [x] Two-pass compute budget (N13) — ✅ Done (simulate → tighten CU → re-simulate, safe fallback)
- [x] Session-request binding (P0-3) — ✅ Done
- [x] Context trimming fix (tool_use/tool_result pair-safe) — ✅ Done
- [x] Wallet context injection — agent system prompts include loaded wallet pubkeys

#### 1.6 — End-to-End Test ✅
- [x] Session → propose → simulate → policy → external sign → verify → submit → **Confirmed on devnet** (2026-03-30)
- [x] On-chain tx: `427TuF48fJGUsHDCgNETndGZwfg2sa85917YadZcJ852xUF2hya9GPf4XZEdhgkGDZUDeCTNFMAxTrPrQE2Q4P8F`

**Exit Criteria:** A real browser wallet connects, signs a pending transaction, and it lands on devnet with confirmation tracking.

---

### Phase 2 — Policy Engine Expansion *(in progress)*

> **Prerequisite:** Phase 1 complete ✅. Policy engine foundation delivered in Session 20.

**Goal:** Evolve the policy layer from "approve / require-human / block" into a configurable, composable rule engine for organizational transaction governance.

#### 2.1 — Granular Policy Rules *(partially done)*
- [x] Destination denylist (pubkeys that can never receive funds) — ✅ Done
- [x] Program allowlist (reject transactions invoking unlisted programs) — ✅ Done
- [x] Amount thresholds (`AmountExceedsLamports` — escalate above threshold) — ✅ Done
- [x] TOML-driven `[[policy.rules]]` with first-match-wins evaluation — ✅ Done
- [x] Instruction summary decoding (System Program transfers, CreateAccount) — ✅ Done
- [x] Policy verdict metadata in API responses (`policy_rule_name`, `policy_reason`) — ✅ Done
- [x] Per-session policy overrides (`SessionPolicyStore`, layered: session → global → defaults) — ✅ Done
- [ ] Time-based rules (business hours, maintenance windows)

#### 2.2 — Approval Matrix *(foundation done)*
- [x] Role-based approver gating (`required_approver_role` on policy action + verdict) — ✅ Done
- [x] Conditional routing (amount > threshold → escalate to specific approver role) — ✅ Done
- [x] Approver role validation on `POST /sessions/:id/approve` (403 on mismatch) — ✅ Done
- [ ] Multi-step approval chain (agent → risk → treasury sequentially)
- [ ] Quorum-based approval for high-value transactions

#### 2.3 — Signer Constraints
- [ ] Per-wallet signing policies (different thresholds per wallet)
- [ ] Signing session time-bounds

#### 2.4 — Policy Observability *(partially done)*
- [x] Policy evaluation audit trail (which rules fired, why, with full context) — ✅ Done
- [x] Per-rule hit-rate metrics (`CounterMap` in MetricsRegistry, `GET /metrics`) — ✅ Done
- [x] `PolicyEvaluationResult` with `rules_total`/`rules_evaluated`/`matched_rule_index` — ✅ Done
- [ ] Policy violation alerting (webhook/event on rejection)

**Exit Criteria:** Operators can configure policy rules (spend limits, program allowlists, approval escalation) without code changes. *(Largely met — TOML rules, per-session overrides, and role-based approval gating are live. Multi-step chain and quorum remain.)*

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

**Current:** Read-only tools + swap execution + message-driven demo — live, devnet confirmed.

| Tool | Status |
|------|--------|
| `orca_get_whirlpool` | ✅ Live |
| `orca_get_quote` | ✅ Live |
| `orca_check_pool` | ✅ Live |
| `orca_swap` | ✅ Live — devnet confirmed ([swap tx](https://explorer.solana.com/tx/CPsdyFS7UTzZBZ6g9qUfKvoiA6YDobXKgyVDdcfGAW8zDzMVWc8QZN3MDGfLi7wUidQxEaMoZJWsiunt6o8X4Jb?cluster=devnet)) |
| Message-driven demo | ✅ Live — "swap SOL to USDC" → agent → pipeline → devnet ([demo tx](https://explorer.solana.com/tx/56yYDtW9TNv8GwWtBn8HFDEohcM3a4QcHQkpJoqmcgQ7AzACTQViciWdBBTZicowwm4KZk9dhQEBQ6ac9NCV4ovw?cluster=devnet)) |

**Planned:**
- [ ] `DeFiAction` trait + `ProtocolAdapter` abstraction
- [ ] `orca_open_position` / `orca_close_position` — CLMM LP management
- [ ] Full CL tick-array-aware quote (replace constant product approximation)
- [ ] `TransactionComposer` — combine multiple actions into atomic bundles
- [ ] Pool state analysis tools (tick distribution, IL, PnL)

### Jupiter (aggregator — Phase 1 intent-first JIT)

**Current:** `submit_jupiter_swap` tool with intent-first policy + approval + JIT V0 build.
See [docs/JUPITER_JIT_PHASE1.md](docs/JUPITER_JIT_PHASE1.md) and
[docs/integration-architecture.md](docs/integration-architecture.md).

| Capability | Status |
|------------|--------|
| `submit_jupiter_swap` (intent submission) | ✅ Live |
| Swap policy (mint allowlist, amount cap, slippage cap) | ✅ Live |
| JIT V0 build + ALT assembly + simulate | ✅ Live |
| Local-signer execution path | ✅ Live |
| External-wallet (Phantom) V0 completion + strict blockhash mode | ✅ Live |
| `rebuild_required` retry signal on wallet blockhash modification | ✅ Live |
| Phantom blockhash-refresh compatibility mode | ⏳ Deferred (needs interop evidence) |
| Trigger / Limit / `/order` (DCA, scheduled) | ⏳ Future milestone |

### Future Protocol Adapters

- [ ] Solend (lending: deposit, withdraw, borrow, repay, health factor monitoring)
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
