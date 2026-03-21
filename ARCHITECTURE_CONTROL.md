# ClawSolana — Architecture Control Document

**Purpose:** This document defines the non-negotiable system invariants, layered architecture model, data flow contracts, failure modes, and phase gate criteria for the ClawSolana project. It exists to **prevent design drift** as the system evolves from V1 control plane to the North Star (Autonomous & Intent-Based DeFi Orchestrator).

**Authority:** Any code change that violates an invariant defined here requires explicit sign-off and an update to this document first. "Move the fence, don't step over it."

**Created:** 2026-03-19 | **Updated:** 2026-03-21
**Companion:** [ROADMAP.md](ROADMAP.md) (phased delivery plan)

> **Update (2026-03-21):** The invariants in this document remain valid. Since creation, the following architectural additions have been made that extend (but do not violate) these invariants:
> - **INV-9 (implicit): Durable-first pending state** — no externally visible `request_id` without SQLite backing. Implemented via `DurablePendingState` with lifecycle state (`pending`/`consumed`/`expired`/`rejected`).
> - **INV-10 (implicit): Mark-terminal-before-remove** — DB row must be marked terminal before in-memory removal, preventing stale resurrection on crash.
> - **`abort_pending()` vs `expire()`** — rollback cleanup and natural TTL expiry are semantically distinct operations on the orchestrator.
> - These additions are consistent with INV-6 (Fail-Closed on Ambiguity) and INV-3 (Append-Only Audit Trail).

---

## 1. Non-Negotiable System Invariants

These invariants are **absolute**. No phase, feature, or optimization may violate them. If a design requires weakening an invariant, the design is wrong.

### INV-1: Human Signs, AI Proposes

```
∀ transaction T: T.signed == true ⟹ ∃ human H who explicitly approved T
```

- The AI agent **never** holds signing authority. `SignTransaction` and `SendTransaction` capabilities are never granted to any `AgentRole`.
- Every signed transaction requires either:
  - (a) Human approval via `POST /approve` + local keystore signing, or
  - (b) External wallet signature from user-controlled wallet (Phantom)
- **No "auto-sign" mode.** Not in Phase 0, not in Phase 4, not ever.
- Emergency Escape Pod (Phase 3) proposes with elevated priority notifications — it does **not** auto-execute.
- Autonomous observers (Phase 3) may **wake up** the agent and **construct** proposals, but the signing gate is inviolable.

### INV-2: Typestate Pipeline Integrity

```
TransactionProposal → SimulatedTransaction → ApprovedTransaction → SignedTransaction
```

- Every state transition is enforced at **compile time** via Rust's type system.
- You cannot call `sign()` on a `TransactionProposal`. You cannot call `evaluate_policy()` on an unsigned tx.
- The `TransactionComposer` (Phase 1) produces a `TransactionProposal` — it enters the same pipeline.
- The `EmergencyWithdrawPlanner` (Phase 3) produces a `TransactionProposal` — it enters the same pipeline.
- The `IntentCompiler` (Phase 4) produces a `TransactionProposal` — it enters the same pipeline.
- **No shortcut paths.** Every transaction, regardless of origin, traverses the full pipeline.
- `new_unchecked()` constructors remain `pub` only for the resume-from-persistence path and **must** be annotated with `// SAFETY:` comments explaining the invariant being upheld.

### INV-3: Append-Only Audit Trail

```
∀ action A that mutates state or proposes a transaction:
  audit_log.append(A) BEFORE A.execute()
```

- `AuditRepository` has **no** update or delete methods. This is enforced by API surface design, not just convention.
- Every new subsystem (observers, schedulers, intent parsers, emergency detectors) must emit audit events **before** acting.
- Audit events include:
  - `who`: session_id, agent_role, or system component
  - `what`: action type + payload
  - `why`: reasoning chain or trigger signal
  - `when`: UTC timestamp
  - `outcome`: success/failure/pending
- Autonomous wakeups (Phase 3) must log: trigger signal → analysis → decision → proposal (or no-action).
- Intent translations (Phase 4) must log: raw input → parsed intent → compiled actions.

### INV-4: Capability-Gated Tool Execution

```
∀ tool invocation T in session S:
  T.required_capability ∈ S.capability_set  ∨  T is REJECTED
```

