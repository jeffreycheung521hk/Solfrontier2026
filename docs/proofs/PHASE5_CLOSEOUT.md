# Phase 5 Closeout — LLM-Guided Solend Mainnet Flow

## 1. Summary

- **Phase 5 status:** COMPLETE
- **Sealing commit:** `587e6d5`
- **Repo:** https://github.com/jeffreycheung521hk/testingcrypto2
- **Pushed range:** `0e43ed3..587e6d5`
- **Date sealed:** 2026-04-25

Phase 5 closes the loop opened by Phase 4: a natural-language user message can drive a real LLM provider, through the system's strict one-turn dispatch + capability-gated tool surface, into a typed approval/sign/submit/confirmation pipeline that finalizes a real Solend USDC deposit on Solana mainnet — with the LLM holding zero authority to sign, submit, or broadcast at any step.

## 2. Proof Chain

The Phase 5 milestone rests on three on-chain or live-provider proofs, each programmatically written only after its own success criterion was observed.

### 2.1 Phase 4C — non-LLM Solend mainnet proof

The same Solend execution pipeline used by Phase 5G, exercised directly via tool invocation (no LLM in the loop). This is the load-bearing proof that the execution rail itself is correct.

- **Proof doc:** [`SOLEND_USDC_DEPOSIT_PHASE4C_E2E.md`](SOLEND_USDC_DEPOSIT_PHASE4C_E2E.md)
- **Tx signature:** [`2QqSfDq…pxALs`](https://solscan.io/tx/2QqSfDq53a34WDXVkvZeyW59eFJcxZtkQtjZw25CoUizEmzDjahusKvBT8eNb4XdpKYwrgGcTyBwYXgth5wpxALs)
- **Finalized slot:** `415475589`
- **Lifecycle observed:** `awaiting_signature → submitted → finalized`
- **Wallet (controlled test):** `BPfDMmeMBmCbMC1rWh7hwigMBoKGBrKwXxSeUu9hhs5L`
- **Amount:** `1000` raw / `0.001000 USDC`

### 2.2 Phase 5F — live provider chat dry-run proof

First end-to-end exercise of the user-facing chat route against a real OpenAI API. No execution rail engaged — the harness uses a stub Solend tool that returns `awaiting_approval` immediately, proving that the LLM correctly produces a strictly-shaped tool call with the right amount and stops.

- **Proof doc:** [`LLM_SOLEND_CHAT_DRY_RUN_PHASE5E.md`](LLM_SOLEND_CHAT_DRY_RUN_PHASE5E.md)
- **Provider:** openai
- **Model:** gpt-4o-mini
- **Tool call:** `solend_deposit_usdc { "amount": 1000 }`
- **Final route status:** `awaiting_approval`
- **Stop point:** before approval, before signing, before submit

### 2.3 Phase 5G — LLM-guided Solend mainnet proof

The full chain: NL → live OpenAI → strict one-turn `ConversationHandler` → production `solend_deposit_usdc` → `awaiting_approval` → harness explicit approval → controlled wallet signs session-wallet slot → 4C-5 verification + broadcast → confirmation tracker observes `Finalized`.

- **Proof doc:** [`LLM_SOLEND_MAINNET_E2E_PHASE5G.md`](LLM_SOLEND_MAINNET_E2E_PHASE5G.md)
- **Tx signature:** [`4M4ezLgm…Py3y`](https://solscan.io/tx/4M4ezLgm1mFpGmUpLJdDAVhfXYwUxjS2ZMkjKprBiWzsfgNudPkhEvBr6GdJbh1zBscKLF6kpUBhZg7tAm3ePy3y)
- **Finalized slot:** `415571964`
- **Provider:** openai
- **Model:** gpt-4o-mini
- **Lifecycle observed:** `awaiting_signature → submitted → finalized`
- **Wallet (controlled test):** `BPfDMmeMBmCbMC1rWh7hwigMBoKGBrKwXxSeUu9hhs5L`
- **Amount:** `1000` raw / `0.001000 USDC`

## 3. Security Model

The Phase 5 stack preserves every safety invariant from [`ARCHITECTURE.md`](../../ARCHITECTURE.md):

- **The LLM proposes only.** The chat capability set granted to the runtime dispatcher contains exactly one capability: `ProposeSigning`. It does not contain `SignTransaction`, `SendTransaction`, or any wallet-management capability. The LLM cannot construct a path to broadcast no matter what tool call it returns.
- **Human / harness approval is required.** The `awaiting_approval` outcome can only be advanced through `ApprovalStore::decide(Approve)` + `route_approval_outcome`. In Phase 5G the test harness performed this step explicitly — there is no auto-approve mode.
- **Wallet signature is required.** The session-wallet slot of every Solend transaction is signed by the controlled keypair via `try_partial_sign`, and only that slot. The backend never holds session-wallet key material; the keystore loaded the keypair directly from a file path outside the repo.
- **The backend never signs the session-wallet slot.** Compile-time + runtime asserts: pre-signing assert that the session-wallet slot is empty, post-signing assert that it is populated and verifies via `ed25519::verify`.
- **Submit verifies message-hash immutability and obligation-signature bit-equality.** The 4C-5 verification pipeline (`submit_signed_solend_transaction`) atomically consumes the parked artifact, checks `message.hash()` did not change across signing, ensures the obligation signature was untouched, and verifies blockhash freshness, before broadcast.
- **Confirmation requires `Finalized`.** `Confirmed` alone never advances the lifecycle to terminal success. `SolendConfirmationTracker` polls `getSignatureStatuses` until `confirmation_status = "finalized"`; only then does the proof writer fire.

## 4. Phase 5 Components

| Slice | Purpose | Key artefact |
|---|---|---|
| **5A** Tool Invocation Contract & Capability Gate | Enforced strict `solend_deposit_usdc` schema (`additionalProperties: false`, amount-only) and added `pending_action_exists` guard scoped to `(session_id, session_wallet)`. | `crates/gateway/src/tools/solend_deposit.rs` |
| **5B** LLM Safety Harness | Deterministic regression suite for the tool surface — confirms strict input shape, multi-tool rejection, capability gate, fail-closed dispatch, and prompt-injection posture. | `crates/gateway/src/tools/solend_deposit.rs` (test sections) |
| **5C** ConversationHandler | Strict one-turn handler that calls the LLM exactly once per user turn, dispatches at most one tool, and never re-feeds tool output to the provider. Role separation enforced (system prompt is immutable; user text never enters System slot). | `crates/agent-runtime/src/conversation.rs` |
| **5D.1** Provider factory | `LlmProviderMode` enum with fail-closed credential handling (`ApiKey` redacting wrapper, `EnvProvider` test seam, no silent fallback). | `crates/agent-runtime/src/provider.rs` |
| **5D.2** Chat route | `POST /sessions/:id/chat` dumb-pipe HTTP handler with `DefaultBodyLimit::max(4096)`, strict request schema, full status mapping (200/400/404/409/503; never 500 for domain errors), and `ChatHandler` trait seam. | `crates/api/src/routes/chat.rs`, `crates/api/src/state.rs` |
| **5E** Real provider opt-in wiring | Env-gated provider construction (`CLAW_CHAT_PROVIDER` + `OPENAI_API_KEY` / `ANTHROPIC_API_KEY`), narrowed registry, alignment-override system prompt, 15s HTTP timeout, 200 max_tokens. | `crates/gateway/src/runtime/chat_wiring.rs` |
| **5F** Live provider dry run | First real OpenAI call through the chat route; default-skip + double opt-in (`CLAW_LIVE_CHAT_DRY_RUN=1` + `CLAW_LIVE_CHAT_DRY_RUN_GO=1`); GO checklist printer; sanitized log + proof writer. | `crates/gateway/tests/live_chat_provider_dry_run.rs` |
| **5G** LLM-guided mainnet proof | NL → live OpenAI → chat handler → production Solend tool → harness approval → wallet sign → submit → `Finalized`; double opt-in + 5-minute hard wall-clock; cross-references the 4C and 5F proofs. | `crates/gateway/tests/live_llm_solend_e2e.rs` |

## 5. What Was Proven

- **Natural language can produce a safe Solend proposal.** A real OpenAI gpt-4o-mini, given the alignment-override system prompt and the strict `solend_deposit_usdc` tool schema, returns exactly one tool call with the correct amount and stops at `awaiting_approval`.
- **The full LLM-guided path can finalize on Solana mainnet.** Phase 5G's `4M4ezLgm…Py3y` is the first transaction in this codebase whose causal chain begins with a natural-language user message and ends at `confirmation_status = "finalized"`.
- **Every high-risk action remained outside LLM authority.** The chat capability set is `ProposeSigning` only. Approval, signing, submission, and confirmation observation each require an explicit step that the LLM cannot reach. The backend never signed the session-wallet slot. The submit pipeline rejected message-hash mutation. No `tx_signature` was produced without human-equivalent approval.

### Fail-Closed Defense Record

During the Phase 4C/4-Fix → 5G sequence the system surfaced and **safely blocked** three real failure modes on live mainnet, with **zero funds lost**:

- **Propose-stage panic — invalid PDA seed.** A 33-byte seed in the production Solend deposit propose path would have panicked `find_program_address` and propagated. The check fired before any approval was created and before any RPC submit. Diagnosed in Phase 4-blocked, fixed in 4C-8F, re-proven in 4C-8G.
- **Tx dropped — zero priority fee.** A submitted transaction with zero priority-fee was dropped from the cluster mempool before inclusion. The confirmation tracker observed the timeout, transitioned the lifecycle to a non-success terminal, and reconciled state read-only against `getSignatureStatuses` — no automatic retry, no re-broadcast, no double-submit. Calibrated in 4C-8G-Fix to 50,000 µ-lamports/CU.
- **Propose-stage hard-block — Pyth oracle staleness.** A live deposit proposal failed the lending policy evaluator because the Pyth USDC oracle's slot age exceeded `MaxOracleStalenessMs`. The proposal was rejected at the policy stage, *before* any approval was created. The policy is code, not LLM-controlled, and could not be argued out of by the assistant. Calibrated in 4C-8G-Staleness to 120,000 ms — still well below tolerable risk window.

These were not regressions hidden by the system; they were live failure modes surfaced safely, classified, fixed or calibrated, and re-proven on-chain. The fact that each surfaced *before* an unsafe action is the Phase 5 system's point.

## 6. What Was Not Claimed

Phase 5 is a sealed milestone for one specific shape of LLM-guided execution. It is explicitly **not**:

- a multi-intent system (the chat surface dispatches one tool per turn; complex flows like swap-then-deposit require additional design)
- a generalised portfolio manager
- a permissionless autonomous trading agent
- a system with automatic retries on any failure path
- a system with unattended wallet control

## 7. Remaining Work

Tracked separately; not blocking the Phase 5 seal:

- **Frontend UX polish** — the chat route is wire-stable, but a polished operator UI is out of scope for Phase 5.
- **Persistent chat / audit viewer** — the audit table records every dispatch, but a navigable UI for past conversations is future work.
- **Anthropic provider parity dry run.** The `claude-sonnet-4-6` path is wired and unit-tested, but a live mainnet proof using Anthropic has not yet been run. Optional — not required for the milestone.
- **Provider rate-limit / retry hardening.** The current 15s timeout + `max_tokens=200` is sufficient for the proven flow; production-grade rate-limit handling (429 backoff, etc.) is future work.
- **CI secret scanning.** A pre-commit / CI-side secret scan would catch accidental key/log inclusion before push.
- **Phase 6 multi-intent expansion.** Compose multiple safe tool calls into longer flows while preserving the one-turn-per-LLM-invocation invariant.

## 8. Operational Notes

- **Live harnesses default-skip.** `cargo test` performs zero network calls, zero LLM calls, zero broadcasts. Live runs require explicit env opt-in (`CLAW_LIVE_*` master + `_GO` execution gate).
- **Env gates are required.** `CLAW_CHAT_PROVIDER` must be `openai` or `anthropic`; the matching API key must be non-empty. There is no silent fallback — missing credentials surface a typed `LlmProviderConfigError`.
- **Proof docs contain only public chain data.** No API keys, no Bearer tokens, no Authorization headers, no `transaction_base64`, no `tx_bytes`, no private keys. The post-write security scan (Phase 5F-Fix lesson) rejects these substrings before the file lands on disk.
- **Do not commit logs or local config.** `.gitignore` covers `*.log`, `claw.toml`, `.claude/`, `data/`, `.env*`, and key-material file extensions.

---

**Cross-references**
- [`ARCHITECTURE.md`](../../ARCHITECTURE.md) — system invariants and architecture decision records
- [`DEBT.md`](../../DEBT.md) — technical debt ledger; D-2 daemon attrition tracking
- [`SOLEND_USDC_DEPOSIT_PHASE4C_E2E.md`](SOLEND_USDC_DEPOSIT_PHASE4C_E2E.md) — Phase 4C non-LLM mainnet proof
- [`LLM_SOLEND_CHAT_DRY_RUN_PHASE5E.md`](LLM_SOLEND_CHAT_DRY_RUN_PHASE5E.md) — Phase 5F live provider dry-run proof
- [`LLM_SOLEND_MAINNET_E2E_PHASE5G.md`](LLM_SOLEND_MAINNET_E2E_PHASE5G.md) — Phase 5G LLM-guided mainnet proof
