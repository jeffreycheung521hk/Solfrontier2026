# ClawSolana

**Alpha-stage Solana signing orchestration gateway — Rust-first, approval-gated, restart-safe.**

ClawSolana is a local daemon (`clawd`) that connects an AI agent (Claude / Anthropic LLM) to the Solana blockchain via a typed tool system, a policy-gated transaction pipeline, and a durable signature orchestration layer. Users interact via HTTP API or CLI client (`claw`).

> **Status: Alpha.** Core orchestration, approval flow, durability, and capability boundaries are implemented and tested. Real external wallet connectivity (Phantom bridge) is still under active development. This is not a production-ready wallet product.

---

## Project Status

| Metric | Value |
|--------|-------|
| **Build** | `cargo check` PASS |
| **Tests** | 200 tests, zero failures |
| **Last updated** | 2026-03-21 |
| **Stage** | Alpha — architecture prototype with real test coverage |
| **Production readiness** | NOT PRODUCTION-READY |

### What Works

- Compile-time enforced simulation-before-signing (typestate pipeline)
- Per-session capability narrowing by agent role (no role grants `SignTransaction`)
- Human-in-the-loop approval with background signing resume
- Signature orchestrator with adapter routing (local + external wallet adapters)
- Durable pending state lifecycle (SQLite-backed, survives daemon restart)
- Durable-first semantics: no `request_id` exposed without durable backing
- Startup recovery with TTL-based expiry of stale pending requests
- Exactly-once completion semantics via atomic consume-on-attempt
- Full audit trail (append-only SQLite)
- Spend tracking with double-count protection (`INSERT OR IGNORE`)
- Native Anthropic tool_use/tool_result content blocks (structurally isolated)
- SSE event stream with session-scoped filtering
- Ed25519 verification for external wallet signatures (4-step check)
- Compute budget instruction prepend (idempotent, single-pass)
- Orca Whirlpool read-only tools (pool info, quote, validation)
- API token hardening, `#![forbid(unsafe_code)]` in critical crates
- Periodic terminal-row purge (configurable retention)

### What Is Not Yet Complete

- **External wallet connectivity** — the Phantom bridge protocol is implemented at the verification layer, but end-to-end browser-to-daemon wallet-connect flow is not yet finished
- **On-chain transaction submission** — `sendTransaction` + retry + confirmation tracking not yet implemented
- **Rate limiting** — no rate limiting on API endpoints
- **Wallet binding ownership proof** — `bind_wallet` accepts pubkey without challenge-response
- **Context trimming** — `trim_if_needed()` can orphan tool_use blocks without their tool_result pairs
- **Ledger / hardware signer support** — only `LocalKeypair` signer type is supported
- **Multi-protocol DeFi execution** — Orca tools are read-only; swap/LP execution is Phase 1

---

## Architecture

```
claw-types (zero deps) ← shared vocabulary
    ↓
claw-observability, claw-state-store, claw-solana-core
    ↓
claw-wallet-engine, claw-risk-engine, claw-tool-system
    ↓
claw-agent-runtime, claw-channels, claw-api
    ↓
claw-gateway (orchestrator) → SessionManager, EventBus, SignatureOrchestrator
    ↓
bins/clawd (daemon), bins/claw (CLI client)
```

### Key architectural boundaries

| Boundary | Control Plane | Execution Plane |
|----------|--------------|-----------------|
| **Owns** | Simulation, policy, approval, audit, spend, events | Wallet adapters, signing, verification, pending lifecycle |
| **Store** | ApprovalStore, PendingSigningStore | SignatureOrchestrator pending map |
| **Durability** | DurablePendingState (SQLite write-through) | Orchestrator recovery via `recover_pending()` |

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
| `POST` | `/sessions/:id/bind-wallet` | Yes | Bind external wallet |
| `POST` | `/sessions/:id/wallet-signatures` | Yes | Submit signed tx |
| `GET` | `/sessions/:id/wallet-signatures` | Yes | List pending sign requests |

### Capability Model

```
AgentRole::Research  → ReadWallet, ReadChain, SimulateTransaction
AgentRole::Execution → + BuildTransaction, ProposeSigning
AgentRole::Risk      → ReadChain, ReadWallet, ManageSubscriptions
AgentRole::Ops       → All except SignTransaction, SendTransaction
```

`SignTransaction` and `SendTransaction` are **never** granted to any agent role. Signing authority stays in gateway infrastructure.

---

## Quick Start

```bash
# Clone and verify build
cargo check

# Run all tests (200 tests, ~1 second)
cargo test

# Review project status
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
model = "claude-sonnet-4-6"
# api_key via CLAW_LLM_API_KEY env var

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
| `CLAW_LLM_API_KEY` | Anthropic Claude API key |
| `CLAW_API_TOKEN` | Bearer token for HTTP API auth (auto-generated if not set) |

---

## Security Highlights

- **Typestate pipeline** — compile-time enforced: simulate → policy → approve → sign
- **`#![forbid(unsafe_code)]`** in wallet-engine, types, observability, state-store, and more
- **Append-only audit** — no update/delete methods on `AuditRepository`
- **API local-only** — hard-coded bind to `127.0.0.1`, never `0.0.0.0`
- **Ed25519 verification** — 4-step check for external wallet signatures
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
| Wallet binding auth | Open | No challenge-response for `bind_wallet` |
| Event durability | Open | Events are fire-and-forget broadcast, no replay |
| On-chain submission | Open | `sendTransaction` not yet implemented |
| External wallet E2E | Open | Verification layer done; browser bridge not finished |

---

## Project Structure

```
ClawSolana/
├── bins/
│   ├── claw/                    # CLI client binary
│   └── clawd/                   # Daemon binary
├── crates/
│   ├── agent-runtime/           # ReAct loop, LLM client, personas
│   ├── api/                     # Axum HTTP server, auth, routes
│   ├── channels/                # Channel adapters
│   ├── gateway/                 # Control plane, orchestrator, durable pending state
│   │   └── tests/               # Integration tests (n6, n7, n10, n15, n16, n18)
│   ├── observability/           # Logging, metrics, health
│   ├── risk-engine/             # Policy evaluation, spend tracking
│   ├── solana-core/             # RPC, simulation, blockhash, compute
│   ├── state-store/             # SQLite persistence (audit, spend, pending state)
│   ├── tool-system/             # Tool registry, dispatch, capabilities
│   │   └── tools/protocols/     # Protocol-specific tools (Orca)
│   ├── types/                   # Shared domain types
│   └── wallet-engine/           # Signing pipeline, keystore
├── config/
│   └── default.toml             # Default configuration
├── docs/
│   └── threat-model.md          # Security threat model
├── scripts/
│   └── simulate_bridge.sh       # Bridge simulation script
├── Cargo.toml                   # Workspace root
├── TASK.md                      # Task tracker
├── PROGRESS.md                  # Session-by-session development log
├── HANDOFF.md                   # Session handoff status
└── PRODUCTION_READINESS_REVIEW.md
```

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
