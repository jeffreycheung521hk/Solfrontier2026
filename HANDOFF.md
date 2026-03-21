# ClawSolana — Session Handoff

**Last updated:** 2026-03-21
**State:** `cargo check` PASS, `cargo test` PASS (200 tests, zero failures)

---

## What Works Right Now

| Component | Status | Verified By |
|-----------|--------|-------------|
| `cargo check` | PASS | Session 10 |
| `cargo test` | PASS (200 tests) | Session 13+ |
| Both binaries compile | Yes (`clawd`, `claw`) | `cargo check` |
| Simulation-before-signing | **Compile-time enforced** (typestate) | `pipeline.rs` + 3 tests |
| Policy-before-sign | **Compile-time enforced** (typestate) | `pipeline.rs` |
| `POST /sessions` | Working | `daemon.rs` SessionOps adapter |
| `POST /sessions/:id/messages` | Working | `daemon.rs` MessageHandler adapter |
| `DELETE /sessions/:id` | Working | `daemon.rs` SessionOps adapter |
| `GET /health` | Working | `server.rs` unprotected route |
| `POST /sessions/:id/approve` | Working (202/200/409/404) | `approve.rs` |
| `GET /sessions/:id/approvals` | Working | `approve.rs` |
| `GET /sessions/:id/events` | Working (SSE, session-scoped, lag handling, 30s heartbeat) | `events.rs` |
| Agent ReAct loop | Wired end-to-end | `agent.rs` + `router.rs` |
| Per-session capability narrowing | **Live** — `CapabilitySet::for_role(AgentRole)` | `permissions.rs` + 11 tests |
| ProposeSigning capability | **Live** — Execution role can propose, cannot sign directly | `permissions.rs` + N6 smoke test |
| Audit persistence | Wired (agent start/end, tool invocations, approval decisions, signing) | `daemon.rs` + N6 smoke test |
| Tool trace persistence | **Live** — every invocation persisted | `router.rs` |
| Approval store | In-memory, one-time-actionable, DashMap-backed | `approval_store.rs` + 5 tests + N6 |
| Pending signing store | In-memory, oneshot-channel parking/resume | `pending_signing.rs` |
| Sign-path resume (N5) | **Working + tested** — operator approval triggers background signing | `tools/signing.rs` + N6 smoke test |
| Wallet loading (N4) | **Working** — `LocalKeypair` from file or base58 | `daemon.rs` step 9 |
| `submit_for_signing` tool | **Injected + tested** — gateway-private, full pipeline, requires `propose_signing` | `tools/signing.rs` + N6 |
| Orca read-only tools | **3 tools live** — whirlpool, quote, pool check | `protocols/orca.rs` + 5 tests |
| **Spend tracking (N7)** | **Live** — SQLite spend_ledger, real policy eval context | `spend.rs` + N7 tests (7) |
| **Native tool_result blocks (N8)** | **Live** — structured ContentBlock types, Anthropic-native tool_use/tool_result | `llm/` + 11 tests |
| **Compute budget prepend (N9)** | **Live** — idempotent prepend before simulation, finalized tx in AwaitingApproval | `compute.rs` + 6 tests |
| **External signer bridge (N10A/N11)** | **Live + Hardened** — session-wallet binding, external wallet flow, signed tx verification (message bytes + signer presence + ed25519 cryptographic verification), audit + SSE events, rejection audit/tracing | `external_wallet.rs` + `signing.rs` + 18 unit tests |
| **External signer HTTP bridge (N10D)** | **Tested** — full HTTP round-trip via axum Router (happy path, tampered, wrong signer) | `n10b_bridge_simulation.rs` + 3 tests |
| **External signer daemon integration (N10E)** | **Tested** — concurrent race exactly-once, restart resilience, full approval→sign flow with real SQLite | `n10d_daemon_integration.rs` + 3 tests |
| **Production readiness review** | **Documented** — P0/P1/P2 issues identified, V2 architecture proposed | `PRODUCTION_READINESS_REVIEW.md` |
| **Signature Orchestrator (N15)** | **Skeleton live** — `SignatureOrchestrator`, `WalletAdapter` trait, `LocalWalletAdapter`, `ExternalWalletAdapter`, pending-only lifecycle, exactly-once semantics, consume-on-attempt `complete()`/`expire()`, `DuplicateRequestId` rejection, reaper support | `orchestrator/` + 12 tests |
| `POST /sessions/:id/bind-wallet` | Working | `wallet_signatures.rs` |
| `POST /sessions/:id/wallet-signatures` | Working (submit signed tx, verification, spend recording) | `wallet_signatures.rs` |
| `GET /sessions/:id/wallet-signatures` | Working (list pending requests) | `wallet_signatures.rs` |
| API token hardening | `CLAW_API_TOKEN` set → no stderr leak; preview-only in logs | `daemon.rs` |
| No `unsafe` in codebase | True | `forbid(unsafe_code)` in 6 crates |
| Threat model documented | `docs/threat-model.md` | Needs update (see below) |

