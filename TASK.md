# ClawSolana — Task Tracker

**Updated:** 2026-03-21
**Build:** cargo check PASS, cargo test PASS (218 tests, zero failures)
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

---

## Current Priorities — Phase 1: Execution Completeness

> Wallet ownership proof and Phantom bridge are implemented. Next: formalize external signer mode and complete the on-chain execution path.

- [x] **A1: Wallet ownership proof** — ✅ Done (challenge-response, session-bound nonce, 9 tests)

- [x] **A2: Phantom browser bridge** — ✅ Done (bridge/index.html, Phantom signMessage + signTransaction)

- [ ] **A3: External signer formalization** — `SignerType::External` from deferred to live
  - Config can declare a wallet as external
  - Signing tool routes to external pending path (not local default signer)
  - Session binding + orchestrator routing works end-to-end

- [ ] **A4: End-to-end connectivity test** — manual or semi-automated
  - session → bind wallet (with challenge) → agent proposes tx → approval → external sign → verify → accepted

### Phase B — On-Chain Execution

- [ ] **B1: sendTransaction** — submit verified signed tx to Solana RPC
  - `send_raw_transaction` → get signature → audit → `TransactionSubmitted` event
- [ ] **B2: Confirmation tracking** — poll or websocket
  - Status: submitted → confirmed → finalized → failed/dropped/expired
- [ ] **B3: Basic retry / rebroadcast policy**
  - Same signed bytes can be resent; max retry count; blockhash expired = fail
  - No auto-resign or replacement tx in V1

### Phase C — Hardening & Operational Readiness

- [ ] **C1: Rate limiting** — per API token, per session, message size limit, 429 response
- [ ] **C2: Context trimming fix (N12)** — trim in tool_use/tool_result pairs, not individual entries
- [ ] **C3: Metrics / operational visibility** — basic counters for pending/signed/expired/rejected
- [ ] **C4: `bind_wallet` persistence** — if needed for cross-restart session continuity

### Remaining Phase 0 Items (fix alongside or before Phase A)

- [ ] **P0-3:** Session-request binding — verify session_id ownership before accepting wallet signatures
- [ ] **N13:** Two-pass compute budget optimization (simulate → tighten CU → re-simulate)
- [ ] Clean up compiler warnings (unused imports, missing docs)

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