- Fail-closed: unknown capabilities are rejected.
- Capabilities are narrowed per-session based on `AgentRole` at session creation time. They cannot be escalated mid-session.
- New tools introduced in any phase **must** define their required capability. A tool without a capability gate is a bug.
- Protocol adapter tools (Phase 2+) inherit capability requirements from the `DeFiAction` they wrap:
  - Read operations → `ReadChain`
  - Write operations (build tx) → `BuildTransaction`
  - Propose signing → `ProposeSigning`

### INV-5: Simulation Before Signing

```
∀ transaction T: T.signed == true ⟹ T.simulated == true ∧ T.simulation_passed == true
```

- No transaction reaches the signing stage without a successful simulation.
- Simulation uses the **finalized** transaction (with compute budget instructions prepended).
- If simulation fails, the pipeline halts. There is no "skip simulation" flag.
- Composed transactions (Phase 1) are simulated as a whole — not individual actions.

### INV-6: Fail-Closed on Ambiguity

- If the AI cannot determine the user's intent → ask for clarification, do not guess.
- If a policy rule cannot be evaluated (DB error, missing data) → block the action.
- If an observer signal is ambiguous → log and alert, do not trigger emergency response.
- If intent parsing confidence is below threshold → request disambiguation.
- **Default deny.** The system must be explicitly configured to allow actions, not explicitly configured to block them.

### INV-7: Local-Only API Surface

```
api.bind_addr == "127.0.0.1"  (NEVER "0.0.0.0")
```

- The HTTP API is local-only by design. Remote access requires a user-managed tunnel (SSH, WireGuard).
- Channel adapters (Telegram, Phase 4) are **outbound** connections from the daemon, not inbound listeners on public interfaces.
- This invariant exists because the API uses bearer tokens, not mutual TLS. Exposing to the network would require a full auth redesign.

### INV-8: No Unsafe Code in Critical Crates

```
#![forbid(unsafe_code)] in: wallet-engine, types, observability, risk-engine, tool-system
```

- These crates handle signing, policy evaluation, and capability enforcement. `unsafe` here is categorically unacceptable.
- New crates (`claw-defi-core`, `claw-scheduler`, `claw-observers`, `claw-notify`) must also carry `#![forbid(unsafe_code)]`.
- `claw-solana-core` may use `unsafe` only if required by Solana SDK FFI, with explicit `// SAFETY:` documentation.

---

## 2. Layered Architecture Model

The system is organized into four architectural planes. Each plane has a clear responsibility boundary, and cross-plane communication follows strict contracts.

```
┌──────────────────────────────────────────────────────────────────┐
│                     INTENT PLANE (Phase 4)                       │
│  Channels (CLI, Telegram, Voice) → Intent Parser → DeFiIntent   │
│  Responsibility: Accept fuzzy human input, produce structured    │
│                  intents. No execution logic.                    │
└───────────────────────────┬──────────────────────────────────────┘
                            │ DeFiIntent
                            ▼
┌──────────────────────────────────────────────────────────────────┐
│                    CONTROL PLANE (Phase 0–1)                     │
│  Agent Runtime (ReAct) → Tool Dispatch → Policy Engine           │
│  Transaction Composer → Typestate Pipeline → Approval Gate       │
│  Responsibility: Translate intents/commands into validated,      │
│                  policy-checked transaction proposals.            │
│  Key constraint: NEVER executes. Only proposes.                  │
└───────────────────────────┬──────────────────────────────────────┘
                            │ ApprovedTransaction
                            ▼
┌──────────────────────────────────────────────────────────────────┐
│                   EXECUTION PLANE (Phase 0–1)                    │
│  Signing Gate (Human Approval) → Wallet Engine → RPC Submitter   │
│  External Wallet Bridge (Phantom) → Confirmation Tracker         │
│  Responsibility: Sign and submit approved transactions.          │
│  Key constraint: Only processes ApprovedTransactions.             │
│                  Cannot create or modify transactions.            │
└──────────────────────────────────────────────────────────────────┘

┌──────────────────────────────────────────────────────────────────┐
│                   OBSERVER PLANE (Phase 3)                       │
│  Scheduler → On-Chain Listeners → External Signals               │
│  Emergency Detector → Risk Monitor                               │
│  Responsibility: Watch external state, emit trigger events.      │
│  Key constraint: Read-only. Cannot construct or sign transactions.│
│                  Can only wake up the Control Plane.              │
└───────────────────────────┬──────────────────────────────────────┘
                            │ TriggerEvent
                            ▼
                       Control Plane (re-enters at Agent Runtime)
```