---

## Architecture: How the Signing Flow Works

```
Agent calls submit_for_signing tool (requires ProposeSigning capability)
  → SubmitForSigningTool::execute()
    → Queries SpendRepository for wallet_daily_spend + session_spend
    → Builds PolicyEvaluationContext with REAL spend values
    → pipeline.run() (simulate → policy → sign)
      → If auto-approved: sign immediately, record spend, return signature
      → If RequiresHumanApproval:
        1. Park tx in PendingSigningStore (bincode bytes + sim + verdict)
        2. Register ApprovalRequest in ApprovalStore
        3. Emit ApprovalRequested event
        4. Spawn background tokio task awaiting oneshot::Receiver
        5. Return {status: "awaiting_approval", approval_request_id} to agent

Operator POSTs to /sessions/:id/approve
  → GatewayApprovalHandler::decide_inner()
    1. ApprovalStore::decide() (one-time-actionable)
    2. Audit persistence (human_approved / human_rejected)
    3. PendingSigningStore::signal(request_id, approved) → sends through oneshot
    4. Emit ApprovalReceived event

Background task wakes
  → If rejected: remove parked entry, emit TransactionFailed
  → If approved:
    1. Take parked entry from PendingSigningStore
    2. Deserialize Transaction from stored bytes
    3. Reconstruct SimulatedTransaction::new_unchecked() + ApprovedTransaction::new_unchecked()
    4. pipeline.clone().with_approval_mode(HumanGranted).sign()
    5. Record spend via SpendRepository (fee_lamports from simulation)
    6. Audit "transaction_signed_after_approval"
    7. Emit TransactionSigned event
```

### Spend Tracking Design (Session 7)

```
                   ┌──────────────────────────────────┐
                   │        SpendRepository           │
                   │   (claw-state-store/spend.rs)     │
                   ├──────────────────────────────────┤
                   │ record_spend()      INSERT OR    │
                   │                     IGNORE       │
                   │ wallet_daily_spend() SUM by date │
                   │ session_spend()      SUM by sess │
                   │ wallet_daily_count() COUNT       │
                   └────────────┬─────────────────────┘
                                │
           ┌────────────────────┼────────────────────┐
           ▼                    ▼                     ▼
   ┌───────────────┐   ┌──────────────┐   ┌──────────────────┐
   │ Policy eval   │   │ Direct sign  │   │ Resume sign      │
   │ (pre-signing) │   │ (post-sign)  │   │ (post-approval)  │
   │               │   │              │   │                  │
   │ reads daily + │   │ records      │   │ records          │
   │ session spend │   │ fee_lamports │   │ fee_lamports     │
   └───────────────┘   └──────────────┘   └──────────────────┘
```

- **Unit:** Lamports (fee_lamports from SimulationResult)
- **Recording point:** Signed stage (irreversible commitment)
- **Double-count protection:** UNIQUE(transaction_id) + INSERT OR IGNORE
- **Persistence:** SQLite spend_ledger table (append-only, survives restarts)

### Compute Budget Prepend — Pipeline Flow (Session 9)

