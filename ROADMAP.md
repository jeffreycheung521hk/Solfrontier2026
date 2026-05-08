# ClawSolana — Roadmap

**Product:** Off-chain, LLM-driven transaction control plane for Solana DeFi. Fail-closed by design.

**Updated:** 2026-05-09 &nbsp;·&nbsp; **Companion:** [`ARCHITECTURE.md`](ARCHITECTURE.md), [`docs/proofs/PHASE5_CLOSEOUT.md`](docs/proofs/PHASE5_CLOSEOUT.md), [`docs/proofs/INDEX.md`](docs/proofs/INDEX.md)

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
| **Phase 6 — Stage 1 Tail: Canonical Intent Proof** | 📋 In progress (current focus) |
| Phase 7 — DAO / Treasury Adapters (Squads etc.) | 📋 Planned |
| Phase 8 — MPC / Enterprise Signing | 📋 Planned |
| Phase 9 — Research Directions | 🔬 |
| Long-term Vision — Stage 2: Strong Action-Binding + Autonomous Execution | 🔬 Strategic direction |

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

## Phase 6 — Stage 1 Tail: Canonical Intent Proof 📋

### Goal

Complete the trust story for today's human-in-the-loop execution path by making the LLM-parsed intent **tamper-evident**. The user's instruction, its expiry, and its action parameters get committed as a canonical hash; the same transaction records the hash on-chain alongside the action; any backend mutation between LLM output and Phantom signing is caught by frontend re-hash before signing.

> **Stage 1 Tail is tamper-evident, not yet fully action-enforcing.** The on-chain record proves "user signed this intent hash"; it does **not** prove the action ix bytes correspond to the hash. Real-time tamper defense lives in frontend re-hash + user canonical preview + atomic `record_intent` + action ix in one tx. Strong action-binding (program decodes action ix / pre-post balance bracket) is Stage 2.

### Boundary — now / now / now / now / now

- **Now:** user gives instruction (NL).
- **Now:** assistant proposes (canonical intent struct + canonical hash).
- **Now:** human reviews canonical preview and approves.
- **Now:** Phantom signs.
- **Now:** daemon submits.

The user is present, awake, and in the loop. There is no later-execution component in Stage 1 Tail.

### What this is NOT

- ❌ Not autonomous daemon trigger.
- ❌ Not authorize-now / execute-later.
- ❌ Not Stage 2 Authorization PDA (no conditions, no executor field, no escrow).
- ❌ Not delegated-wallet model.
- ❌ Not Pyth multi-feed conditions.
- ❌ Not full on-chain action enforcement (the chain does **not** decode the Solend / Jupiter action ix bytes).

### Mainnet evidence used for Hackathon submission

Stage 1 Tail's Hackathon submission uses these existing finalized mainnet txs as ground truth. They predate Stage 1 Tail's safety layer, but they prove the underlying execution rails work on mainnet. **Stage 1 Tail does not require new mainnet runs in the Hackathon window** — the new safety layer is demonstrated on devnet (see § Devnet demo scope) and promoted to mainnet post-Hackathon.

