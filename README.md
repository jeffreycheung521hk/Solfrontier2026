# ClawSolana

**Policy-gated transaction control plane for Solana.**

ClawSolana is a Rust-native daemon that sits between intent sources (AI agents, operators, automated services) and signing authorities (wallets, signers) on Solana. It enforces a compile-time-guaranteed pipeline — **simulate → policy → approve → sign → verify** — ensuring no transaction reaches a signer without passing simulation, policy evaluation, and (where required) human approval.

The system is designed so that entities proposing transactions **never hold signing authority**, and entities holding signing authority **never see a transaction the control plane hasn't vetted**.

> **Status: Alpha.** Core control plane, policy enforcement, durable pending lifecycle, capability boundaries, on-chain submission, and transaction lifecycle tracking are implemented and tested (220+ tests). Retry/rebroadcast policy and external signer formalization are under active development. This is not a production-ready wallet product.

---

## What This System Is

- A **transaction control plane** — governs which transactions proceed, under what conditions, with what approvals
- A **policy enforcement layer** — evaluates spend limits, approval rules, and capability boundaries before any transaction reaches a signer
- A **signing orchestrator** — routes approved transactions to the correct signing authority (local keypair, external wallet, future: MPC/hardware)
- An **audit system** — every proposal, simulation, policy decision, approval, and signature is recorded to append-only SQLite
- An **agent-safe execution boundary** — AI agents can propose actions but are structurally prevented from signing or sending transactions

## What This System Is Not

- **Not a wallet** — does not hold, manage, or custody private keys in the wallet-product sense
- **Not a signing proxy** — a proxy forwards requests; ClawSolana *evaluates* requests against policy, *gates* them through approval, *tracks* spend, and *audits* every step
- **Not a frontend dApp** — backend daemon with HTTP API; the planned Phantom bridge is a minimal signing client, not a user-facing application
- **Not an RPC abstraction** — uses RPC for simulation and (planned) submission, not as a general proxy

## How It Differs

| System | Role | ClawSolana's relationship |
|--------|------|--------------------------|
| **Wallet** (Phantom, Backpack) | Holds keys, signs when user approves | ClawSolana is **upstream** — decides which transactions should reach a wallet |
| **RPC endpoint** (Helius, Triton) | Submits transactions to Solana | ClawSolana is **upstream** — decides which signed transactions should be submitted |
| **Signer service** (Turnkey, Fireblocks) | Manages key material, signs on request | ClawSolana is **upstream** — governs which signing requests are issued |
| **Agent framework** (LangChain, AutoGPT) | Generates intents and tool calls | ClawSolana is **downstream** — receives proposals from agents and subjects them to the full pipeline |

---

## Project Status

| Metric | Value |
|--------|-------|
| **Build** | `cargo check` PASS |
| **Tests** | 200+ tests, zero failures |
| **Last updated** | 2026-03-21 |
| **Stage** | Alpha — infrastructure prototype with real test coverage |
| **Production readiness** | NOT PRODUCTION-READY |

### Current Capabilities

| Layer | Capability | Status |
|-------|-----------|--------|
| **Pipeline** | Typestate-enforced simulate → policy → approve → sign | Compile-time guaranteed |
| **Policy** | Spend caps, daily limits, mainnet-safe defaults, human-approval rules | Live |
| **Approval** | Async human-in-the-loop with background resume, one-time-actionable | Live, durable-first |
| **Orchestrator** | Local + external wallet adapter routing, exactly-once completion | Live |
| **Durability** | SQLite-backed pending state with lifecycle (pending/consumed/expired/rejected) | Live, restart-safe |
| **Capability** | Per-session role narrowing; no role grants SignTransaction or SendTransaction | Live |
| **Spend** | Cumulative tracking with INSERT OR IGNORE dedup, policy-aware | Live |
| **Audit** | Append-only SQLite, every proposal/simulation/decision/signature recorded | Live |
| **Events** | SSE stream, session-scoped filtering, heartbeat | Live |
| **Verification** | 4-step ed25519 check for external wallet signatures | Live |
| **Wallet ownership** | Challenge-response proof before wallet binding (ed25519 nonce signing) | Live |
| **Phantom bridge** | Minimal browser signing page (connect, bind, sign, submit) | Live (alpha) |
| **LLM providers** | OpenAI (GPT-4o) and Anthropic (Claude) with auto-detection | Live |

### What Is Not Yet Complete

- **On-chain submission** — `sendTransaction` and lifecycle tracking are implemented; retry/rebroadcast policy not yet added
- **Rate limiting** — no rate limiting on API endpoints
- **Configurable policy rules** — current policy is code-defined; no runtime policy configuration
- **Context trimming** — `trim_if_needed()` can orphan tool_use blocks without their tool_result pairs
- **Multi-signer orchestration** — single signer per request only
- **External signer formalization** — `SignerType::External` config mode not yet live in daemon