### Plane Communication Contracts

| From | To | Contract Type | Payload |
|------|----|---------------|---------|
| Intent Plane → Control Plane | `DeFiIntent` | Structured enum, validated | Intent kind, parameters, confidence score |
| Control Plane → Execution Plane | `ApprovedTransaction` | Typestate-enforced | Simulated, policy-checked, human-approved tx |
| Observer Plane → Control Plane | `TriggerEvent` | Event bus message | Signal type, severity, source, timestamp, raw data |
| Control Plane → Intent Plane | `ProposalNotification` | Push notification | Human-readable summary, approval deep-link |
| Execution Plane → Control Plane | `ExecutionOutcome` | Event bus message | Tx signature, confirmation status, error |

### Forbidden Cross-Plane Calls

| Forbidden | Why |
|-----------|-----|
| Observer → Execution | Observers cannot sign or submit. They can only trigger the control plane to propose. |
| Intent → Execution | Intents cannot bypass the control plane. Even "urgent" intents go through policy. |
| Execution → Control (mutation) | Execution plane reports outcomes but cannot modify proposals or re-enter the pipeline. |
| Any plane → Audit (delete/update) | Audit is append-only from all planes. |

---

## 3. End-to-End Data Flow Pipelines

### Pipeline A: User-Initiated Command (Phase 0–1)

```
User ──[HTTP/CLI]──→ Session/Message API
                          │
                          ▼
                    Agent Runtime (ReAct loop)
                          │
                    ┌─────┴──────┐
                    │ Tool Calls │ (capability-gated)
                    └─────┬──────┘
                          │
                    ┌─────┴──────────────┐
                    │ Read Tools         │──→ Return data to agent
                    │ (ReadChain, etc.)  │
                    └────────────────────┘
                    ┌─────┴──────────────┐
                    │ DeFi Action Tools  │
                    │ (BuildTransaction) │
                    └─────┬──────────────┘
                          │ Vec<DeFiAction>
                          ▼
                    TransactionComposer
                          │ TransactionProposal
                          ▼
                    prepend_compute_budget()
                          │
                          ▼
                    simulate() ──[fail]──→ HALT, report error
                          │ SimulatedTransaction
                          ▼
                    evaluate_policy() ──[deny]──→ HALT, report denial
                          │ ApprovedTransaction (or RequiresHumanApproval)
                          ▼
                    ┌─────────────────┐
                    │ Human Approval  │ ← SSE push + Phantom deep-link
                    │ Gate            │
                    └─────┬───────────┘
                          │ (approved)
                          ▼
                    sign() ──→ Local keystore OR External wallet bridge
                          │ SignedTransaction
                          ▼
                    sendTransaction() ──→ Solana RPC
                          │
                          ▼
                    Confirmation Tracker ──→ SSE event + audit log
```

### Pipeline B: Autonomous Observer Trigger (Phase 3)

```
External Signal ──→ Observer Plane
    (on-chain event, cron tick, webhook, social panic)
                          │ TriggerEvent { signal, severity, data }
                          ▼
                    ┌─────────────────────┐
                    │ Signal Aggregator   │ ← Deduplication, debounce, severity threshold
                    │ & Priority Router   │
                    └─────┬───────────────┘
                          │
                    ┌─────┴──────────────────────┐
                    │ severity >= EMERGENCY       │──→ Priority notification path
                    │ severity < EMERGENCY        │──→ Normal notification path
                    └─────┬──────────────────────┘
                          │
                          ▼
                    Agent Runtime (ReAct loop, autonomous wakeup)
                          │
                    [Audit: log trigger → analysis → decision]
                          │
                    ┌─────┴──────┐
                    │ Action?    │
                    │ Yes / No   │
                    └─────┬──────┘
                          │ (if yes)
                          ▼
                    ──→ Pipeline A (from TransactionComposer onward)
                          │
                    [Human Approval Gate — ALWAYS required, even for emergencies]
```

### Pipeline C: Intent-Based Operation (Phase 4)