| Evidence | Tx | Notes |
|---|---|---|
| Phase 4C — Solend USDC deposit baseline (daemon-driven) | [`2QqSfDq…pxALs`](https://solscan.io/tx/2QqSfDq53a34WDXVkvZeyW59eFJcxZtkQtjZw25CoUizEmzDjahusKvBT8eNb4XdpKYwrgGcTyBwYXgth5wpxALs) | Slot 415,475,589. Proof: [`SOLEND_USDC_DEPOSIT_PHASE4C_E2E.md`](docs/proofs/SOLEND_USDC_DEPOSIT_PHASE4C_E2E.md). |
| Phase 5G — LLM-guided Solend deposit | [`4M4ezLgm…Py3y`](https://solscan.io/tx/4M4ezLgm1mFpGmUpLJdDAVhfXYwUxjS2ZMkjKprBiWzsfgNudPkhEvBr6GdJbh1zBscKLF6kpUBhZg7tAm3ePy3y) | Slot 415,571,964. Proof: [`LLM_SOLEND_MAINNET_E2E_PHASE5G.md`](docs/proofs/LLM_SOLEND_MAINNET_E2E_PHASE5G.md). |
| Jupiter A1 — daemon-driven Phantom-signed swap | tx in [`a1_execute_tx_signature.txt`](docs/proofs/a1_execute_tx_signature.txt) | Proof: [`JUPITER_LIVE_MAINNET_PROOF.md`](docs/proofs/JUPITER_LIVE_MAINNET_PROOF.md). |
| Jupiter A2 — LLM-driven Phantom-signed swap | tx in [`a2_execute_tx_signature.txt`](docs/proofs/a2_execute_tx_signature.txt) | NL → OpenAI → daemon → Phantom → mainnet. |
| Solend withdraw (controlled wallet) | [`3tMpTSEnm…EZPJZ`](https://solscan.io/tx/3tMpTSEnmvszjSwgow43KMd4dLqnZwwSvnTbHkcPsCiyfizLf2h7kcMc1M7LwokHLC3SBPzpTfykjsc48z8EZPJZ) | Observed live; proof doc entry pending before submission. |

### Devnet demo scope

The new Stage 1 Tail safety layer is demonstrated on **devnet** during the Hackathon window. Mainnet promotion is post-Hackathon.

| Item | Status |
|---|---|
| Canonical intent schema (Borsh) — closed schema, no extra fields | 📋 |
| `expires_at_slot` included in canonical intent hash | 📋 (see § Expiry) |
| Rust ↔ TypeScript hash parity test (CI-blocking) | 📋 |
| `record_intent` Solana program deployed to devnet | 📋 |
| Atomic transaction: `record_intent` ix + action ix in one tx | 📋 |
| Frontend canonical preview UI (NL → struct → hash → human-readable summary) | 📋 |
| Frontend re-hash before signing (mismatch → refuse to sign) | 📋 |
| Phantom integration in `/approval/[id]` | 🟡 Day 1 chat skeleton landed (b7b6996); preview + sign integration remain |
| Attack demo mode (compile-time gated) | 📋 (see § Attack demo mode) |
| Recorded demo video — uses mainnet evidence + devnet Stage 1 Tail safety demo | 📋 |
| Devpost / Frontier submission writeup | 📋 |

### Tamper-evident security model

Six checkpoints, layered. None of them claim "the chain decodes the action ix"; that is Stage 2.

| Checkpoint | Where | Catches |
|---|---|---|
| Hash computed at LLM-output earliest point | Backend, immediately after schema validation | Sets the ground-truth hash |
| Closed-schema validator | Backend, before user sees anything | Hallucinated / extra fields from the LLM |
| Frontend re-hash | Browser, before passing tx to Phantom | Backend mutation of struct between LLM output and Phantom |
| User canonical preview review | Browser, before clicking Sign | Backend giving a consistent-but-fake (struct, hash) pair |
| Atomic `record_intent` + action in one tx | Solana runtime | Separating the intent commit from the action |
| On-chain re-hash inside `record_intent` | Solana program | Client-supplied hash that does not match the supplied struct |
| Persistent Intent PDA | On-chain forever | Forensic / audit trail post-execution |

The on-chain record is **forensic proof** ("this user committed to this hash at this slot, in the same tx as this action_type"), not a real-time action validator. Real-time tamper defense lives in the browser-side re-hash and the user's review of the canonical preview.

### Intent PDA — minimal fields

```
Seeds: [b"intent", schema_version (u8), user (Pubkey), intent_id ([u8; 16])]
```

| Field | Type | Mutable? |
|---|---|---|
| `schema_version` | `u8` (v1 = 1) | ❌ |
| `intent_id` | `[u8; 16]` | ❌ |
| `user` | `Pubkey` | ❌ |
| `canonical_intent_hash` | `[u8; 32]` | ❌ |
| `action_type` | `u8` (SolendDeposit / SolendWithdraw / JupiterSwap) | ❌ |
| `expires_at_slot` | `u64` | ❌ |
| `created_at_slot` | `u64` | ❌ |
| `bump` | `u8` | ❌ |

Structured action params (amount, destination, etc.) are **not** stored on-chain — they are committed in the canonical hash. The PDA is intentionally small and version-extensible via `schema_version` in the seeds (so a future v2 PDA at the same `intent_id` is a different account, no collision).

### Expiry

- **`expires_at_slot` is part of the canonical intent hash.** Any change to expiry changes the hash; expiry cannot be silently extended without invalidating the hash.
- **Three fail-closed gates:**
  1. **Frontend approval gate** — UI rejects approval if `current_slot >= expires_at_slot`; expired intent shown as greyed-out / explicit "expired" state.
  2. **Backend submit-time check** — daemon refuses to submit a tx whose intent is past expiry, regardless of when the user signed.
  3. **On-chain `record_intent` ix** — program rejects if `Clock.slot >= expires_at_slot`.
- An expired intent is rejected at the earliest gate it hits; no later-stage retry on expiry failure.

### Attack demo mode

Demonstrates the tamper-evident model live: backend mutates struct after LLM output; frontend re-hash catches the mismatch; signing is refused.

- **Compile-time gated.** `#[cfg(feature = "demo-attack")]`. Production builds **must not** include this feature.
- **Not env-variable gated.** The toggle is a build-time decision, not a runtime decision; production binaries cannot accidentally enable it.
- **Devnet only.** Not exposed in any mainnet-targeting build.
- **What the demo proves:** the **frontend** defense (re-hash mismatch → refuse to sign). The on-chain part of the flow does not even fire because the tx is never signed. Demo narration must say: "frontend re-hash + atomicity catches the tamper here; the on-chain PDA is the post-execution forensic record, not a real-time blocker."

### Post-Hackathon follow-up

These items are flagged for the post-Hackathon credibility track. They are **not** part of Hackathon Stage 1 Tail delivery and not promised in the submission.

- **Current-HEAD Jupiter rehearsal** through the same chat-tool path as Phase 5G — consolidates Jupiter into the unified pipeline; A1 / A2 already prove Jupiter mainnet capability.
- **Mainnet `record_intent` program deployment** — devnet first; mainnet is one deliberate deploy after audit-style review.
- **Mainnet atomic-tx rehearsal** for Solend deposit, Solend withdraw, and Jupiter swap, each with `record_intent` ix appended.
- **Solend withdraw proof doc** — the `3tMpTSEnm…EZPJZ` tx is observed live; a formal `docs/proofs/` entry is pending and required before the next mainnet submission cycle.
- **Stage 2 strong action-binding** — program inspects next ix via `instructions_sysvar`; pre/post balance bracket; full action-payload binding.
- **Stage 2 execute-later flow** — conditions engine, Authorization PDA, delegated executor, escrow. See § Stage 2 preview below.

### Stage 2 preview (out of scope for Hackathon)

| Stage 1 Tail (this Hackathon) | Stage 2 (post-Hackathon) |
|---|---|
| Tamper-evident commit | Strong action-binding (program decodes / brackets the action ix) |
| User signs, daemon submits, all in same session | User authorises now; daemon executes later when conditions match |
| No on-chain conditions | Pyth multi-feed AND/OR conditions |
| No Authorization PDA | Authorization PDA with executor, conditions, escrow |
| No delegated wallet | Delegated keypair under hard bounds |
| No autonomous execution | Conditional autonomous execution within bounds |
| Real-time defense via frontend + atomicity | Real-time defense via on-chain action-payload validation |

Stage 2 specs are drafted but not implemented; nothing about Stage 2 is promised in the Hackathon submission.

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
| Sandboxed agent budgets (per-agent compute / spend / call quotas, kill switch) | 🔬 — Stage 2 introduces bounded delegation; the broader per-agent supervisor remains research |
| Cross-protocol risk modelling (net exposure, health-factor simulation, rebalancing planner) | 🔬 — touches read-side aggregation across multiple protocols |
| Programmable agent observers (cron-driven, on-chain event triggers) | 🔬 — Stage 2 introduces Pyth-backed conditions; on-chain event triggers and multi-position planning remain research |
| Long-running daemon hardening (multi-day uptime, restart recovery for watch rules, multi-position health-factor planning) | 🔬 — required to take the Stage 2 personal-risk-guard direction beyond demo |

---

## Long-term Vision — Stage 2: Strong Action-Binding + Autonomous Execution 🔬

Stage 1 Tail (Phase 6) gives a **tamper-evident** intent commit. Stage 2 extends it in two directions:

1. **Strong action-binding.** The on-chain program inspects the action ix in the same tx (via `instructions_sysvar`), enforces input/output mints, brackets pre-and-post token balances, and rejects when the action ix bytes do not correspond to the canonical hash. This converts forensic proof into real-time on-chain enforcement.
2. **Authorize-now / execute-later.** Conditions engine (Pyth multi-feed, AND/OR), Authorization PDA, delegated executor keypair under hard bounds, optional PDA escrow. The user authorises once; the daemon executes later when conditions hold; the program independently re-verifies every condition and every action constraint before any execution lands.

**Personal risk guard — the consumer-shaped direction.** A 24-hour-running daemon that watches a single user's positions and, within hard bounds the user has set, prevents foreseeable losses while the user is asleep. The Alice scenario (rules like *"if SOL < $80 AND BTC > $70k, swap up to 50 USDC"*) is the demo cut for this direction; full multi-position health-factor planning is research-direction.

**How this relates to Stage 1 Tail.**

| Aspect | Stage 1 Tail (this Hackathon) | Stage 2 |
|---|---|---|
| Trigger | User NL message, present | Watch rule on observed state, asleep |
| Action-binding | Tamper-evident (forensic) | Strong (real-time on-chain enforcement) |
| Default action | Propose, human signs, daemon submits | Execute under bounds, notify after |
| Trust boundary | Main wallet, human signs | Delegated keypair under hard caps |
| Tamper defense | Frontend re-hash + atomicity | Frontend re-hash + atomicity + on-chain action-ix validation |

**What the Hackathon proves about Stage 2:**

- Closed-schema LLM parsing + canonical hash + atomic on-chain commit (Stage 1 Tail) is the substrate. Stage 2 reuses it.
- Policy engine + fail-closed substrate (Phases 2–5) reuse cleanly as the Stage 2 bounds enforcer.
- Append-only audit gives the morning-summary timeline for autonomous-execution mode.

**What the Hackathon does NOT prove:**

- **Strong action-binding** — the chain does not yet decode action ix bytes. Stage 2 work.
- **Real user delegation flow** — Stage 2; needs real third-party user authorisation UX with consent / disclosure / recovery design.
- **Distribution.** *How does the target user find ClawSolana?* The hardest remaining problem after the technical work; not yet attempted.
- **Long-running daemon hardening** (multi-day uptime, restart recovery, multi-position planning) — listed under Phase 9; not built.
- **Cross-protocol risk modelling** — explicitly 🔬.

**M1 deliverable:** a written-down **capability boundary white paper** — what the system can and cannot do, with mainnet evidence references and an explicit not-yet-done section. The honesty of the boundary is the product, not just its capabilities.

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

- **Production frontend UI** — Day 1 of the chat skeleton just landed; full Phantom-integrated demo loop is in flight under Phase 6.
- **Stage 1 Tail mainnet promotion** — `record_intent` program is devnet-scoped during the Hackathon. Mainnet deployment + atomic-tx mainnet rehearsals (Solend deposit / withdraw / Jupiter swap) are post-Hackathon credibility work, not in this submission.
- **Solend withdraw proof doc** — withdraw mainnet tx [`3tMpTSEnm…EZPJZ`](https://solscan.io/tx/3tMpTSEnmvszjSwgow43KMd4dLqnZwwSvnTbHkcPsCiyfizLf2h7kcMc1M7LwokHLC3SBPzpTfykjsc48z8EZPJZ) is observed live; a formal `docs/proofs/` entry is pending before submission.
- **Current-HEAD Jupiter rehearsal** through the unified Phase 5G chat-tool path — A1 / A2 already prove Jupiter mainnet capability, but the consolidated route is post-Hackathon.
- **Strong action-binding (Stage 2)** — the chain does not yet decode the action ix; tamper-evidence is forensic, not real-time on-chain. Stage 2 design only.
- **Conditional autonomous execution (Stage 2)** — Authorization PDA, conditions engine, delegated executor, escrow. Stage 2 design only; not built, not promised.
- **Squads multisig integration** — designed-around but not built. Phase 7.
- **MPC / hardware signing adapters** — Fireblocks, Turnkey, Fordefi, Ledger are all 📋. Phase 8.
- **Policy DSL and sandboxed agent budgets** — research directions, not on a near-term schedule.
- **Treasury-grade SLAs and operational tooling** — webhook alerting infrastructure exists, but no production deployment, no on-call rotation, no audit certification.
- **Multi-protocol composition** (e.g., swap-then-deposit in one chat turn) — current chat surface dispatches one tool per turn, by design. Composing intents while preserving the one-turn-per-LLM-invocation invariant is a Phase 7+ design problem.
- **Anthropic mainnet parity** — provider path is wired and unit-test green; mainnet proof using `claude-sonnet-4-6` has not been run.
- **Larger amounts** — every mainnet proof on this roadmap used controlled-test-wallet amounts (1,000 raw USDC = 0.001 USDC), bounded by hard caps in the harness. The system has not been exercised at production-scale balances.
- **Distribution / go-to-market** — the Stage 2 personal-risk-guard direction needs an answer to "how does the target user find this product." Not yet attempted; flagged as the hardest remaining problem after the technical work.
- **Capability boundary white paper** — written-down list of what ClawSolana can and cannot do (with mainnet evidence and explicit not-yet-done sections). Targeted as the Stage 2 M1 deliverable; not started.

---

## Current Hackathon Focus

Frontier hackathon submission (deadline 2026-05-11 06:59 UTC). Strategy is **D' — mainnet evidence on existing finalized txs, devnet evidence on the Stage 1 Tail safety layer**. No new mainnet rehearsals are required for this submission window.

**Mainnet evidence (existing, no new runs needed):**

1. Phase 4C Solend deposit baseline — finalized tx [`2QqSfDq…pxALs`](https://solscan.io/tx/2QqSfDq53a34WDXVkvZeyW59eFJcxZtkQtjZw25CoUizEmzDjahusKvBT8eNb4XdpKYwrgGcTyBwYXgth5wpxALs).
2. Phase 5G LLM-guided Solend deposit — finalized tx [`4M4ezLgm…Py3y`](https://solscan.io/tx/4M4ezLgm1mFpGmUpLJdDAVhfXYwUxjS2ZMkjKprBiWzsfgNudPkhEvBr6GdJbh1zBscKLF6kpUBhZg7tAm3ePy3y).
3. Jupiter A1 / A2 — daemon-driven and LLM-driven Phantom-signed Jupiter swaps. See [`docs/proofs/`](docs/proofs/).
4. Solend withdraw — observed live tx [`3tMpTSEnm…EZPJZ`](https://solscan.io/tx/3tMpTSEnmvszjSwgow43KMd4dLqnZwwSvnTbHkcPsCiyfizLf2h7kcMc1M7LwokHLC3SBPzpTfykjsc48z8EZPJZ); proof doc entry pending before submission.

**Devnet deliverables (Stage 1 Tail safety layer):**

5. Canonical intent schema (Borsh, closed-schema, `expires_at_slot` in hash).
6. Rust ↔ TypeScript hash parity test (CI-blocking).
7. `record_intent` program deployed to devnet.
8. Atomic tx demo: `record_intent` ix + action ix in one tx (Solend deposit on devnet).
9. Frontend canonical preview UI + frontend re-hash before signing.
10. Attack demo mode (compile-time gated; tamper → hash mismatch → refuse signing).

**Common deliverables:**

11. **3-minute demo video** — open with mainnet evidence (existing finalized txs); show Stage 1 Tail safety layer live on devnet (canonical preview, attack demo, atomic tx); close with Stage 2 preview (autonomous execution + strong action-binding) and capability boundary statement.
12. **Devpost submission** — reuse `PITCH.md`; add demo video, screenshots, and link to ROADMAP Stage 1 Tail / Stage 2 framing.

The Phase 5G mainnet proof is in. Stage 1 Tail completes the trust story for the present-tense execution path. Stage 2 (autonomous execution + strong on-chain action-binding) is the post-Hackathon credibility track and is **not** promised in this submission.

---

## Principles

These have not changed since project inception, and Phases 4–5 strengthen rather than weaken them.

1. **Human signs, AI proposes.** No phase ever introduces auto-signing on the main wallet. The user always holds the main-wallet keys. Stage 1 Tail keeps this fully intact: every action is signed by the user in Phantom, in the same session as the natural-language request.[^inv1]
2. **Compile-time safety first.** Every pipeline stage uses typestate enforcement. If it compiles, it's safe.
3. **Fail-closed on ambiguity.** If the AI isn't sure, it asks. If a policy check fails, the action is blocked. If an oracle is stale, the proposal is rejected at the policy stage.
4. **Audit everything.** Every proposal, simulation, policy decision, approval, and signature — logged with a full append-only trail.
5. **Control plane first, adapters second.** The pipeline and policy engine are the product. Protocol integrations (Solend, Jupiter, Orca, future) are downstream applications.
6. **No silent fallback.** Missing credentials surface a typed error, not a degraded mode. No automatic retry on broadcast failure. Three documented mainnet failure modes were stopped, not retried.

[^inv1]: *Stage 2 footnote (out of Hackathon scope).* Stage 2 introduces a separate, user-authorised **delegated keypair** with hard bounds (max balance, expiry, per-trigger cap, action whitelist) for the autonomous-execution direction. The main wallet remains under INV-1: AI proposes, human signs. The delegated keypair is a distinct trust boundary that the user explicitly opts into and can revoke at any time. The bound itself — what the delegated keypair can / cannot do — is the safety claim, not "no auto-signing ever." Stage 2 is design only at submission time.
