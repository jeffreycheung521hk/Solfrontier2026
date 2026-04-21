# ClawSolana — Architecture & Security

**Updated:** 2026-04-04 | **Companion:** [ROADMAP.md](ROADMAP.md)

---

## 1. System Invariants

These are **absolute**. No feature may violate them.

### INV-1: Human Signs, AI Proposes

No agent role grants `SignTransaction` or `SendTransaction`. Every signed transaction requires explicit human approval (via `POST /sessions/:id/approve` + local keystore, or external wallet signature from Phantom). No auto-sign mode exists or is planned.

### INV-2: Typestate Pipeline Integrity

```
TransactionProposal → SimulatedTransaction → ApprovedTransaction → SignedTransaction
```

Every transition is enforced at **compile time**. You cannot call `sign()` on a `TransactionProposal`. Every transaction, regardless of origin (agent, operator, Orca swap, future protocols), traverses the same pipeline.

### INV-3: Append-Only Audit Trail

`AuditRepository` has **no** update or delete methods. Every proposal, simulation, policy decision, approval, and signature is logged before execution.

### INV-4: Capability-Gated Tool Execution

Fail-closed dispatch: unknown capabilities are rejected. Per-session narrowing based on `AgentRole` at session creation. Tools without capability requirements are bugs.

### INV-5: Simulation Before Signing

No transaction reaches signing without successful simulation. No "skip simulation" flag exists.

### INV-6: Fail-Closed on Ambiguity

Default deny. If no policy rule matches → `RequiresHumanApproval`. If a check fails → block. If AI is unsure → ask.

### INV-7: Local-Only API

`bind_addr = "127.0.0.1"` — never `0.0.0.0`. Remote access is the operator's responsibility.

### INV-8: No Unsafe in Critical Crates

`#![forbid(unsafe_code)]` in: wallet-engine, types, observability, risk-engine, tool-system, state-store, api, agent-runtime.

---

## 2. Architecture Overview

```
┌─────────────────────────────────────────────────────────────┐
│                     INTENT SOURCES                          │
│   AI Agents  │  Operators  │  Services  │  Dashboards       │
└──────┬───────────┬─────────────┬──────────────┬─────────────┘
       ▼           ▼             ▼              ▼
┌─────────────────────────────────────────────────────────────┐
│                 CLAWSOLANA CONTROL PLANE                     │
│                                                             │
│  Capability Dispatch → Typestate Pipeline → Orchestrator    │
│  (role check,          (simulate,           (adapter route, │
│   tool gate,            policy,              pending life-  │
│   fail-closed)          approval)            cycle, verify) │
│                                                             │
│  Durable State  │  Spend Tracking  │  Audit + Events        │
│  (SQLite,        (dedup,            (append-only,            │
│   lifecycle,      policy context)    SSE broadcast)          │
│   recovery)                                                 │
└──────┬──────────────────────────────────┬───────────────────┘
       ▼                                  ▼
┌──────────────────┐          ┌────────────────────────┐
│  LOCAL WALLET     │          │  EXTERNAL WALLET        │
│  ADAPTER          │          │  ADAPTER                │
│  (SecretKeystore, │          │  (unsigned tx → client, │
│   sync sign)      │          │   client signs,         │
│                   │          │   4-step ed25519 verify) │
└──────┬────────────┘          └───────────┬─────────────┘
       ▼                                   ▼
┌─────────────────────────────────────────────────────────────┐
│                    SOLANA NETWORK                            │
└─────────────────────────────────────────────────────────────┘
```

### Crate Map (13 core + 2 binaries)

| Crate | Purpose |
|-------|---------|
| `claw-types` | Shared domain types (zero deps) |
| `claw-observability` | Logging, metrics (static + dynamic counters), health |
| `claw-state-store` | SQLite persistence (audit, spend, traces, pending state, wallet bindings) |
| `claw-solana-core` | RPC client, simulation, blockhash, compute budget, lifecycle tracking |
| `claw-wallet-engine` | Typestate signing pipeline, keystore, signer traits |
| `claw-risk-engine` | Policy evaluation (`PolicySet`, `PolicyEvaluationResult`), spend cap rules |
| `claw-tool-system` | Tool registry, dispatch, capability enforcement, Orca swap |
| `claw-agent-runtime` | ReAct loop, LLM clients (Anthropic + OpenAI), context management |
| `claw-channels` | Channel adapters (CLI) |
| `claw-api` | Axum HTTP server, auth, route handlers, rate limiting |
| `claw-gateway` | Control plane daemon, session/policy/approval management, signature orchestrator, Jupiter JIT + `submit_jupiter_swap` |
| `clawd` | Daemon binary |
| `claw` | CLI client binary |