```
SubmitForSigningTool calls pipeline.run(proposal, tx, signer, policy_check)
  → pipeline.simulate(proposal, tx):
    1. Resolve priority fee from PriorityFeeStrategy
    2. prepend_compute_budget(tx, DEFAULT_COMPUTE_UNITS, priority_fee)
       - If tx already has ComputeBudget instructions → skip (idempotent)
       - Otherwise → prepend SetComputeUnitLimit + SetComputeUnitPrice
    3. Attach fresh blockhash
    4. Simulate the FINALIZED transaction
    5. Return SimulatedTransaction (inner = finalized tx)
  → pipeline.evaluate_policy() → ApprovedTransaction
  → pipeline.sign():
    - On AwaitingApproval: return finalized_tx in PipelineResult
    - On Signed: signer signs the finalized tx directly

SubmitForSigningTool on AwaitingApproval:
  → Parks finalized_tx (NOT the original pre-pipeline tx)
  → resume_signing_after_approval deserializes parked bytes
  → Signs the exact same tx that was simulated and policy-checked
```

**Key invariants:**
- Compute budget instructions are prepended BEFORE simulation → simulation result reflects final tx
- `prepend_compute_budget()` is idempotent → user-supplied compute budget is respected
- `PipelineResult::AwaitingApproval` carries `finalized_tx` → approval-resume path is consistent
- V1 uses `DEFAULT_COMPUTE_UNITS` (200,000) — conservative, safe for all transaction types
- `PriorityFeeStrategy::None` → no price instruction prepended (0 microlamports)

### External Signer Bridge — Client-Side Signature Handoff (Session 10)

```
Client binds external wallet:
  POST /sessions/:id/bind-wallet { pubkey: "Phantom..." }
    → ExternalWalletStore.bind_wallet(session_id, pubkey)

Agent calls submit_for_signing (wallet_pubkey = external wallet):
  → SubmitForSigningTool.execute_external()
    → pipeline.simulate() → pipeline.evaluate_policy()
    → If auto-approved:
      1. Park finalized unsigned tx in ExternalWalletStore
      2. Emit WalletSignatureRequested event
      3. Return { status: "awaiting_wallet_signature", unsigned_tx_b64, ... }
    → If requires human approval:
      1. Park in PendingSigningStore + ApprovalStore (same as local path)
      2. Spawn resume_external_after_approval background task
      3. Return { status: "awaiting_approval", signer_type: "external" }

Operator approves (same POST /sessions/:id/approve):
  → resume_external_after_approval wakes
    → Take parked entry from PendingSigningStore
    → Re-park in ExternalWalletStore with new wallet_sig_request_id
    → Emit WalletSignatureRequested event

Client signs and submits:
  POST /sessions/:id/wallet-signatures { request_id, signed_tx_b64 }
    → GatewayWalletSignatureHandler.submit_signed_tx_inner()
      1. Decode signed tx from base64
      2. Verify message bytes match parked unsigned tx (no tampering)
      3. Verify expected signer is in account_keys
      4. Verify expected signer has non-default signature
      5. Verify ed25519 signature against message bytes (cryptographic check)
      6. On pass: record spend, emit WalletSignatureReceived + TransactionSigned
      7. On fail: emit WalletSignatureRejected, return error
```

**Key invariants:**
- Message bytes must be identical between unsigned (parked) and signed tx
- Expected signer must have a non-default signature at its position
- Both checks fail-closed (reject on mismatch)
- Spend is recorded at signature acceptance (same stage as local signing)
- Audit trail: `wallet_signature_requested` → `wallet_signature_received` (or `_rejected`)

### Signature Orchestrator — Execution Plane Boundary (Session 13)

```
Control Plane (unchanged)                    Execution Plane (new)
─────────────────────────────               ─────────────────────────
SubmitForSigningTool                        SignatureOrchestrator
  → pipeline.simulate()                       → adapters: [external, local]
  → pipeline.evaluate_policy()                → pending: DashMap (pending-only)
  → [human approval if needed]
  → constructs SignatureRequest             submit(request):
     { finalized_tx_bytes,                    → find adapter where supports() == true
       expected_signer,                       → adapter.sign()
       session_id, ... }                        → Local:  sign immediately → Signed
                                                → External: → Pending
─── SignatureRequest ──────────────────▶       → if Pending: insert pending map
                                                → return SignatureOutcome
◀── SignatureOutcome ──────────────────
  Signed { signature, signed_tx_bytes }     complete(request_id, signed_tx_bytes):
  or Pending { finalized_tx_bytes }           → atomic DashMap::remove()
                                               → adapter.complete(consumed_request, signed_bytes)
Caller-side (unchanged):                       → verify_signed_tx() (reused pure fn)
  → audit.append()                             → return Signed or VerificationFailed
  → spend.record_spend()
  → event_bus.publish()                     expire(request_id):
                                               → atomic DashMap::remove()
                                               → return consumed SignatureRequest
```

