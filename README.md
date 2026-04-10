# ClawSolana

**Policy-gated transaction control plane for Solana.**

ClawSolana is a Rust-native daemon that sits between intent sources (AI agents, operators, automated services) and signing authorities (wallets, signers) on Solana. It enforces a compile-time-guaranteed pipeline — **simulate → policy → approve → sign → verify** — ensuring no transaction reaches a signer without passing simulation, policy evaluation, and (where required) human approval.

> **Status: Alpha.** 343 tests, zero warnings, Rust 1.94.0. Full message-driven Orca DeFi swap confirmed on Solana devnet ([swap tx](https://explorer.solana.com/tx/56yYDtW9TNv8GwWtBn8HFDEohcM3a4QcHQkpJoqmcgQ7AzACTQViciWdBBTZicowwm4KZk9dhQEBQ6ac9NCV4ovw?cluster=devnet)). NOT production-ready.

## Why This Matters

AI agents are getting access to wallets. Most agent-wallet integrations are one of two extremes: the agent holds the private key (zero governance), or a human approves every action (zero autonomy). Neither scales.

ClawSolana sits in the gap: **the agent proposes, the control plane governs, the human signs.**

- **DeFi treasuries are deploying agents** — they need policy rails, not a hot wallet and a prayer
- **Solana's 400ms slots** mean a rogue transaction lands before a human can react — pre-submission enforcement is the only viable control point
- **The signing boundary must be structural** — ClawSolana enforces it at compile time, not by convention

## What This Is / Is Not

| Is | Is Not |
|----|--------|
| Transaction control plane | A wallet |
| Policy enforcement layer | A signing proxy |
| Signing orchestrator (local + external wallets) | A frontend dApp |
| Append-only audit system | An RPC abstraction |
| Agent-safe execution boundary | Production-ready |

## Capabilities

| Layer | Status |
|-------|--------|
| Typestate pipeline (simulate → policy → approve → sign) | Compile-time enforced |
| TOML-driven policy rules (amount thresholds, allowlist, denylist) | Live |
| Per-session policy overrides (layered: session → global → defaults) | Live |
| Approval matrix (role-based approver gating) | Live |
| Policy observability (per-rule metrics, structured audit) | Live |
| Durable pending state (SQLite, restart-safe) | Live |
| Signature orchestrator (local + external, exactly-once) | Live |
| Wallet ownership proof (challenge-response) | Live |
| On-chain submission + lifecycle tracking | Live, devnet confirmed |
| Orca Whirlpool swap | Live, devnet confirmed |
| Rate limiting, compute budget optimization, retry/rebroadcast | Live |
| OpenAI + Anthropic LLM support | Live |

## Quick Start

```bash
cargo check                # verify build
cargo test                 # 343 tests
cat DEVELOPMENT.md         # current state + priorities
cat config/default.toml    # full config reference
```

## Documentation

| Doc | Purpose |
|-----|---------|
| [DEVELOPMENT.md](DEVELOPMENT.md) | Current state, priorities, session log, bridge test guide |
| [ARCHITECTURE.md](ARCHITECTURE.md) | System invariants, security posture, crate map, API surface |
| [ROADMAP.md](ROADMAP.md) | Phased plan (Phase 0-3) |

## Tech Stack

Rust (Edition 2021) / Tokio / Axum 0.7 / solana-sdk 2.1.21 / SQLite (SQLx 0.8) / ed25519-dalek 2 / DashMap 5 / tracing

## License

Private / Not yet licensed.