### Startup Sequence

1. Load `ClawConfig` from TOML
2. Initialize tracing, open SQLite, run migrations
3. Build `RpcPool` with circuit breaker → `ClawRpcClient` → `BlockhashManager`
4. Build `PolicySet` from config (custom rules → defaults) + `SessionPolicyStore`
5. Load wallets (local keypairs + external pubkeys), build adapter chain
6. Build `SignatureOrchestrator` + `SubmitForSigningTool` (with session policy store)
7. Wire tool registry, agent router, session manager
8. Start Axum HTTP server on 127.0.0.1:7070

### Policy Evaluation Flow

```
config.policy.rules (custom, first-match-wins)
  + session_policy_overrides (if any, prepended)
  + default_rules (mainnet-requires-human or allow-all)
  → PolicySet::evaluate()
  → PolicyEvaluationResult { verdict, rules_total, rules_evaluated, matched_rule_index }
```

### Approval Matrix

When `PolicyAction::RequireHumanApproval` specifies `required_approver_role`:
- `ApprovalRequest` carries the requirement
- `POST /sessions/:id/approve` must include matching `approver_role`
- Mismatch → 403 Forbidden (request stays pending, not consumed)
- No role requirement → any operator can approve (backward compatible)

---

## 3. Security Posture

### Enforced (Code-Level)

| Threat | Mitigation | Enforcement |
|--------|-----------|-------------|
| Tx signed without simulation | Typestate pipeline | Compile-time |
| Tx signed without policy check | `ApprovedTransaction` typestate | Compile-time |
| Unauthorized API access | Bearer token, constant-time XOR | Runtime |
| Remote API access | Localhost-only bind | Config (hard-coded) |
| Key material in logs | `SecretKeypair::fmt()` → `[REDACTED]` | Compile-time |
| Prompt injection via tool output | Tagged prefix + system prompt | Partial |
| Capability escalation | `CapabilitySet` checked at dispatch | Runtime |
| Double-completion | Atomic `DashMap::remove()` consume-on-attempt | Runtime |
| Cross-session signature theft | Session-request binding (P0-3) | Runtime |
| Wallet binding impersonation | Challenge-response ownership proof | Runtime |
| External tx tampering | 4-step ed25519 verification | Runtime |
| Spend double-count | `UNIQUE(transaction_id)` + `INSERT OR IGNORE` | Database |
| Approver role bypass | `ApprovalStore::decide()` validates role before consuming | Runtime |

### Assumptions (Not Strong Guarantees)

- **Localhost-only** protects against remote access, NOT local process compromise
- **API token** is ephemeral unless `CLAW_API_TOKEN` is set; printed to stderr on startup
- **LLM provider** is a trust boundary — adversarial responses pass through policy engine, which is code (not LLM-controlled)
- **Prompt injection** is mitigated by tagged prefix, not eliminated

### Open Risks

| Risk | Status | Notes |
|------|--------|-------|
| Event bus durability | Open | `tokio::broadcast` may drop events; audit log is authoritative |
| Per-session policy persistence | Open | Session overrides are in-memory only (lost on restart) |
| Multi-step approval chain | Open | Current: single decision per request |
| TLS on API | Not planned V1 | Localhost-only; TLS is reverse proxy responsibility |

---

## 4. Production Readiness Review Status

> Originally reviewed 2026-03-19. Updated through Session 20.

| Issue | Severity | Status |
|-------|----------|--------|
| P0-1: Atomic completion race | P0 | ✅ Fixed — `DashMap::remove()` consume-on-attempt |
| P0-2: Request expiry reaper | P0 | ✅ Fixed — 30s interval, 120s TTL |
| P0-3: Session-request binding | P0 | ✅ Fixed — three-way match verification |
| P1-1: Durable pending state | P1 | ✅ Fixed — SQLite lifecycle, durable-first |
| P1-2: Rate limiting | P1 | ✅ Fixed — per-token sliding window, 429 + Retry-After |
| P1-3: Wallet ownership proof | P1 | ✅ Fixed — challenge-response, session-bound nonce |
| P1-4: Durable event log | P1 | Open — events are fire-and-forget |
| Runtime policy config | — | ✅ Fixed — TOML-driven rules + per-session overrides |
| Policy observability | — | ✅ Fixed — per-rule metrics, structured audit, evaluation metadata |
| Approval matrix | — | Partial — role-based gating done, multi-step chain not done |