**Key design decisions:**
- Orchestrator is sole pending-state owner (no duplicate storage in adapters)
- `LocalWalletAdapter` calls `signer.sign_transaction()` directly — NOT `pipeline.sign()`
- Input is `finalized_tx_bytes: Vec<u8>` — no typestate objects cross the boundary
- No base64 in Execution Plane types — raw bytes only
- No `Instant` in public types — internal bookkeeping only
- `complete()` is consume-on-attempt: atomic remove before verification, no re-insertion
- `DuplicateRequestId` rejection on `submit()` — fail-closed
- `ExternalWalletBindings` read-only trait — adapter reads bindings, never mutates

**Not yet wired:** `SubmitForSigningTool` and `daemon.rs` still use direct `ExternalWalletStore` / `pipeline.sign()`. Wiring is STEP 5 (next).

### Tool/LLM Boundary — Native Content Blocks (Session 8)

```
Agent loop dispatches tool
  → tool returns result
  → context.push_tool_result(tool_name, call_id, result_text)
    → ContextEntry::ToolResult stored in context

Agent loop records assistant's tool-calling intent
  → context.push_assistant_tool_use(text, tool_calls)
    → ContextEntry::AssistantToolUse stored in context

context.messages() called before LLM API call
  → AssistantToolUse → LlmMessage { role: "assistant", content: [ToolUse blocks] }
  → Consecutive ToolResults → merged into single LlmMessage { role: "user", content: [ToolResult blocks] }

Anthropic request builder
  → message_to_api() serializes content as JSON array of content blocks:
    - Text → {"type": "text", "text": "..."}
    - ToolUse → {"type": "tool_use", "id": "...", "name": "...", "input": {...}}
    - ToolResult → {"type": "tool_result", "tool_use_id": "...", "content": "..."}
```

**Key types:**
- `ContentBlock` (enum): `Text`, `ToolUse`, `ToolResult` — maps to Anthropic content blocks
- `LlmMessage.content`: `Vec<ContentBlock>` — structured, not flat string
- `ContextEntry::AssistantToolUse` — records tool_use blocks for correct pairing

**Defense-in-depth:**
- Tool result content retains `[UNTRUSTED TOOL OUTPUT]` prefix inside the `tool_result` block
- Structural isolation (tool_result block) + textual signal (prefix) + system prompt instruction
- Tool results are never sent as plain `Text` content blocks

**Provider assumptions:**
- Content block structure is Anthropic-specific
- `LlmClient` trait accepts `&[LlmMessage]` — other providers would need to adapt `ContentBlock` to their format
- The `content_text()` helper extracts flat text for backward-compatible access

### Capability Authority Model (Session 6)

```
Agent (Execution role)
  → CapabilitySet::for_role(Execution) grants: ReadWallet, ReadChain,
    SimulateTransaction, BuildTransaction, ProposeSigning
  → ProposeSigning allows calling submit_for_signing tool
  → SignTransaction / SendTransaction are NEVER granted to any role

Gateway infrastructure (not agent-scoped)
  → TransactionReviewPipeline holds the signer (SignerRef)
  → sign() is called in gateway-owned background tasks
  → No capability check at this level — pipeline IS the authority
```

---

## Security Observations

### Strong

