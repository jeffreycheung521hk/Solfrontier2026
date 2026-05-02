# ClawSolana — Roadmap

**Product:** Off-chain, LLM-driven transaction control plane for Solana DeFi. Fail-closed by design.

**Updated:** 2026-05-02 &nbsp;·&nbsp; **Companion:** [`ARCHITECTURE.md`](ARCHITECTURE.md), [`docs/proofs/PHASE5_CLOSEOUT.md`](docs/proofs/PHASE5_CLOSEOUT.md), [`docs/proofs/INDEX.md`](docs/proofs/INDEX.md)

> **Maturity reminder.** This is a hackathon prototype with verified mainnet behaviour at controlled-test-wallet amounts (0.001 USDC). Nothing on this roadmap implies the system is currently production-deployed or audited.

---

## Status legend

| Marker | Meaning |
|---|---|
| ✅ **Done** | Shipped, tested, and (where applicable) mainnet-proven. |
| 🟡 **Infrastructure ready** | Code path or data model exists; live demo / external adapter / production hardening still required. |
| ⏳ **Deferred / replaced** | Superseded by a later design decision; the original line item is not coming back as written. |
| 📋 **Planned** | Future / post-hackathon work. Scope and design are clear but not yet built. |
| 🔬 **Research direction** | Not an immediate deliverable. Listed here to mark intent and avoid silent scope expansion. |

---

## Where we are now

| Phase | Status |
|---|---|
| Phase 0 — Foundation (durable state, atomic completion, rate limiting, session binding) | ✅ Done |
| Phase 1 — Execution completeness (wallet ownership proof, Phantom bridge, on-chain submission) | ✅ Done |
| Phase 2 — Policy engine (TOML rules, per-session overrides, role-gated approval, observability) | ✅ Done in primary scope; 🟡 multi-stage / quorum / webhook live-demo paths |
| Phase 3 *(original "Enterprise control plane" framing)* | ⏳ Replaced — rebranded into Phases 7 & 8 below |
| **Phase 4 — Solend Lending Rail** | **✅ Done — mainnet finalized** |
| **Phase 5 — LLM-Guided Solend Flow** | **✅ Done — mainnet finalized** |
| Phase 6 — Hackathon Demo UX / Phantom Frontend | 📋 In progress (current focus) |
| Phase 7 — DAO / Treasury Adapters (Squads etc.) | 📋 Planned |
| Phase 8 — MPC / Enterprise Signing | 📋 Planned |
| Phase 9 — Research Directions | 🔬 |

---

## Phase 0 — Production-Readiness Foundation ✅

Stabilised the control plane for safe operation. Every item below is shipped and tested.

| Item | Status |
|---|---|
| P0-1: Atomic completion (`DashMap::remove()` consume-on-attempt) | ✅ |
| P0-2: Request expiry reaper (30s interval, 120s TTL) | ✅ |
| P0-3: Session-request binding | ✅ |
| P1-1: Durable pending state (SQLite lifecycle, startup recovery, durable-first) | ✅ |
| P1-2: Per-token rate limiting (sliding window, 429 + Retry-After) | ✅ |
| P1-3: `bind_wallet` challenge-response (session-bound nonce, ed25519 verify) | ✅ |
| `bind_wallet` cross-restart persistence | ✅ |
| Append-only audit (`AuditRepository` has no update/delete methods) | ✅ |

---

## Phase 1 — Execution Completeness ✅

A real browser wallet connects, signs a pending transaction, and it lands on chain with confirmation tracking.

| Item | Status |
|---|---|
| Phantom browser bridge (`bridge/index.html`) | ✅ |
| External signer formalisation (`SignerType::External`) | ✅ |
| `sendTransaction` after verification + confirmation tracking (submitted → confirmed → finalized) | ✅ |
| Two-pass compute budget (simulate → tighten CU → re-simulate) | ✅ |
| Devnet end-to-end proof — Orca swap, message-driven flow | ✅ |

---

## Phase 2 — Policy Engine ✅ (with 🟡 surface area)

The policy layer operates as a configurable rule engine for transaction governance.

### 2.1 — Granular policy rules

| Item | Status |
|---|---|
| Destination denylist | ✅ |
| Program allowlist | ✅ |
| Amount thresholds (`AmountExceedsLamports`) | ✅ |
| TOML-driven `[[policy.rules]]` with first-match-wins | ✅ |
| Instruction summary decoding (System Program transfers, CreateAccount, SPL Token) | ✅ |
| Per-session policy overrides (`SessionPolicyStore`) | ✅ |
| USDC-aware policy primitives (legacy `Transfer` blind spot closed) | ✅ |
| Time-based rules (business hours, maintenance windows) | 📋 |