```
User ──[Telegram/Voice/Text]──→ Channel Adapter
                                      │ raw text/audio
                                      ▼
                                Speech-to-Text (if voice)
                                      │ text
                                      ▼
                                Intent Parser (LLM-powered)
                                      │
                                ┌─────┴──────────────┐
                                │ Confidence < θ     │──→ Disambiguation prompt to user
                                │ Confidence >= θ    │
                                └─────┬──────────────┘
                                      │ DeFiIntent (validated)
                                      ▼
                                [Audit: log raw input → parsed intent]
                                      │
                                      ▼
                                Intent Compiler
                                      │ Resolves: token symbols → mints,
                                      │           "highest yield" → pool lookup,
                                      │           slippage from urgency
                                      │
                                      │ Vec<DeFiAction>
                                      ▼
                                ──→ Pipeline A (from TransactionComposer onward)
```

---

## 4. Failure Modes & Recovery Strategies

### 4.1 Transaction Failure Matrix

| Failure Point | Detection | Recovery | Idempotency |
|---------------|-----------|----------|-------------|
| **Simulation fails** | `simulate()` returns error | Pipeline halts. Agent reports error to user with reason. No retry — user may need to adjust parameters. | Safe — no state mutated. |
| **Policy denial** | `evaluate_policy()` returns `Deny` | Pipeline halts. Agent reports denial reason. User may adjust or request override (if policy allows). | Safe — no state mutated. |
| **Human rejects** | `POST /approve` with `approved: false` | Pipeline halts. `ApprovalRejected` audit event. Agent informed. | Safe — approval is one-shot. |
| **Blockhash expires** (before signing) | Reaper detects `parked_at + TTL` exceeded | Request expired, `WalletSignatureExpired` event. User must re-initiate. Agent can re-propose with fresh blockhash. | Safe — pending entry removed. |
| **Signing fails** (local keystore) | `sign()` returns error | Pipeline halts. Audit event logged. Agent can retry with new proposal. | Safe — unsigned tx not submitted. |
| **External wallet — user never signs** | Reaper TTL expiry | Same as blockhash expiry above. | Safe. |
| **External wallet — tampered signature** | `verify_signed_tx()` fails | Rejection logged. `WalletSignatureRejected` event. Request consumed (not reusable). | Safe — atomic take-and-verify. |
| **RPC submission fails** | `sendTransaction()` error | Retry with exponential backoff (max 3 attempts). If all fail, `TransactionFailed` event. User may re-sign with fresh blockhash. | Safe — Solana deduplicates by tx signature. |
| **RPC confirms then reverts** (fork) | Confirmation tracker detects slot rollback | `TransactionReverted` event. Alert user. Agent may re-propose if action still needed. | Depends on on-chain program idempotency. |
| **Partial composed tx failure** | Solana atomicity — tx is all-or-nothing | Entire composed tx fails. No partial execution. User re-initiates. | Safe — Solana tx is atomic. |

### 4.2 Observer Plane Failures

| Failure | Detection | Recovery |
|---------|-----------|----------|
| **WebSocket disconnects** | Connection error / heartbeat timeout | Exponential backoff reconnect. Log gap in observation window. Resume from last known slot. |
| **Duplicate signals** | Signal deduplication by `(source, type, content_hash, window)` | Deduplicate in aggregator. Only first signal in window triggers action. |
| **False positive emergency** | Cannot be detected automatically | Human approval gate prevents execution. Audit trail captures reasoning for post-mortem. |
| **Observer crash** | Health check failure | Supervisor restarts observer. Gap in monitoring logged. Alert user that coverage was interrupted. |
| **Stale on-chain data** | Compare `slot` in response vs expected | Reject stale data. Re-fetch. Log staleness metric. |

### 4.3 Intent Plane Failures

| Failure | Detection | Recovery |
|---------|-----------|----------|
| **Ambiguous intent** | Confidence score below threshold | Disambiguation prompt: "Did you mean X or Y?" |
| **Unknown token/protocol** | Token symbol resolver returns empty | Ask user: "I don't recognize 'BONK'. Did you mean [suggestions]?" |
| **Impossible intent** | Compiler detects infeasible action (e.g., withdraw more than deposited) | Report: "You only have X deposited. Withdraw X instead?" |
| **LLM hallucination** (fabricated pool/token) | Compiler validates all referenced entities exist on-chain | Reject. Ask user to rephrase. Log the hallucination for model evaluation. |
| **Speech-to-text error** | Confidence score on transcription | Confirm: "I heard: '[transcription]'. Is that correct?" |

### 4.4 Idempotency Model