- **Typestate pipeline** — impossible to call `sign()` without `SimulatedTransaction` → `ApprovedTransaction`. Compile-time enforced.
- **`new_unchecked()` has `debug_assert`** — defense-in-depth against incorrect reconstruction in the resume path.
- **Capability dispatch** — `ToolDispatcher` checks `CapabilitySet` before every tool invocation. Unknown capabilities fail-closed.
- **ProposeSigning ≠ SignTransaction** — agents can propose but never directly sign. Signing authority stays in gateway infrastructure.
- **Sign/Send never in role caps** — `SignTransaction` and `SendTransaction` excluded from all `for_role()` grants.
- **Approval idempotency** — `ApprovalStore::decide()` is one-time-actionable; duplicate decisions return `NotFound` (entry removed after first decision). Verified by N6 test.
- **Spend tracking is real** — `SpendRepository` queries SQLite for actual accumulated spend. Policy rules (`DailySpendExceedsSol`) now fire with real data.
- **Spend double-count protection** — `UNIQUE(transaction_id)` + `INSERT OR IGNORE` ensures the same tx is never counted twice. Verified by N7 test.
- **Native tool_result blocks** — tool outputs are structurally distinct from user text via Anthropic-native `tool_result` content blocks. Prompt injection attempts are isolated in tool_result blocks, never in text blocks.
- **External wallet message-bytes verification** — signed transaction message must be byte-identical to the parked unsigned transaction. Any instruction modification, blockhash change, or compute budget tampering is rejected. Verified by N10A tests (including dedicated blockhash + CU tampering tests).
- **External wallet signer check** — the expected wallet pubkey must have a non-default signature at its correct index (not hardcoded to index 0), AND the signature must pass ed25519 cryptographic verification against the message bytes. Wrong-wallet and corrupted signatures are rejected. Verified by N10A tests (including `reject_on_invalid_signature`).
- **External wallet one-time-actionable** — `response_tx.take()` ensures each request can only be completed once. Verified by N10A double_complete test.
- **External wallet rejection audit trail** — all rejection paths (message mismatch, wrong signer) write to AuditRepository with full traceability (request_id, transaction_id, wallet_pubkey, reason). Added in Session 11 hardening.
- **Orca fail-closed** — empty allowlist = no pools approved. Cannot be auto-approved.
- **Audit is append-only** — no `update()` or `delete()` on `AuditRepository`. Verified queryable by N6 test.
- **API token not in structured logs** — only 8-char preview in tracing; full token to stderr once.

### Partial / Noted

- **`new_unchecked()` is `pub`** — the gateway needs this to reconstruct from parked data, but it means any crate can call it. The `debug_assert` is the only runtime guard. Consider a `pub(crate)` + friend module pattern in V2.
- **Blockhash expiry** — parked transactions have a ~90s window (Solana blockhash validity). If the operator takes longer, signing will fail at the RPC level. This is safe (fail-closed) but not surfaced to the operator as a clear error.
- **Spend recording uses unwrap_or(0)** — if `SpendRepository` queries fail, policy eval proceeds with zero spend (fail-open on spend reads). Spend _recording_ failures are also silently ignored with `let _ =`. This is a pragmatic V1 choice — a DB failure shouldn't block signing — but means spend data could be incomplete.

---

## Files Changed in Session 13

```
crates/gateway/src/orchestrator/mod.rs              — NEW: SignatureOrchestrator (pending-only DashMap,
                                                       adapter dispatch, exactly-once complete/expire,
                                                       expired_candidates, pending_for_session)
crates/gateway/src/orchestrator/types.rs            — NEW: SignatureRequestId, SignatureRequest,
                                                       SignatureOutcome, SignatureError, PendingSummary
crates/gateway/src/orchestrator/adapter.rs          — NEW: WalletAdapter trait, ExternalWalletBindings trait
crates/gateway/src/orchestrator/local_adapter.rs    — NEW: LocalWalletAdapter (signer.sign_transaction direct)
crates/gateway/src/orchestrator/external_adapter.rs — NEW: ExternalWalletAdapter (stateless, reuses
                                                       verify_signed_tx)
crates/gateway/src/orchestrator/tests.rs            — NEW: 12 unit tests (routing, lifecycle, exactly-once,
                                                       duplicate ID rejection, tampered msg, expiry)
crates/gateway/src/lib.rs                           — Added pub mod orchestrator
TASK.md                                             — Updated test count, added N15, updated P0-1/P0-2
                                                       with orchestrator design notes, added STEP 5-6 tasks
PROGRESS.md                                         — Session 13 entry
HANDOFF.md                                          — Updated for Session 13
```