### 2.2 — Approval matrix

| Item | Status |
|---|---|
| Role-based approver gating (`required_approver_role`, 403 on mismatch) | ✅ |
| Conditional routing (escalate above threshold to specific approver role) | ✅ |
| Lease-enforced approval (TTL, terminal expiry distinct from rejection) | ✅ |
| Multi-step approval chain (agent → risk → treasury) — types + ordering tests in place | 🟡 (data model + regression locks live; live demo and operator UX deferred to Phase 7) |
| Quorum-based approval (M-of-N, operator deduplication) — proptest fuzzer + concurrent test | 🟡 (infrastructure live; not exercised as a primary user-facing flow yet) |

### 2.3 — Signer constraints

| Item | Status |
|---|---|
| Per-wallet signing policies (different thresholds per wallet) | ⏳ Replaced — covered by `SessionPolicyStore` + per-rule `required_approver_role`; revisit if a real treasury use case demands the original phrasing |
| Signing session time-bounds | ✅ Replaced by lease enforcement (`tokio::time::timeout` in `resume_after_approval`, lease seconds threaded from config) |

### 2.4 — Policy observability

| Item | Status |
|---|---|
| Policy evaluation audit trail (which rules fired, why, with full context) | ✅ |
| Per-rule hit-rate metrics (`CounterMap`, `GET /metrics`) | ✅ |
| `PolicyEvaluationResult` with `rules_total`/`rules_evaluated`/`matched_rule_index` | ✅ |
| Policy violation alerting (webhook on rejection) — `policy_alerting::PolicyAlertSender` + `AlertDispatcher` enum live | 🟡 (sender module + reqwest client wired; production webhook integrations + retry/backoff hardening deferred) |

---

## Phase 4 — Solend Lending Rail ✅

The non-LLM baseline that proves the Solend execution rail works end-to-end on mainnet, exercised directly via tool invocation (no LLM in the loop).