---

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                     INTENT SOURCES                          │
│   AI Agents  │  Operators  │  Services  │  Dashboards       │
└──────┬───────────┬─────────────┬──────────────┬─────────────┘
       │           │             │              │
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
       │                                  │
       ▼                                  ▼
┌──────────────────┐          ┌────────────────────────┐
│  LOCAL WALLET     │          │  EXTERNAL WALLET        │
│  ADAPTER          │          │  ADAPTER                │
│  (SecretKeystore, │          │  (unsigned tx → client, │
│   sync sign)      │          │   client signs,         │
│                   │          │   4-step ed25519 verify) │
└──────┬────────────┘          └───────────┬─────────────┘
       │                                   │
       ▼                                   ▼
┌─────────────────────────────────────────────────────────────┐
│                    SOLANA NETWORK                            │
└─────────────────────────────────────────────────────────────┘
```

### Crate Overview (13 core + 2 binaries)

| Crate | Purpose |
|-------|---------|
| `claw-types` | Shared domain types (zero dependencies) |
| `claw-observability` | Logging, metrics, health checks |
| `claw-state-store` | SQLite persistence (audit, spend, traces, pending state) |
| `claw-solana-core` | RPC client, simulation, blockhash, compute budget, fees |
| `claw-wallet-engine` | Typestate signing pipeline, keystore, signer traits |
| `claw-risk-engine` | Policy evaluation, spend cap rules |
| `claw-tool-system` | Tool registry, dispatch, capability enforcement |
| `claw-agent-runtime` | ReAct loop, LLM client (Anthropic), context management |
| `claw-channels` | Channel adapters (CLI; future: Telegram, Slack) |
| `claw-api` | Axum HTTP server, auth, route handlers |
| `claw-gateway` | Control plane daemon, session management, event bus, signature orchestrator, durable pending state |
| `clawd` | Daemon binary entry point |
| `claw` | CLI client binary |

### Transaction Pipeline (Typestate-Enforced)

```
TransactionProposal
  → prepend_compute_budget() (idempotent)
  → simulate() → SimulatedTransaction
  → evaluate_policy() → ApprovedTransaction (or RequiresHumanApproval)
  → SignatureOrchestrator.submit() → Signed | Pending
```

Each stage transition is enforced at compile time. You cannot call `sign()` without a valid `ApprovedTransaction`.

### Capability Model

```
AgentRole::Research  → ReadWallet, ReadChain, SimulateTransaction
AgentRole::Execution → + BuildTransaction, ProposeSigning
AgentRole::Risk      → ReadChain, ReadWallet, ManageSubscriptions
AgentRole::Ops       → All except SignTransaction, SendTransaction
```

`SignTransaction` and `SendTransaction` are **never** granted to any agent role. Signing authority stays in gateway infrastructure.

### HTTP API Surface (127.0.0.1:7070)

| Method | Path | Auth | Purpose |
|--------|------|------|---------|
| `GET` | `/health` | No | Health check |
| `POST` | `/sessions` | Yes | Create session |
| `GET` | `/sessions` | Yes | List sessions |
| `DELETE` | `/sessions/:id` | Yes | Close session |
| `POST` | `/sessions/:id/messages` | Yes | Send message to agent |
| `GET` | `/sessions/:id/events` | Yes | SSE event stream |
| `POST` | `/sessions/:id/approve` | Yes | Approve/reject pending tx |
| `GET` | `/sessions/:id/approvals` | Yes | List approval requests |
| `POST` | `/sessions/:id/bind-wallet` | Yes | Bind external wallet (legacy, no proof) |
| `POST` | `/sessions/:id/wallet-bind-challenge` | Yes | Request wallet ownership challenge |
| `POST` | `/sessions/:id/wallet-bind-confirm` | Yes | Submit signed challenge + bind |
| `POST` | `/sessions/:id/wallet-signatures` | Yes | Submit signed tx |
| `GET` | `/sessions/:id/wallet-signatures` | Yes | List pending sign requests |

---

## Roadmap

See [ROADMAP.md](ROADMAP.md) for the full phased plan.

| Phase | Focus | Status |
|-------|-------|--------|
| **Phase 0** | Production readiness — typestate pipeline, durability, orchestrator | **Near complete** |
| **Phase 1** | Execution completeness — Phantom bridge, wallet proof, sendTransaction, lifecycle tracking | **In progress** |
| **Phase 2** | Policy engine expansion — configurable rules, approval matrix, signer constraints | Planned |
| **Phase 3** | Enterprise control plane — multi-signer, MPC integration, treasury workflows | Planned |

---

## Quick Start

```bash
# Clone and verify build
cargo check