## Files Changed in Session 12

```
crates/gateway/src/external_wallet.rs        — verify_signed_tx() (4-step ed25519),
                                                submit_signed_transaction() store-level handler,
                                                VerifyError/SubmitError types, store.take() fix,
                                                rejection_metadata(), pending_for_session()
crates/gateway/src/daemon.rs                 — Fixed signature index (sig_idx not [0]),
                                                uses verify_signed_tx() helper,
                                                added audit writes on rejection paths,
                                                fixed transaction_id/wallet_pubkey in rejection events,
                                                implemented pending_for_session() with real data
crates/gateway/src/lib.rs                    — Exported verify_signed_tx, submit_signed_transaction,
                                                VerifyError, SubmitError, PendingSigningStore
crates/gateway/tests/n10a_external_wallet.rs — 18 tests (was 10): added verification, roundtrip,
                                                blockhash, CU tampering, ed25519 tests
crates/gateway/tests/n10b_bridge_simulation.rs — NEW: 3 HTTP bridge integration tests
crates/gateway/tests/n10d_daemon_integration.rs — NEW: 3 daemon integration tests
crates/gateway/Cargo.toml                    — Added dev-dependencies: axum, tower, http-body-util
scripts/simulate_bridge.sh                   — NEW: Manual bridge simulation script
BRIDGE_SIMULATION.md                         — NEW: Bridge architecture + test matrix + curl examples
PRODUCTION_READINESS_REVIEW.md               — NEW: P0/P1/P2 issues + V2 architecture proposal
```

## Files Changed in Session 10

```
crates/types/src/transaction.rs              — Added TransactionStatus::AwaitingWalletSignature
crates/types/src/events.rs                   — Added WalletSignatureEvent struct,
                                                WalletSignatureRequested/Received/Rejected variants
crates/gateway/src/external_wallet.rs        — NEW: ExternalWalletStore (session-wallet binding,
                                                park/complete/reject, expected_signer tracking)
crates/gateway/src/lib.rs                    — Added pub mod external_wallet + pub use
crates/gateway/src/tools/signing.rs          — Added external_wallet field, execute_external(),
                                                park_external_awaiting_approval(),
                                                park_for_external_signature(),
                                                resume_external_after_approval()
crates/gateway/src/daemon.rs                 — ExternalWalletStore creation, wired into
                                                SubmitForSigningTool + GatewayWalletSignatureHandler
crates/api/src/state.rs                      — WalletSignatureHandler trait + ref wrapper +
                                                WalletSignatureOutcome + PendingWalletSignatureInfo,
                                                wallet_signatures field in AppState
crates/api/src/lib.rs                        — Re-exported new types
crates/api/src/routes/wallet_signatures.rs   — NEW: 3 route handlers
crates/api/src/routes/mod.rs                 — Added pub mod wallet_signatures
crates/api/src/server.rs                     — Wired 3 new routes
crates/api/src/routes/events.rs              — Added WalletSignature* event filtering
crates/gateway/tests/n10a_external_wallet.rs — NEW: 10 validation tests
PROGRESS.md                                  — Session 10 entry
TASK.md                                      — N10A marked complete, N12 defined
HANDOFF.md                                   — Updated for Session 10
```

## Files Changed in Session 9

```
crates/solana-core/src/compute.rs         — has_compute_budget_instructions(), prepend_compute_budget(),
                                             idempotent prepend with unsigned-tx assertion,
                                             6 new unit tests
crates/wallet-engine/src/pipeline.rs      — simulate() now resolves fee + prepends compute budget
                                             before simulation, PipelineResult::AwaitingApproval
                                             now carries finalized_tx: Transaction
crates/gateway/src/tools/signing.rs       — AwaitingApproval match parks finalized_tx instead of
                                             original tx
PROGRESS.md                               — Session 9 entry
TASK.md                                   — N9 marked complete, N10 defined
HANDOFF.md                                — updated for Session 9
```

## Files Changed in Session 8