**On-chain proof:** [`SOLEND_USDC_DEPOSIT_PHASE4C_E2E.md`](docs/proofs/SOLEND_USDC_DEPOSIT_PHASE4C_E2E.md) — tx [`2QqSfDq…pxALs`](https://solscan.io/tx/2QqSfDq53a34WDXVkvZeyW59eFJcxZtkQtjZw25CoUizEmzDjahusKvBT8eNb4XdpKYwrgGcTyBwYXgth5wpxALs) Finalized at slot **415,475,589**.

| Component | Status |
|---|---|
| `solend_deposit_usdc` tool with strict amount-only schema (`additionalProperties: false`) | ✅ |
| Solend snapshot assembler + lending policy evaluator (Pyth oracle staleness, mint allowlists, amount caps) | ✅ |
| Preflight simulator (`simulateTransaction` on the exact signed payload) | ✅ |
| Partial-signing handoff store (obligation Keypair held in-process, never serialised) | ✅ |
| Submit verification pipeline (atomic consume, message-hash immutability, session + obligation signature checks, blockhash freshness) | ✅ |
| Confirmation tracker (polls `getSignatureStatuses` until `Finalized`) | ✅ |
| Phase 4C-8 mainnet proof harness (default-skip, double opt-in) | ✅ |

Three live failure modes were caught and documented during the path to this proof — see [Phase 5 Closeout §5](docs/proofs/PHASE5_CLOSEOUT.md) for the fail-closed defense record.

---

## Phase 5 — LLM-Guided Solend Flow ✅

Natural-language user message → live LLM → `solend_deposit_usdc` proposal → human approval → wallet signing → submit → on-chain `Finalized`.

**On-chain proof:** [`LLM_SOLEND_MAINNET_E2E_PHASE5G.md`](docs/proofs/LLM_SOLEND_MAINNET_E2E_PHASE5G.md) — tx [`4M4ezLgm…Py3y`](https://solscan.io/tx/4M4ezLgm1mFpGmUpLJdDAVhfXYwUxjS2ZMkjKprBiWzsfgNudPkhEvBr6GdJbh1zBscKLF6kpUBhZg7tAm3ePy3y) Finalized at slot **415,571,964**.

**Live-provider chat dry run (no execution rail):** [`LLM_SOLEND_CHAT_DRY_RUN_PHASE5E.md`](docs/proofs/LLM_SOLEND_CHAT_DRY_RUN_PHASE5E.md) — first real OpenAI gpt-4o-mini call through the chat route, stopped at `awaiting_approval`.

| Slice | Component | Status |
|---|---|---|
| 5A | Strict tool schema + capability gate + `pending_action_exists` guard scoped to `(session_id, session_wallet)` | ✅ |
| 5B | LLM safety harness — deterministic regression suite (multi-tool rejection, capability gate, fail-closed dispatch, prompt-injection posture) | ✅ |
| 5C | Strict one-turn `ConversationHandler` (provider called exactly once per user turn; tool output never re-fed to model; no autonomous loop) | ✅ |
| 5D.1 | Provider factory — `LlmProviderMode` (Disabled / Scripted / OpenAi / Anthropic), `ApiKey` redacting wrapper, `EnvProvider` test seam, fail-closed credential handling, no silent fallback | ✅ |
| 5D.2 | `POST /sessions/:id/chat` route with `DefaultBodyLimit::max(4096)`, strict body schema, status mapping (200/400/404/409/503; never 500 for domain errors) | ✅ |
| 5E | Real provider opt-in wiring (`CLAW_CHAT_PROVIDER` env gate, narrowed registry, alignment-override system prompt, 15s HTTP timeout, 200 max_tokens) | ✅ |
| 5F | Live OpenAI dry run through `POST /chat` — first real provider call, exactly one tool call, stops at `awaiting_approval` | ✅ |
| 5G | LLM-guided Solend mainnet proof — full chain Finalized | ✅ |

The complete safety model, slice-by-slice components, and three-incident fail-closed defense record are sealed in [`PHASE5_CLOSEOUT.md`](docs/proofs/PHASE5_CLOSEOUT.md).

---

## Phase 6 — Hackathon Demo UX / Phantom Frontend 📋

**Current focus.** Make the Phase 5G proof demo-able through a browser, not the command line.

| Item | Status |
|---|---|
| `/chat` page — natural-language entry, strict `ChatResponse` state-machine rendering | 🟡 Day 1 skeleton landed (b7b6996); polish + live-mode wiring remain |
| `useSigningHandoff` hook — poll `GET /sessions/:id/solend-signatures/:request_id` | 📋 Day 2 |
| Phantom integration in `/approval/[id]` — wallet connect, sign session-wallet slot, auto-submit | 📋 Day 2 |
| Wallet header — connect/disconnect, current USDC + Solend balance | 📋 Day 2-3 |
| Audit timeline visualisation — render the append-only chain as a flow diagram | 📋 Day 3 (stretch) |
| Recorded demo video — 3-minute walkthrough with mainnet tx as the closer | 📋 Day 6-7 |
| Devpost / Frontier submission writeup | 📋 Day 7 |

---

## Phase 7 — DAO / Treasury Adapters 📋

Multi-approver flows for shared / institutional wallets. The control-plane code is design-aware of multi-approver scenarios; this phase is the productisation layer.

| Item | Status |
|---|---|
| Squads multisig adapter (vault transaction proposal → threshold sign → execute) | 📋 |
| Multi-approver UX (vault state, threshold, signature collection, execute gating) | 📋 |
| Webhook alert profiles (slack, pagerduty, email-via-relay) | 🟡 Underlying `PolicyAlertSender` + `AlertDispatcher` exist; profile adapters + retry/backoff are 📋 |
| Treasury policy profiles (CFO escalation, off-hours block, daily spend caps per agent) | 📋 |
| Periodic reporting and reconciliation surface | 📋 |
| Ledger wallet + Solana CLI signer integration for individual treasury members | 📋 |

---

## Phase 8 — MPC / Enterprise Signing 📋

Replace single-key signing with enterprise key-management adapters.

| Item | Status |
|---|---|
| Fireblocks adapter | 📋 |
| Turnkey adapter | 📋 |
| Fordefi adapter | 📋 |
| Ledger / hardware wallet integration | 📋 |
| Cross-vendor signer routing (policy decides which signing surface) | 📋 |

These are post-hackathon / post-funding scope. The provider-agnostic `LlmClient` trait pattern from Phase 5D.1 will be mirrored on the signing side.

---

## Phase 9 — Research Directions 🔬

Listed for transparency about long-term intent. Not deliverables; not on a near-term schedule.

| Direction | Notes |
|---|---|
| Policy DSL (declarative configuration language for rules) | 🔬 — current TOML rules cover the demonstrated shapes; a DSL is justified once rule complexity exceeds TOML's expressiveness |
| Sandboxed agent budgets (per-agent compute / spend / call quotas, kill switch) | 🔬 — hard isolation requires deeper supervisor work |
| Cross-protocol risk modelling (net exposure, health-factor simulation, rebalancing planner) | 🔬 — touches read-side aggregation across multiple protocols |
| Programmable agent observers (cron-driven, on-chain event triggers) | 🔬 — currently single-shot intent execution |

---

## Execution Adapters & Protocol Integrations

Execution adapters are downstream integrations that operate **under** the control plane. They produce `TransactionProposal`s that enter the pipeline.

### Solend ✅
Mainnet-proven (4C + 5G). The Solend rail is the canonical reference for the propose / approval / preflight / signing handoff / submit / confirmation pattern. See above.

### Jupiter ✅
Mainnet-proven via Phantom bridge.

| Capability | Status |
|---|---|
| `submit_jupiter_swap` (intent submission) | ✅ |
| Swap policy (mint allowlist, amount cap, slippage cap) | ✅ |
| JIT V0 build + ALT assembly + simulate | ✅ |
| Local-signer execution path | ✅ |
| External-wallet (Phantom) V0 completion + strict blockhash mode | ✅ Mainnet-proven (Phase A1 / A2) |
| `rebuild_required` retry signal on wallet blockhash modification | ✅ |
| Phantom blockhash-refresh compatibility mode | ⏳ Deferred (needs interop evidence) |
| Trigger / Limit / `/order` (DCA, scheduled) | 📋 |

### Orca Whirlpools ✅ (devnet)

| Tool | Status |
|---|---|
| `orca_get_whirlpool` / `orca_get_quote` / `orca_check_pool` | ✅ |
| `orca_swap` — devnet confirmed | ✅ |
| Message-driven swap demo — devnet | ✅ |
| `DeFiAction` trait + `ProtocolAdapter` abstraction | 📋 |
| `orca_open_position` / `orca_close_position` (CLMM LP management) | 📋 |
| Full CL tick-array-aware quote (replace constant-product approximation) | 📋 |

### Future protocol adapters

| Adapter | Status |
|---|---|
| Solend lending — withdraw / borrow / repay / health-factor monitoring | 📋 (deposit done; full lifecycle is Phase 7+ scope) |
| Cross-protocol risk engine | 🔬 |

---

## What Is Not Yet Done

This section exists for honest credibility. **None of the items below are claimed complete.**

- **Production frontend UI** — Day 1 of the chat skeleton just landed; full Phantom-integrated demo loop is in flight.
- **Squads multisig integration** — designed-around but not built. Phase 7.
- **MPC / hardware signing adapters** — Fireblocks, Turnkey, Fordefi, Ledger are all 📋. Phase 8.
- **Policy DSL and sandboxed agent budgets** — research directions, not on a near-term schedule.
- **Treasury-grade SLAs and operational tooling** — webhook alerting infrastructure exists, but no production deployment, no on-call rotation, no audit certification.
- **Multi-protocol composition** (e.g., swap-then-deposit in one chat turn) — current chat surface dispatches one tool per turn, by design. Composing intents while preserving the one-turn-per-LLM-invocation invariant is a Phase 7+ design problem.
- **Anthropic mainnet parity** — provider path is wired and unit-test green; mainnet proof using `claude-sonnet-4-6` has not been run.
- **Larger amounts** — every mainnet proof on this roadmap used controlled-test-wallet amounts (1,000 raw USDC = 0.001 USDC), bounded by hard caps in the harness. The system has not been exercised at production-scale balances.

---

## Current Hackathon Focus

Frontier hackathon submission (deadline 2026-05-11 06:59 UTC). The technical depth is sealed at Phase 5G; remaining work is making it visible:

1. **Build `/chat` UI** — natural-language entry, render the typed `ChatResponse` state machine. *(Day 1 skeleton committed; live-mode polish + balance display remaining.)*
2. **Connect approval → Phantom signing → finalized status** — `useSigningHandoff` hook, Phantom integration in `/approval/[id]`, lifecycle progress display.
3. **Run one final live mainnet demo** through the new UI — the same flow Phase 5G proved, but via the browser instead of the test harness.
4. **Record a 3-minute demo video** — show fail-closed defense record, mainnet finalization, open-source verifiability.
5. **Devpost submission** — reuse `PITCH.md` (already in Devpost format); add screenshots and demo video.

The mainnet proof is in. The work that remains is the storytelling layer.

---

## Principles

These have not changed since project inception, and Phases 4–5 strengthen rather than weaken them.

1. **Human signs, AI proposes.** No phase ever introduces auto-signing. The user always holds the keys.
2. **Compile-time safety first.** Every pipeline stage uses typestate enforcement. If it compiles, it's safe.
3. **Fail-closed on ambiguity.** If the AI isn't sure, it asks. If a policy check fails, the action is blocked. If an oracle is stale, the proposal is rejected at the policy stage.
4. **Audit everything.** Every proposal, simulation, policy decision, approval, and signature — logged with a full append-only trail.
5. **Control plane first, adapters second.** The pipeline and policy engine are the product. Protocol integrations (Solend, Jupiter, Orca, future) are downstream applications.
6. **No silent fallback.** Missing credentials surface a typed error, not a degraded mode. No automatic retry on broadcast failure. Three documented mainnet failure modes were stopped, not retried.