All state mutations must be idempotent or protected by exactly-once mechanisms:

| Operation | Idempotency Mechanism |
|-----------|----------------------|
| Spend recording | `INSERT OR IGNORE` + `UNIQUE(transaction_id)` |
| Approval decision | One-shot `oneshot::Sender` — can only fire once |
| External wallet signature | Atomic `take_and_verify()` — entry removed before processing |
| Audit event | Append-only, `INSERT` with auto-increment — duplicates are harmless |
| RPC submission | Solana deduplicates by signature — re-submitting same signed tx is safe |
| Observer trigger | Deduplication window — same signal within N seconds is collapsed |
| Scheduled wakeup | Idempotency key per schedule tick — `INSERT OR IGNORE` on execution record |

### 4.5 Partial Execution & Recovery (Cross-Protocol, Phase 2+)

Cross-protocol operations that span multiple transactions (e.g., "Withdraw from Orca" + "Repay Solend" as separate txs) introduce partial execution risk:

**Strategy: Sequential Execution with Checkpoints**

```
Plan: [Action1_tx, Action2_tx, Action3_tx]
         │
         ▼
    Execute Action1_tx → Confirm on-chain
         │
    [Checkpoint: Action1 complete, Action2/3 pending]
         │
         ▼
    Execute Action2_tx → FAILS
         │
    [Checkpoint: Action1 complete, Action2 failed, Action3 not started]
         │
         ▼
    Recovery Options (presented to user):
      (a) Retry Action2 with fresh blockhash
      (b) Rollback: reverse Action1 (if reversible)
      (c) Abort: leave in partial state, user handles manually
```

- **Checkpoints** are persisted to SQLite. The system can resume from the last checkpoint after restart.
- **Rollback** is best-effort — not all DeFi actions are reversible (e.g., swap already executed at a price).
- **User always decides** which recovery path to take. The AI proposes options; it does not auto-recover.
- Single-transaction composed operations (via `TransactionComposer`) are atomic — Solana guarantees all-or-nothing.

---

## 5. Phase Gate Criteria

Each phase has **mandatory** exit criteria. Development of the next phase **must not begin** until all criteria are met. "Green CI" alone is insufficient — architectural invariants must be verified.

### Gate 0 → 1: Production Foundation Certified

| # | Criterion | Verification Method |
|---|-----------|---------------------|
| G0.1 | All P0 issues (P0-1, P0-2, P0-3) are fixed and tested | Specific regression tests for each P0 |
| G0.2 | All P1 issues (P1-1 through P1-4) are resolved | Integration tests for persistence, rate limiting, ownership proof, event replay |
| G0.3 | `sendTransaction` works on devnet with confirmation tracking | E2E test: propose → approve → sign → submit → confirmed on-chain |
| G0.4 | 120+ tests passing, zero failures | `cargo test` |
| G0.5 | No P0-severity issues in any crate | Manual review of all `DashMap` usage, all HTTP endpoints |
| G0.6 | Audit trail captures all state transitions including tx confirmation | Query audit log after E2E test, verify completeness |
| G0.7 | Reaper tasks active for all TTL-bound stores | Observe reaper logs in integration test, verify expired entries are cleaned |

### Gate 1 → 2: Single-Protocol Execution Verified

| # | Criterion | Verification Method |
|---|-----------|---------------------|
| G1.1 | `DeFiAction` trait defined and implemented for Orca (swap, LP open/close, increase/decrease) | Unit tests for instruction building, integration tests with simulated pool |
| G1.2 | `TransactionComposer` produces valid composed transactions | Test: Withdraw → Swap → Deposit composes into single tx, simulates successfully |
| G1.3 | Orca swap E2E on devnet via Phantom | Manual test: agent proposes swap → user signs in Phantom → tx lands on devnet |
| G1.4 | Composed transactions traverse full typestate pipeline | Compile-time verification (if it compiles, this holds) + integration test |
| G1.5 | 150+ tests passing | `cargo test` |
| G1.6 | Pool state analysis tools return accurate data | Cross-check `orca_analyze_pool` output against Orca SDK directly |
| G1.7 | `ProtocolAdapter` trait is generic enough for a second protocol | Review: Solend integration plan does not require trait changes |

### Gate 2 → 3: Multi-Protocol Risk Engine Operational

