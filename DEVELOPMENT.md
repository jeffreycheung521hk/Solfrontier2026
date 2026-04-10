# ClawSolana — Development Guide

**Updated:** 2026-04-04
**Build:** `cargo check` PASS, `cargo test` PASS (343 tests, zero failures)
**Rust:** 1.94.0 stable
**Current phase:** Phase 2 — Policy Engine Expansion (2.1 done, 2.4 partial, 2.2 foundation)

---

## Quick Start

```bash
cargo check          # should be green
cargo test           # 343 tests, all green
cat ROADMAP.md       # phased plan
```

### Environment Variables

| Variable | Purpose |
|----------|---------|
| `OPENAI_API_KEY` | OpenAI API key (auto-detected) |
| `CLAW_LLM_API_KEY` | Anthropic/OpenAI API key (takes priority) |
| `CLAW_LLM_PROVIDER` | Override provider: `openai` or `anthropic` |
| `CLAW_API_TOKEN` | Bearer token for HTTP API auth (auto-generated if not set) |
| `CLAW_RPC_URL` | Override Solana RPC endpoint |

### Configuration

Copy `config/default.toml` to `claw.toml` and edit. See the default file for full documentation of all sections including `[policy]` rules, `[retry]`, `[rate_limit]`, and wallet declarations.

---

## Current State Summary

The core control plane — typestate pipeline, policy enforcement, capability boundaries, durable pending lifecycle, audit trail — is implemented and tested. On-chain submission and lifecycle tracking are complete with durable persistence and restart recovery. The agent-to-RPC pipeline has been **validated end-to-end on devnet**.

### Key Milestones