# Run all tests (200+ tests, ~1 second)
cargo test

# Review current priorities
cat TASK.md
```

### Configuration

Copy `config/default.toml` to `claw.toml` and edit:

```toml
[daemon]
db_path = "./data/claw.db"
# terminal_retention_days = 14       # how long to keep terminal pending rows
# terminal_purge_interval_hours = 6  # periodic cleanup frequency

[network]
network = "devnet"    # devnet | testnet | mainnet-beta | localnet

[rpc]
primary_url = "https://api.devnet.solana.com"
timeout_ms = 15000

# [[wallets]]
# label = "devnet-hot-wallet"
# signer_type = "local_keypair"
# keypair_path = "~/.config/solana/devnet.json"

[policy]
mainnet_safe_defaults = true

[llm]
provider = "openai"           # "openai" or "anthropic"
model = "gpt-4o"              # or "claude-sonnet-4-6" for Anthropic
# api_key via OPENAI_API_KEY or CLAW_LLM_API_KEY env var

[api]
bind_addr = "127.0.0.1"
port = 7070

[logging]
format = "pretty"
level = "info"
```

### Environment Variables

| Variable | Purpose |
|----------|---------|
| `OPENAI_API_KEY` | OpenAI API key (auto-detected, sets provider to `openai`) |
| `CLAW_LLM_API_KEY` | Anthropic Claude API key (sets provider to `anthropic`) |
| `CLAW_LLM_PROVIDER` | Override LLM provider: `openai` or `anthropic` |
| `CLAW_API_TOKEN` | Bearer token for HTTP API auth (auto-generated if not set) |
| `CLAW_RPC_URL` | Override Solana RPC endpoint |

---

## Security Highlights

- **Typestate pipeline** — compile-time enforced: simulate → policy → approve → sign
- **`#![forbid(unsafe_code)]`** in wallet-engine, types, observability, state-store, and more
- **Append-only audit** — no update/delete methods on `AuditRepository`
- **API local-only** — hard-coded bind to `127.0.0.1`, never `0.0.0.0`
- **Ed25519 verification** — 4-step check for external wallet signatures
- **Wallet ownership proof** — challenge-response with session-bound nonce before binding
- **Tool/LLM boundary** — tool outputs structurally isolated in `tool_result` content blocks with untrusted-data prefix
- **Spend double-count protection** — `UNIQUE(transaction_id)` + `INSERT OR IGNORE`
- **Capability dispatch** — fail-closed; unknown capabilities rejected
- **Durable-first pending state** — no externally visible `request_id` without SQLite backing
- **Consume-on-attempt** — pending entries atomically removed before verification, preventing double-complete

---

## Known Limitations

See [PRODUCTION_READINESS_REVIEW.md](PRODUCTION_READINESS_REVIEW.md) for full details.

| Area | Status | Notes |
|------|--------|-------|
| Atomic completion | Resolved | `SignatureOrchestrator.complete()` uses atomic `DashMap::remove()` |
| Request expiry | Resolved | Reaper task with 120s TTL, periodic execution |
| Pending state durability | Resolved | SQLite-backed with lifecycle state (`pending`/`consumed`/`expired`/`rejected`) |
| Session-request binding | Open | Submit endpoint does not verify session ownership |
| Rate limiting | Open | No rate limiting on API endpoints |
| Wallet binding auth | Resolved | Challenge-response for `bind_wallet` with session-bound nonce |
| Event durability | Open | Events are fire-and-forget broadcast, no replay |
| On-chain submission | Implemented | `send_raw_transaction` + lifecycle tracking (Submitted → Confirmed → Finalized / Failed / Expired) |
| Retry / rebroadcast | Open | Same-bytes rebroadcast policy not yet added |
| External wallet E2E | Partial | Verification layer + browser bridge done; needs real devnet E2E test |

---

## Tech Stack

| Category | Technologies |
|----------|-------------|
| **Language** | Rust (Edition 2021) |
| **Async runtime** | Tokio 1.x |
| **Web framework** | Axum 0.7 + Tower 0.4 |
| **Solana SDK** | solana-sdk 2.1.21, solana-client, SPL tokens |
| **Database** | SQLite via SQLx 0.8 (async, WAL mode) |
| **Serialization** | Serde, serde_json, bincode, base64, bs58 |
| **Crypto** | ed25519-dalek 2 |
| **Observability** | tracing + tracing-subscriber |
| **LLM** | Anthropic Claude (via reqwest) |
| **CLI** | clap 4 |
| **Concurrency** | DashMap 5, parking_lot 0.12 |

---

## License

Private / Not yet licensed.