### Idempotency Model

| Operation | Mechanism |
|-----------|----------|
| Spend recording | `INSERT OR IGNORE` + `UNIQUE(transaction_id)` |
| Approval decision | One-shot `oneshot::Sender` |
| External wallet signature | Atomic take-and-verify |
| Audit event | Append-only INSERT |
| RPC submission | Solana deduplicates by signature |

---

## 5. HTTP API Surface

All routes (except `/health`) require `Authorization: Bearer <token>`.

| Method | Path | Purpose |
|--------|------|---------|
| `GET` | `/health` | Health check (public) |
| `POST` | `/sessions` | Create session (accepts `policy_overrides`) |
| `GET` | `/sessions` | List sessions |
| `DELETE` | `/sessions/:id` | Close session |
| `POST` | `/sessions/:id/messages` | Send message to agent |
| `GET` | `/sessions/:id/events` | SSE event stream |
| `POST` | `/sessions/:id/approve` | Approve/reject (accepts `approver_role`) |
| `GET` | `/sessions/:id/approvals` | List pending approvals |
| `POST` | `/sessions/:id/bind-wallet` | Bind external wallet (legacy) |
| `POST` | `/sessions/:id/wallet-bind-challenge` | Request ownership challenge |
| `POST` | `/sessions/:id/wallet-bind-confirm` | Submit signed challenge |
| `POST` | `/sessions/:id/wallet-signatures` | Submit signed tx |
| `GET` | `/sessions/:id/wallet-signatures` | List pending sign requests |
| `GET` | `/metrics` | Operational metrics (JSON) |

---

## 6. Architecture Decision Records

### ADR-001: Typestate Pipeline Over Runtime Checks
Compile-time types make invalid transitions unrepresentable. Worth the boilerplate for `new_unchecked` in resume paths.

### ADR-002: Human-in-the-Loop as Inviolable Invariant
No auto-signing, even for emergencies. The risk of autonomous signing outweighs latency benefits.

### ADR-003: Local-Only API Surface
Bearer token auth is sufficient for localhost. Remote access is operator-managed (SSH/VPN).

### ADR-004: Four-Plane Architecture
Intent → Control → Execution → Observer planes with explicit contracts. Observer cannot sign. Intent cannot bypass control.

---

## 7. Module Placement Principles

Guidance for *where* new code belongs. Applies to every PR; does not require a reorg to take effect. Areas that do not yet conform are tracked in [DEBT.md](DEBT.md) with migration triggers.

### 7.1 Builders vs Executors

Every tool or protocol integration falls into one of two categories. The classification is mechanical, not stylistic:

| Test question | If yes |
|---------------|--------|
| Can this module, removed from `claw-gateway`, still produce a valid artifact (unsigned tx, intent preview) from pure inputs? | **Builder** — lives in `claw-tool-system` or a stateless module |
| Does this module need `approval_store`, `park_store`, blockhash tracking, or JIT rebuild on resume? | **Executor** — lives in `claw-gateway`, private to the control plane |

Builders are protocol-specific. Executors are control-plane-aware. Jupiter's intent + JIT flow is the reference shape for executors.

### 7.2 Target-Shape Over Current-Shape

Directory layout should reflect the shape the system is evolving toward, not the shape it happens to have today. If a module does not yet fit its target location, record the gap in DEBT.md with a migration trigger — do not invent a directory to legitimize the current state.

### 7.3 Module Doc on Every Subdirectory

Each new subdirectory's `mod.rs` MUST include a module-level doc comment (`//! …`) stating:
- What this module owns
- What belongs here and what does not

Directory names describe *what* the module is. Module doc describes *why* the contents are grouped and what should not be added. `cargo doc` and `rust-analyzer` surface it automatically.

### 7.4 Migration Exceptions Are Explicit

Areas not yet conforming to these principles live in [DEBT.md](DEBT.md), each with current state, target shape, and the trigger that will force migration. This lets new code follow the principles immediately without waiting for a repo-wide reorg.