| Milestone | Date | Evidence |
|-----------|------|----------|
| A4: Devnet E2E transfer | 2026-03-30 | [tx](https://explorer.solana.com/tx/427TuF48fJGUsHDCgNETndGZwfg2sa85917YadZcJ852xUF2hya9GPf4XZEdhgkGDZUDeCTNFMAxTrPrQE2Q4P8F?cluster=devnet) |
| Orca swap E2E | 2026-04-01 | [tx](https://explorer.solana.com/tx/CPsdyFS7UTzZBZ6g9qUfKvoiA6YDobXKgyVDdcfGAW8zDzMVWc8QZN3MDGfLi7wUidQxEaMoZJWsiunt6o8X4Jb?cluster=devnet) |
| Message-driven Orca demo | 2026-04-02 | [tx](https://explorer.solana.com/tx/56yYDtW9TNv8GwWtBn8HFDEohcM3a4QcHQkpJoqmcgQ7AzACTQViciWdBBTZicowwm4KZk9dhQEBQ6ac9NCV4ovw?cluster=devnet) |
| TOML-driven policy engine | 2026-04-04 | 343 tests |
| Policy observability (audit + per-rule metrics) | 2026-04-04 | `PolicyEvaluationResult` + `CounterMap` |
| Per-session policy overrides | 2026-04-04 | `SessionPolicyStore` + layered evaluation |
| Approval matrix (role-based) | 2026-04-04 | `required_approver_role` + role validation |

### Completed Work

| Area | Status |
|------|--------|
| Typestate pipeline (simulate → policy → approve → sign) | Compile-time enforced |
| Policy engine (TOML rules, allowlist, denylist, amount thresholds) | Live |
| Per-session policy overrides (session rules → global → defaults) | Live |
| Approval matrix (required_approver_role, role mismatch → 403) | Live |
| Policy observability (per-rule metrics, structured audit, `PolicyEvaluationResult`) | Live |
| Durable pending state (SQLite lifecycle, restart recovery) | Live |
| Signature orchestrator (local + external, exactly-once) | Live |
| Wallet ownership proof (challenge-response, session-bound nonce) | Live |
| External signer formalization (config-declared, adapter chain) | Live |
| On-chain submission + lifecycle tracking | Live, devnet confirmed |
| Retry/rebroadcast (signed_tx_bytes wired, configurable policy) | Live |
| Rate limiting (per-token sliding window, 429 + Retry-After) | Live |
| Metrics (20 static + dynamic per-rule counters, `GET /metrics`) | Live |
| Two-pass compute budget (simulate → tighten CU → re-simulate) | Live |
| Orca Whirlpool swap (manual IX, wSOL wrap, tick array PDA) | Live, devnet confirmed |
| OpenAI + Anthropic LLM support (auto-detect) | Live |

### Current Invariants

1. **No `request_id` without durable backing** — any externally visible pending state has SQLite row
2. **Durable-first create** — DB write succeeds BEFORE in-memory state + response
3. **Mark-terminal-before-remove** — DB row marked terminal BEFORE in-memory removal
4. **Consume-on-attempt** — `complete()` atomically removes pending entry before verification
5. **Fail-closed on persist failure** — no in-memory-only downgrade on durable write failure
6. **Human signs, AI proposes** — no agent role grants `SignTransaction` or `SendTransaction`

---

## Current Priorities

### Phase 2.1 — Granular Policy Rules ✅
- [x] Amount thresholds, destination denylist, program allowlist
- [x] TOML-driven `[[policy.rules]]` with first-match-wins
- [x] Per-session policy overrides (session rules → global rules → defaults)

### Phase 2.2 — Approval Matrix (foundation done)
- [x] `required_approver_role` on `RequireHumanApproval` action
- [x] Approver role validation on `POST /sessions/:id/approve` (403 on mismatch)
- [ ] Multi-step approval chain (requires architectural changes to oneshot channel)
- [ ] Quorum-based approval

### Phase 2.4 — Policy Observability (partial)
- [x] Policy evaluation audit trail (all verdict branches)
- [x] Per-rule hit-rate metrics (`CounterMap`, `GET /metrics`)
- [x] `PolicyEvaluationResult` with evaluation metadata
- [ ] Policy violation alerting (webhook/event on rejection)

### Remaining
- [ ] Per-session policy overrides persistence (currently in-memory only)
- [ ] Time-based policy rules (business hours, maintenance windows)

---

## Phantom Bridge — Local Test Flow

The Phantom browser bridge is a minimal HTML/JS signing client at `bridge/index.html`.

### Setup

```bash
# 1. Start daemon
export $(grep -v '^#' .env | grep -v '^\s*$' | xargs)
cargo run --bin clawd -- --config config/default.toml

# 2. Create a session
curl -s -X POST http://127.0.0.1:7070/sessions \
  -H "Authorization: Bearer $CLAW_API_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"role": "execution", "channel": "cli"}'

# 3. Serve bridge
cd bridge && python -m http.server 8080
# Open http://localhost:8080 in browser with Phantom
```

### Bridge Flow

1. Paste session_id and bearer token
2. **Connect Wallet** — Phantom popup
3. **Bind Wallet** — Phantom signs challenge message (ownership proof)
4. **Refresh** — shows pending signature requests
5. **Sign** — Phantom signs the pending transaction

### Bridge Limitations

- No error recovery UI, no auto-polling
- Single session, Phantom only (no WalletConnect)
- No transaction preview (amount/recipient) before signing

### Dependencies

Uses ESM CDN imports (no build step): `@solana/web3.js` + Phantom's `window.solana`.

---

## Session Log

### Session 20 — 2026-04-04 (Phase 2: Policy Engine + Observability + Approval Matrix)

- **343 tests**, zero failures
- TOML-driven policy rules with first-match-wins evaluation
- `AmountExceedsLamports`, `ProgramNotInAllowlist`, `DestinationInDenylist` conditions
- `summarize_transaction_instructions()` — decode System Program transfers from unsigned tx
- Per-rule hit-rate metrics (`CounterMap` in MetricsRegistry)
- Policy evaluation audit on all verdict branches with `PolicyEvaluationResult` metadata
- Per-session policy overrides (`SessionPolicyStore`, layered evaluation)
- Approval matrix: `required_approver_role` + role validation + `RoleMismatch` outcome

### Session 19 — 2026-04-01→02 (Orca Swap + Full Demo)

- 300+ tests, Orca swap E2E on devnet, message-driven demo
- Rate limiting, two-pass compute budget, bind_wallet persistence, warnings cleanup

### Session 17–18 — 2026-03-30→31 (A4 Devnet E2E + Hardening)

- A4 E2E confirmed on devnet, B3 rebroadcast wiring fix, C3 metrics wired, `/metrics` endpoint

### Session 16 — 2026-03-29 (Toolchain + Agent Context)

- Rust 1.82.0 → 1.94.0, wallet context injection, P0-3 session binding, C2 context trimming

### Sessions 1–15 — Foundation

- N1–N8 (capability narrowing, approval round-trip, SSE, wallet loading, sign-path resume, spend tracking)
- Orchestrator + durability (atomic completion, expiry reaper, durable pending, terminal purge)
- Wallet ownership proof, Phantom bridge, external signer formalization, OpenAI LLM, CORS