```
crates/agent-runtime/src/llm/mod.rs       — ContentBlock enum (Text, ToolUse, ToolResult),
                                             LlmMessage.content changed from String to Vec<ContentBlock>,
                                             constructors (text, tool_results, assistant_with_tool_use),
                                             helpers (content_text, has_tool_results, has_tool_use)
crates/agent-runtime/src/llm/context.rs   — AssistantToolUse variant added,
                                             ToolResult now emits ContentBlock::ToolResult,
                                             messages() merges consecutive tool results,
                                             push_assistant_tool_use() added,
                                             6 tests (replacing 3 old string-prefix tests)
crates/agent-runtime/src/llm/anthropic.rs — message_to_api() serializes content as block array,
                                             content_block_to_json() for per-block serialization,
                                             5 new unit tests
crates/agent-runtime/src/agent.rs         — push_assistant() for tool turns → push_assistant_tool_use()
PROGRESS.md                               — Session 8 entry
TASK.md                                   — N8 marked complete, N9 defined
HANDOFF.md                                — updated for Session 8
```

## Files Changed in Session 7

```
crates/state-store/src/migrations/0002_spend_ledger.sql — NEW: spend_ledger schema + indexes
crates/state-store/src/spend.rs                         — NEW: SpendRepository (record, query, count)
crates/state-store/src/lib.rs                           — added pub mod spend + pub use SpendRepository
crates/gateway/src/tools/signing.rs                     — SpendRepository wired into struct, constructor,
                                                          policy eval (real queries), spend recording
                                                          (both direct + resume paths),
                                                          resume_signing_after_approval gains spend param
crates/gateway/src/daemon.rs                            — creates SpendRepository, passes to signing tool
crates/gateway/tests/n6_signing_smoke.rs                — updated both call sites for new spend param
crates/gateway/tests/n7_spend_tracking.rs               — NEW: 7 validation tests (zero, record, accumulate,
                                                          cap pass, cap block, no double-count, multi-tx)
PROGRESS.md                                             — Session 7 entry
TASK.md                                                 — N7 marked complete, N8 as next task
HANDOFF.md                                              — updated for Session 7
```

---

## Known Limitations (V1)

- **Spend fail-open on read errors** — if SpendRepository query fails, policy eval gets 0 spend (fail-open)
- **Spend recording is fire-and-forget** — `let _ = spend.record_spend(...)` — DB write failure won't block signing
- **Local wallet: first loaded wallet used as default signer** — no per-session local wallet selection
- **PendingSigningStore / ApprovalStore / ExternalWalletStore in-memory** — lost on restart
- **External wallet: full ed25519 verification** — `verify_signed_tx()` performs 4-step check: message bytes match, signer in account_keys, non-default signature, ed25519 cryptographic verification. Corrupted/forged signatures are rejected before on-chain submission.
- **External wallet: no expiry/GC for pending requests** — pending wallet signature requests live forever in memory. The new `SignatureOrchestrator` has `expired_candidates()` + `expire()` API ready; reaper task not yet spawned (STEP 5).
- **Orca quote is approximate** — constant product approximation, not full CL tick traversal

---

## Next Task

**STEP 5 — Wire `SubmitForSigningTool` and `daemon.rs` to route through `SignatureOrchestrator`.**

- Refactor local signing path to use `LocalWalletAdapter` via orchestrator
- Refactor external signing path to use `ExternalWalletAdapter` via orchestrator
- Wire `submit_signed_tx_inner()` to call `orchestrator.complete()` instead of direct `ExternalWalletStore`
- Spawn expiry reaper task using `orchestrator.expired_candidates()` + `orchestrator.expire()`
- Preserve all existing behavior (audit, spend, events remain caller-side)
- Must not regress any of the 120 existing tests

After STEP 5: **STEP 6** — Integration tests through orchestrator paths.

Then resume remaining Phase 0 items: P0-3, P1-1 through P1-4, N12, N14. See TASK.md for full list.

---

## To Resume Next Session

```bash
cd /Users/jc521chinahk/Downloads/ClawSolana
cargo check          # should be green
cargo test           # 108 tests, all green
cat TASK.md          # next task: N12 context trimming pairs
cat PRODUCTION_READINESS_REVIEW.md  # P0 issues for production readiness
```
