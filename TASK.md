# ClawSolana — Task Tracker

**Updated:** 2026-03-21
**Build:** cargo check PASS, cargo test PASS (200 tests, zero failures)
**Roadmap:** See [ROADMAP.md](ROADMAP.md) — North Star: Autonomous & Intent-Based DeFi Orchestrator

---

## Completed (Sessions 1–10)

- [x] P1–P7: Security baseline (typestate pipeline, audit, tool boundary, capability enforcement, no unsafe)
- [x] N1: Per-session capability narrowing — `CapabilitySet::for_role(AgentRole)` with 9 tests
- [x] N2: Human approval round-trip — `ApprovalStore` + `POST /approve` + `GET /approvals` + audit + events
- [x] N3: SSE event stream — `GET /sessions/:id/events` with session-scoped filtering, lag handling, 30s heartbeat
- [x] N4: Wallet loading at startup — `LocalKeypair` from file or base58 into `SecretKeystore`
- [x] N5: Sign-path resume — `SubmitForSigningTool` + `PendingSigningStore` + `ApprovalMode::HumanGranted` + background signing task
- [x] STEP 4: Orca read-only tools — `orca_get_whirlpool`, `orca_get_quote`, `orca_check_pool` with 5 tests
- [x] N6: Capability wiring fix (`ProposeSigning`) + integration smoke test (7 tests) — approve/reject/duplicate/typestate/capability/events/audit
- [x] N7: Spend tracking accumulator — SQLite `spend_ledger`, `SpendRepository`, wired into signing tool + policy eval, 7 tests
- [x] N8: Native tool_result blocks — structured `ContentBlock` types, Anthropic-native tool_use/tool_result blocks, 8 new tests
- [x] N9: Compute budget prepend — `prepend_compute_budget()`, idempotent, single-pass before simulation, finalized tx in AwaitingApproval, 6 new tests
- [x] N10A/N11: External signer bridge — `ExternalWalletStore`, session-wallet binding, signed tx verification (message bytes + signer), `WalletSignatureHandler` trait + API routes, audit + SSE events, 10 new tests
- [x] N10B: External signer hardening — signature index fix, rejection audit logs, transaction_id in rejection events, pending_for_session implemented, blockhash/CU tampering tests, 3 new tests
- [x] N10C: Verification layer — `verify_signed_tx()` with 4-step ed25519, `submit_signed_transaction()` store-level handler, `VerifyError`/`SubmitError` types, 8 new tests in n10a
- [x] N10D: HTTP bridge simulation — 3 HTTP round-trip tests (happy path, tampered message, wrong signer) via axum Router + tower::ServiceExt
- [x] N10E: Daemon integration tests — 3 tests with real SQLite audit/spend + EventBus (concurrent race exactly-once, restart resilience, full approval→external sign flow)
- [x] N10F: Production readiness review — P0/P1/P2 issue registry, V2 architecture proposal (persistent store, idempotency keys, event durability, reaper, metrics)
- [x] N15: Signature Orchestrator skeleton — `SignatureOrchestrator`, `WalletAdapter` trait, `LocalWalletAdapter`, `ExternalWalletAdapter`, `ExternalWalletBindings` read-only trait, pending-only lifecycle, exactly-once semantics via atomic `DashMap::remove()`, `DuplicateRequestId` rejection, consume-on-attempt `complete()`/`expire()`, reaper support (`expired_candidates()`), 12 new tests
- [x] STEP 5: Orchestrator integration — `SubmitForSigningTool`, `resume_after_approval`, `submit_signed_tx_inner` routed through `SignatureOrchestrator`; expiry reaper wired in daemon
- [x] STEP 6: Orchestrator integration tests — 13 tests (auto/human approved × local/external, tampered completion, race, expiry, metadata cleanup)
- [x] P0-1: Atomic completion — `SignatureOrchestrator.complete()` via atomic `DashMap::remove()` (consume-on-attempt)
- [x] P0-2: Request expiry reaper — Tokio interval task (30s check, 120s TTL), `WalletSignatureExpired` events
- [x] P1-1: Durable pending state — SQLite tables with explicit lifecycle state (`pending`/`consumed`/`expired`/`rejected`), startup recovery, durable-first create semantics, atomic signature+meta persist, `abort_pending()` rollback
- [x] P1-1 hardening: mark-terminal-before-remove ordering, `purge_terminal()` with 14-day configurable retention, startup + periodic purge, no fire-and-forget writes
- [x] Semantic cleanup: `abort_pending()` for rollback vs `expire()` for natural TTL, `persist_signature_and_meta()` atomic DB transaction
- [x] N1–N8 foundation audit — 38 targeted tests proving no regression from orchestrator/durability work

---

## Current Phase: Phase 0 — Production Readiness & V1 Foundation

> After completing Phase 0, we proceed strictly to Phase 1 per [ROADMAP.md](ROADMAP.md).

### Remaining P0 Items

- [ ] **P0-3:** Session-request binding — verify session_id ownership before accepting wallet signature submissions
  - **Where:** `crates/claw-api/src/routes/wallet_signatures.rs`
  - **Fix:** Add session_id guard on submit endpoint

### Remaining P1 Hardening

- [x] **P1-1:** Pending state persistence — DONE (durable-first, SQLite lifecycle state, startup recovery)
- [ ] **P1-2:** Rate limiting on submit endpoint (Tower middleware)
- [ ] **P1-3:** `bind_wallet` challenge-response (signed nonce ownership proof)
- [ ] **P1-4:** Durable event log (SQLite persistence + replay endpoint)

### Technical Debt & Foundation

- [ ] **N12:** Context trimming preserves tool_use/tool_result pairs atomically
  - **Where:** `crates/agent-runtime/src/llm/context.rs` → `trim_if_needed()`
- [ ] **N13:** Two-pass compute budget optimization (simulate → tighten CU → re-simulate)
- [ ] **N14:** On-chain transaction submission (`sendTransaction` + retry + confirmation tracking)
- [ ] Clean up compiler warnings (unused imports, missing docs)

### Phase 0 Exit Criteria
- All P0 fixed, P1-1 through P1-4 resolved
- Transactions actually land on devnet via `sendTransaction`
- 200+ tests passing ✅ (achieved: 200)

---

## Up Next: Phase 1 — Single-Protocol DeFi Execution (Orca)

> Full details in [ROADMAP.md](ROADMAP.md) Phase 1. Do NOT start until Phase 0 exit criteria are met.

Key deliverables:
- `DeFiAction` trait + `ProtocolAdapter` abstraction
- Orca swap/LP execution tools (replacing read-only V1)
- `TransactionComposer` for multi-instruction atomic bundling
- Phantom Bridge E2E (browser → sign → devnet)
- Pool state analysis tools (tick distribution, IL, PnL)

---

## Remaining Gaps (Documented, Not Bugs — Addressed in Roadmap)

### Deferred to Phase 1
- Orca swap execution (currently read-only)
- Full CL tick-array quote (V1 uses constant product approximation)
- `orca_check_pool` configurable allowlist

### Deferred to Phase 2+
- Multi-protocol support (Solend, Jupiter)
- Cross-protocol risk engine (Health Factor simulator, IL calculator)
- Per-session wallet selection, Ledger signer support

### Deferred to Phase 3+
- Autonomous observers, cron/scheduler
- Emergency Escape Pod
- Flow toxicity / volatility analysis

### Deferred to Phase 4
- Intent parsing (fuzzy text → DeFi actions)
- Telegram / voice channel adapters
- Portfolio dashboard API