| # | Criterion | Verification Method |
|---|-----------|---------------------|
| G2.1 | At least 2 protocols fully integrated (Orca + Solend) | E2E tests for both protocols on devnet |
| G2.2 | Cross-protocol rebalancing produces valid composed transactions | Test: "Withdraw Orca → Swap Jupiter → Repay Solend" compiles and simulates |
| G2.3 | Health Factor simulator produces correct results | Unit tests with known positions, compare against manual calculation |
| G2.4 | `ProtocolRegistry` dynamically loads protocol adapters | Integration test: register adapter at runtime, invoke tools |
| G2.5 | Policy Engine V2 enforces cross-protocol exposure limits | Test: policy blocks action that exceeds exposure limit |
| G2.6 | 200+ tests passing | `cargo test` |
| G2.7 | All INV-* invariants verified against new subsystems | Audit checklist: each invariant explicitly tested for Phase 2 code |

### Gate 3 → 4: Autonomous Operation Safe

| # | Criterion | Verification Method |
|---|-----------|---------------------|
| G3.1 | Scheduler wakes up agent on configured interval | Integration test: configure 5s interval, verify 3 consecutive wakeups |
| G3.2 | On-chain listener detects account change and triggers agent | Test: modify account in solana-test-validator, verify trigger event |
| G3.3 | Emergency Escape Pod constructs valid withdraw proposals | Test: simulate panic signal → verify withdraw tx proposal is correct |
| G3.4 | Emergency proposals still require human signature (INV-1) | Test: emergency trigger → proposal created → NOT auto-signed → human approves → tx submitted |
| G3.5 | Observer plane crash does not affect control/execution planes | Test: kill observer, verify control plane still processes user commands |
| G3.6 | Signal deduplication prevents duplicate triggers | Test: send same signal 5 times in 1 second, verify only 1 agent wakeup |
| G3.7 | All autonomous actions fully audited with reasoning chain | Query audit log after autonomous action, verify trigger → analysis → decision → proposal chain |
| G3.8 | 250+ tests passing | `cargo test` |

### Gate 4 → v1.0: Intent System Production-Ready

| # | Criterion | Verification Method |
|---|-----------|---------------------|
| G4.1 | Intent parser handles all `IntentKind` variants | Unit tests with 20+ natural language inputs per variant |
| G4.2 | Telegram adapter receives message and returns proposal card | Integration test with Telegram Bot API (test server or mock) |
| G4.3 | Voice → text → intent pipeline works end-to-end | Test: audio file → transcription → intent → DeFi action |
| G4.4 | Ambiguous intents trigger disambiguation, not execution | Test: send ambiguous input, verify system asks for clarification |
| G4.5 | Hallucinated tokens/pools are caught by compiler validation | Test: fabricated token name → rejection with helpful error |
| G4.6 | All 4 target scenarios (S1–S4) pass E2E testing | Scenario-specific E2E test suites |
| G4.7 | 300+ tests passing | `cargo test` |
| G4.8 | Full invariant audit (INV-1 through INV-8) against all planes | Formal checklist, each invariant verified per plane |

---

## 6. Crate Ownership & Plane Mapping

| Crate | Plane | INV Enforcement | Phase Introduced |
|-------|-------|------------------|-----------------|
| `claw-types` | All | INV-8 (forbid unsafe) | 0 |
| `claw-observability` | All | INV-3 (audit), INV-8 | 0 |
| `claw-state-store` | Control + Execution | INV-3 (append-only) | 0 |
| `claw-solana-core` | Execution | INV-5 (simulation) | 0 |
| `claw-wallet-engine` | Execution | INV-1 (signing gate), INV-2 (typestate), INV-8 | 0 |
| `claw-risk-engine` | Control | INV-4 (capability), INV-6 (fail-closed), INV-8 | 0 |
| `claw-tool-system` | Control | INV-4 (capability gate), INV-8 | 0 |
| `claw-agent-runtime` | Control | INV-6 (fail-closed) | 0 |
| `claw-channels` | Intent | INV-7 (local API) | 0 (CLI), 4 (Telegram) |
| `claw-api` | Control | INV-7 (bind 127.0.0.1) | 0 |
| `claw-gateway` | Control + Execution | INV-1, INV-2, INV-3 | 0 |
| `claw-defi-core` | Control | INV-8 | 1 |
| `claw-proto-orca` | Control | INV-4, INV-5 | 1 |
| `claw-proto-solend` | Control | INV-4, INV-5 | 2 |
| `claw-proto-jupiter` | Control | INV-4, INV-5 | 2 |
| `claw-scheduler` | Observer | INV-3 (audit wakeups), INV-8 | 3 |
| `claw-observers` | Observer | INV-3, INV-6, INV-8 | 3 |
| `claw-notify` | Intent (outbound) | INV-8 | 3–4 |
| `claw-channel-telegram` | Intent | INV-7, INV-8 | 4 |

