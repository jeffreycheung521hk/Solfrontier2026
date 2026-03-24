# ClawSolana — Task Tracker

**Updated:** 2026-03-24
**Build:** cargo check PASS, cargo test PASS (276 tests, zero failures)
**Rust:** 1.94.0 stable
**Roadmap:** See [ROADMAP.md](ROADMAP.md)

---

## Completed

### Sessions 1–10: N1–N8 Foundation
- [x] P1–P7: Security baseline (typestate pipeline, audit, tool boundary, capability enforcement, no unsafe)
- [x] N1: Per-session capability narrowing — `CapabilitySet::for_role(AgentRole)` with 11 tests
- [x] N2: Human approval round-trip — `ApprovalStore` + API routes + audit + events
- [x] N3: SSE event stream — session-scoped filtering, lag handling, 30s heartbeat
- [x] N4: Wallet loading at startup — `LocalKeypair` from file or base58
- [x] N5: Sign-path resume — park/signal/take + background signing task
- [x] N6: Capability wiring + integration smoke test (7 tests)
- [x] N7: Spend tracking accumulator — SQLite spend_ledger, INSERT OR IGNORE dedup, 7 tests
- [x] N8: Native tool_result blocks — structured ContentBlock types, 8 tests
- [x] N9: Compute budget prepend — idempotent, single-pass before simulation, 6 tests
- [x] N10A–F: External signer bridge — session-wallet binding, ed25519 verification (4-step), HTTP bridge simulation, daemon integration, production readiness review

### Sessions 13+: Orchestrator & Durability
- [x] N15: Signature Orchestrator — adapter dispatch, pending lifecycle, exactly-once semantics, 12 tests
- [x] STEP 5: Orchestrator integration — SubmitForSigningTool + resume + handler routed through orchestrator
- [x] STEP 6: Orchestrator integration tests — 13 tests (auto/human × local/external, race, expiry)
- [x] P0-1: Atomic completion — `SignatureOrchestrator.complete()` via atomic `DashMap::remove()`
- [x] P0-2: Request expiry reaper — 30s interval, 120s TTL, `WalletSignatureExpired` events
- [x] P1-1: Durable pending state — SQLite lifecycle state, startup recovery, durable-first create
- [x] P1-1 hardening: mark-terminal-before-remove, `purge_terminal()`, no fire-and-forget writes
- [x] Semantic cleanup: `abort_pending()` for rollback vs `expire()` for natural TTL
- [x] N1–N8 foundation audit — 38 targeted tests proving no regression
- [x] A1: Wallet ownership proof — challenge-response with session-bound nonce, 9 tests (incl. cross-session hijack)
- [x] A2: Phantom browser bridge — bridge/index.html (connect, bind, sign, submit)
- [x] OpenAI LLM support — auto-detects OPENAI_API_KEY, provider config field, 7 tests
- [x] CORS enabled on API server for local bridge development
- [x] Cross-session binding hijack fix — verify_challenge checks session_id

### Session 16: Toolchain + E2E Validation + Agent Wallet Context
- [x] Rust toolchain upgrade — 1.82.0 → 1.94.0 stable, `rust-toolchain.toml`
- [x] OpenSSL Win64 setup for `openssl-sys` dependency
- [x] P0-3: Session-request binding — verify session_id ownership before accepting signatures
- [x] C2: Context trimming fix — trim tool_use/tool_result in pairs, not individually
- [x] B3 foundation: Retry/rebroadcast policy structs + tracker integration
- [x] Wallet context injection — agent system prompts include loaded wallet pubkeys
- [x] Keygen utility — `bins/keygen` binary + `scripts/gen_keypair.rs`
- [x] E2E live validation — GPT agent queried devnet wallet balance (0.8 SOL) successfully

---

## Current Priorities — Phase 1: Execution Completeness

> Toolchain upgraded, agent wallet context fixed, E2E pipeline validated on devnet. Next: formalize external signer and complete retry policy.

- [x] **A1: Wallet ownership proof** — ✅ Done (challenge-response, session-bound nonce, 9 tests)

- [x] **A2: Phantom browser bridge** — ✅ Done (bridge/index.html, Phantom signMessage + signTransaction)

- [ ] **A3: External signer formalization** — `SignerType::External` from deferred to live
  - Config can declare a wallet as external with pubkey only
  - `config_bound_pubkeys` in `ExternalWalletStore` for operator-declared wallets
  - Signing tool routes to external pending path (not local default signer)

- [ ] **A4: End-to-end connectivity test** — manual or semi-automated
  - session → bind wallet (with challenge) → agent proposes tx → approval → external sign → verify → accepted

### Phase B — On-Chain Execution

- [x] **B1: sendTransaction** — ✅ Done (`send_raw_transaction` → signature → audit → `TransactionSent` event)
- [x] **B2: Confirmation tracking** — ✅ Done (adaptive RPC polling, lifecycle state machine, durable persistence, restart recovery)
- [~] **B3: Basic retry / rebroadcast policy** — foundation in place (RetryPolicy struct, tracker integration), needs wiring from config
  - Same signed bytes can be resent; max retry count; blockhash expired = fail
  - No auto-resign or replacement tx in V1

### Phase C — Hardening & Operational Readiness

- [ ] **C1: Rate limiting** — per API token, per session, message size limit, 429 response
- [x] **C2: Context trimming fix (N12)** — ✅ Done (trim in tool_use/tool_result pairs)
- [ ] **C3: Metrics / operational visibility** — basic counters for pending/signed/expired/rejected
- [ ] **C4: `bind_wallet` persistence** — if needed for cross-restart session continuity

### Remaining Phase 0 Items

- [x] **P0-3:** Session-request binding — ✅ Done (verify session_id ownership)
- [ ] **N13:** Two-pass compute budget optimization (simulate → tighten CU → re-simulate)
- [ ] Clean up compiler warnings (unused imports, missing docs — 39 warnings remaining)

---

## Deferred (Not Current Priority)

### Phase 1+ (after Phase A/B/C)
- Orca swap execution (currently read-only tools only)
- Full CL tick-array quote (V1 uses constant product approximation)
- `DeFiAction` trait + `ProtocolAdapter` abstraction
- Multi-protocol support (Solend, Jupiter)

### Phase 2+
- Cross-protocol risk engine
- Per-session wallet selection, Ledger signer support
- Autonomous observers, cron/scheduler

### Phase 3+
- Intent parsing (fuzzy text → DeFi actions)
- Telegram / voice channel adapters
- Emergency Escape Pod