---

## 7. Architecture Decision Records (ADRs)

Significant design decisions must be recorded here when made. Template:

```
### ADR-NNN: <Title>
**Date:** YYYY-MM-DD
**Status:** Accepted | Superseded by ADR-XXX
**Context:** Why this decision was needed.
**Decision:** What we decided.
**Consequences:** What this enables and what it constrains.
```

### ADR-001: Typestate Pipeline Over Runtime Checks

**Date:** 2026-03-18
**Status:** Accepted
**Context:** Transaction safety could be enforced via runtime checks (assert simulation ran before signing) or compile-time types.
**Decision:** Use Rust's type system to make invalid state transitions unrepresentable. `TransactionProposal`, `SimulatedTransaction`, `ApprovedTransaction` are distinct types with consuming methods.
**Consequences:** Impossible to accidentally sign an unsimulated transaction. Adds boilerplate for the resume-from-persistence path (`new_unchecked`). Worth it — a runtime assertion can be missed; a type error cannot.

### ADR-002: Human-in-the-Loop as Inviolable Invariant

**Date:** 2026-03-19
**Status:** Accepted
**Context:** Autonomous operations (Phase 3) and emergency responses could benefit from auto-signing. This would reduce response latency for emergencies.
**Decision:** No auto-signing under any circumstances. Emergency Escape Pod escalates notification priority but still requires human signature.
**Consequences:** Emergency response time is bounded by human reaction time. Accepted trade-off — the risk of autonomous signing (agent error, compromised signal, hallucinated emergency) outweighs the latency benefit. If a user is unreachable, funds are at the same risk as if ClawSolana didn't exist.

### ADR-003: Local-Only API Surface

**Date:** 2026-03-18
**Status:** Accepted
**Context:** Remote access would simplify deployment and enable multi-device usage.
**Decision:** API binds to `127.0.0.1` only. Remote access is the user's responsibility (SSH tunnel, VPN).
**Consequences:** Simplifies auth model (bearer token sufficient for local use). Prevents accidental exposure. Channel adapters (Telegram) use outbound connections, not inbound listeners. Multi-device requires explicit network setup.

### ADR-004: Four-Plane Architecture

**Date:** 2026-03-19
**Status:** Accepted
**Context:** As the system grows from control plane to autonomous orchestrator, responsibilities must be cleanly separated to prevent logic leaks (e.g., observer that directly signs transactions).
**Decision:** Four planes (Intent, Control, Execution, Observer) with explicit communication contracts. Cross-plane calls follow defined payload types. Forbidden calls are documented.
**Consequences:** Each plane can be developed and tested independently. Observer plane crash doesn't affect execution. Intent plane changes don't require execution plane updates. Adds complexity in wiring — worth it for isolation guarantees.

---

## 8. Invariant Verification Checklist

For every PR that introduces a new subsystem, reviewer must verify:

- [ ] **INV-1:** Does this code path ever lead to signing without human approval? (Must be NO)
- [ ] **INV-2:** Does the new code interact with the transaction pipeline? If yes, does it produce a `TransactionProposal` that enters the standard pipeline? (Must be YES)
- [ ] **INV-3:** Does this code mutate state or propose transactions? If yes, are audit events emitted before the action?
- [ ] **INV-4:** Does this code add new tools? If yes, do they have capability requirements?
- [ ] **INV-5:** Does this code construct transactions? If yes, do they go through simulation?
- [ ] **INV-6:** Are there any code paths where ambiguity leads to action (rather than asking/blocking)?
- [ ] **INV-7:** Does this code open any network listeners? If yes, are they local-only?
- [ ] **INV-8:** Does the crate use `unsafe`? If yes, is it in an allowed crate with `// SAFETY:` docs?

---

*This document is the source of truth for architectural constraints. The [ROADMAP.md](ROADMAP.md) defines what to build and when. This document defines how to build it safely.*
