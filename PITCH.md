# ClawSolana — AI-Guided DeFi on Solana, Without Holding Your Keys

> The first verifiable LLM-guided DeFi transaction Finalized on Solana mainnet, where the LLM holds **zero** signing authority — by design, in code, not in prompts.

> **Maturity disclaimer.** ClawSolana is a hackathon prototype with verified mainnet proofs at controlled-test-wallet amounts (0.001 USDC). It is not production-deployed, not third-party-audited, and not intended to handle real treasury balances in its current form. The mainnet artifacts demonstrate that the safety boundary works on real Solana — they are not a claim that the system is ready to be pointed at production assets today.

---

## Inspiration

AI agents are getting access to wallets, and the industry is solving it the wrong way. Today's "AI agents on Solana" projects fall into two failure modes:

1. **The agent holds the private key.** Zero governance, zero audit, zero recovery from a hallucinated transaction.
2. **The user approves every step.** Zero autonomy, zero scale, every action is a click — defeating the point of an agent.

Neither will be deployed to a real DeFi treasury. We built ClawSolana to occupy the gap: **the agent proposes, the control plane governs, the human signs.** The safety boundary is not a prompt — it is a compile-time invariant in the type system, a capability set checked at dispatch time, and an append-only audit trail.

## What it does

A user types a natural-language request. ClawSolana drives a real LLM provider (OpenAI gpt-4o-mini in the proven path; Anthropic also supported), gets back exactly one strictly-shaped tool call, runs it through the production policy engine, parks it for human approval, hands the user a partially-signed transaction to co-sign with their own wallet (Phantom), broadcasts the signed transaction through the verification pipeline, and tracks confirmation until on-chain `Finalized`.

The user typed: *"Deposit 0.001 USDC into Solend."*
The LLM produced: `solend_deposit_usdc { amount: 1000 }`.
The system parked it for approval. The user approved. Phantom signed the user-wallet slot. The verified transaction was broadcast. The confirmation tracker observed `Finalized` at slot 415,571,964.

**On-chain proof: [`4M4ezLgm…Py3y`](https://solscan.io/tx/4M4ezLgm1mFpGmUpLJdDAVhfXYwUxjS2ZMkjKprBiWzsfgNudPkhEvBr6GdJbh1zBscKLF6kpUBhZg7tAm3ePy3y)**

## How we built it

A 13-crate Rust workspace with a strict layering discipline:

- **Compile-time typestate pipeline** — `TransactionProposal → SimulatedTransaction → ApprovedTransaction → SignedTransaction`. Each transition is encoded as a state-typed value; you cannot call `sign()` on a `TransactionProposal` because the method does not exist on that type. The Rust compiler enforces the safety pipeline.
- **Capability-gated tool dispatch** — every tool declares `required_capabilities`. The LLM-driven chat handler is granted `ProposeSigning` only. `SignTransaction` and `SendTransaction` are never in the LLM's capability set, by construction.
- **Strict one-turn LLM dispatcher** (`ConversationHandler`) — calls the provider exactly once per user turn. Tool output is never re-fed to the model. There is no ReAct loop, no autonomous "next thought" — by design.
- **Strict tool input schema** — `serde(deny_unknown_fields)` + `additionalProperties: false`. If the model hallucinates a `wallet_pubkey`, `tx_bytes`, or `submit: true` field, deserialization rejects it before any approval is created.
- **Provider abstraction with fail-closed credential handling** — env-gated OpenAI / Anthropic clients with `ApiKey` redacting wrapper, lazy env reads, and no silent fallback. Missing credentials surface a typed `LlmProviderConfigError`, not a degraded mode.
- **Production Solend execution rail** — V1 USDC deposit pipeline with snapshot assembler, lending policy evaluator (Pyth oracle staleness, mint allowlists, amount caps), preflight simulator (`simulateTransaction` on the exact signed payload), partial-signing handoff store, message-hash-immutable submit verification, and a confirmation tracker that polls until `Finalized`.
- **Append-only audit** — `AuditRepository` has no `update` or `delete` methods. Every propose, simulate, policy decision, approval, and signature is logged before execution. Recovery across daemon restarts is durable-first.

**Tech**: Rust (Edition 2021) · Tokio · Axum · `solana-sdk` 2.1.21 · SQLite (SQLx) · ed25519-dalek · OpenAI / Anthropic SDKs (env-gated) · Solend Protocol V1 · Jupiter Aggregator · Phantom Wallet bridge.

## Challenges we ran into

**The most interesting failures were the ones the system caught before they touched money.** Three real production bugs surfaced on live mainnet during the road to the Phase 5G proof. Each one was correctly stopped, diagnosed, and re-proven. **Zero funds lost across all three.**

### Failure 1 — Propose-stage panic (33-byte PDA seed)

A bug in the Solend deposit propose path constructed a 33-byte seed for `find_program_address`, which has a 32-byte hard limit. In a less-disciplined system this would have surfaced as a runtime panic in production.

In ClawSolana, the bug fired at the **propose** stage — well before any approval was created, before any wallet signing, before any transaction was simulated. We caught the trace, traced it to the seed-construction code, fixed it (Phase 4-Fix), and re-proved the path on mainnet (Phase 4C-8F). No approval was ever stored with the bad PDA.

### Failure 2 — Dropped transaction (zero priority fee)

A submitted transaction with insufficient compute-unit price was dropped from the Solana cluster's mempool before it could land. In a naive system, the harness would have retried, possibly double-submitting once the original showed up.

ClawSolana's confirmation tracker observed the timeout, transitioned the lifecycle store to `BroadcastFailed` / `ConfirmationTimeout`, and **stopped**. No re-broadcast. No re-sign. No re-handoff. The system's failure modes are explicitly enumerated, and "retry on dropped tx" is intentionally not one of them. We calibrated the priority fee in 4C-8G-Fix and the next attempt landed.

### Failure 3 — Pyth oracle staleness hard-block

A live deposit proposal failed the lending policy evaluator because the USDC Pyth oracle's slot age exceeded `MaxOracleStalenessMs`. The proposal was rejected at the **policy** stage — before approval, before signing, before submit.

This is the system working exactly as designed. The policy is **code, not LLM-controlled** — the assistant cannot argue its way around an oracle staleness check, because the check is not in a prompt; it is in the lending policy evaluator's source code. We calibrated the staleness window (still well below tolerable risk) and re-proved the path on mainnet.

**These were not regressions hidden by the system. They were live failure modes surfaced safely, classified, fixed or calibrated, and re-proven.**

## Accomplishments that we're proud of

- **Phase 5G — first LLM-guided Solana mainnet proof in this codebase.** Natural language → live OpenAI → strict one-turn `ConversationHandler` → production Solend pipeline → Phantom-bridge wallet signing → `Finalized` at slot 415,571,964. Tx [`4M4ezLgm…Py3y`](https://solscan.io/tx/4M4ezLgm1mFpGmUpLJdDAVhfXYwUxjS2ZMkjKprBiWzsfgNudPkhEvBr6GdJbh1zBscKLF6kpUBhZg7tAm3ePy3y).
- **Phase 4C — non-LLM Solend mainnet baseline.** Same Solend execution rail, exercised directly through tool invocation, finalized at slot 415,475,589. Tx [`2QqSfDq…pxALs`](https://solscan.io/tx/2QqSfDq53a34WDXVkvZeyW59eFJcxZtkQtjZw25CoUizEmzDjahusKvBT8eNb4XdpKYwrgGcTyBwYXgth5wpxALs). This is the load-bearing proof that the execution rail is correct without any LLM in the loop.
- **Three documented mainnet fail-closed incidents, zero funds lost.** Each surfaced before any unsafe action.
- **Compile-time safety, not prompt safety.** The Rust type system enforces the typestate pipeline; the capability gate fires at dispatch time. Anyone can audit the boundary directly in the source.
- **977+ tests across the workspace.** Default-skip live harnesses ensure `cargo test` performs zero network calls — CI is safe by construction.
- **Wire-shape sealed.** Every DTO between gateway and frontend is type-locked behind a regression suite. Frontends can build against this surface without churn.

## What we learned

The most important thing this project taught us is the difference between **structural** and **behavioural** safety claims.

A *behavioural* safety claim is "the LLM is instructed to never sign transactions." That claim depends on the model, the prompt, and the user's input — all moving parts. It is essentially impossible to verify and trivial to bypass.

A *structural* safety claim is "the LLM does not have a code path to a signing API call." That claim is verifiable by reading the source, locked by the type system, and impossible to bypass at runtime. It does not depend on what the model says or what the user asks.

Most "AI agent safety" work today is behavioural. We built ClawSolana to demonstrate that **structural safety appropriate for DeFi use cases** is achievable *and* that it does not block useful agent behaviour. The Phase 5G mainnet proof is the demonstration of the mechanics — it is not, on its own, a claim that the system is ready for production treasury balances.

We also learned that the right number of automatic retries on failure is **zero**. Every retry is a chance to compound a mistake. Every "but what if we try again with a higher fee" is a chance to double-submit, lose track of state, or sign a stale transaction. Three documented failure modes in our path were each caught precisely because we never retried — we stopped, looked, and waited for human input.

## What's next

- **Phase 6 — User-facing chat UI.** The chat HTTP route (`POST /sessions/:id/chat`) is wire-stable and Phase 5G-proven. The next phase wires a polished frontend so a user can drive the LLM-guided path through a browser without touching the command line.
- **Squads multisig integration.** The current human-approval seam is single-signer (Phantom). Routing the seam through Squads vault transaction proposals turns ClawSolana into an AI control plane for DAO treasuries — same code-level boundary, threshold-of-N humans on the signing side.
- **Multi-intent compositions.** The current chat route dispatches one tool per turn, by design. Phase 7 will explore composed intents (swap-then-deposit, repay-then-withdraw) while preserving the one-turn-per-LLM-invocation invariant.
- **Anthropic mainnet parity.** The Anthropic provider path is wired and unit-test green; a mainnet proof using `claude-sonnet-4-6` is a near-term roadmap item.
- **Provider rate-limit / retry hardening.** The current 15-second timeout and 200 max_tokens are sufficient for the proven flow; production-grade rate-limit handling is future work.

## Try it / verify it

- **Repo**: https://github.com/jeffreycheung521hk/testingcrypto2
- **Proof index**: [`docs/proofs/INDEX.md`](docs/proofs/INDEX.md) — every mainnet / live-LLM artifact in one table
- **Phase 5 closeout**: [`docs/proofs/PHASE5_CLOSEOUT.md`](docs/proofs/PHASE5_CLOSEOUT.md) — milestone seal with security model
- **Architecture**: [`ARCHITECTURE.md`](ARCHITECTURE.md) — system invariants and security posture
- **Phase 5G proof tx**: [`4M4ezLgm…Py3y`](https://solscan.io/tx/4M4ezLgm1mFpGmUpLJdDAVhfXYwUxjS2ZMkjKprBiWzsfgNudPkhEvBr6GdJbh1zBscKLF6kpUBhZg7tAm3ePy3y) on Solscan

The system is open source from day one. All four invariants are visible in the source. Anyone can audit the safety boundary themselves.

---

## Appendix — Treasury / Sponsor Angle (forward-looking)

The hackathon framing above is the user-facing story. The same control-plane **architecture** is designed with institutional treasury operations in mind — particularly for sponsors interested in policy-grade DeFi infrastructure on Solana. **The points below are forward-looking architectural fit; the system is currently a hackathon prototype, not a deployed treasury platform. Treasury-readiness would require third-party audit, production hardening, multi-vendor signing integrations, and operational SLAs that are out of scope for this submission.**

### The structural problem treasuries face on Solana

- **Multisigs are blunt.** They authorize *who* signs, not *what* gets signed. A 3-of-5 multisig cannot say "USDC over 50k needs treasury + CFO; SOL transfers under 1k auto-approve." Approvers either rubber-stamp or block everything.
- **Policy is token-blind.** Existing tooling reads lamports, not SPL semantics. A legacy `Transfer` (no mint in the instruction) silently bypasses any rule that depends on knowing *which* token is moving — a long-standing blind spot for USDC controls.
- **Approval flow has no SLA.** Requests sit in queues with no enforced expiry, no terminal-state audit, no distinction between "rejected" and "the approver went to lunch." Compliance teams cannot reconstruct what actually happened.

### What ClawSolana provides for treasuries

1. **A policy layer purpose-built for SPL token semantics.** Rules can target specific mints, set per-token thresholds, enforce mint allowlists, and reject legacy `Transfer` instructions outright so policy always sees the mint.
2. **Multi-stage approval chains with quorum.** Rules route to risk → treasury → CFO with M-of-N quorum at each stage and operator deduplication. First-match-wins ordering is regression-locked.
3. **Lease-enforced approval.** Every request carries a TTL that is actively enforced. Expiry is its own terminal state with its own audit trail, distinct from rejection.
4. **Durable state and audit.** Pending approvals and workflows recover across restarts. Every decision, expiry, and rejection produces an audit row.
5. **External wallet flow with simulation gating.** Phantom-style propose → simulate → approve → sign. Failed simulations never enter the queue.

### Why each potential sponsor benefits

| Sponsor | Business consequence we deliver |
|---|---|
| **Solana Foundation** | An architectural pattern that, with production hardening, could give institutional treasuries the policy/approval rails they currently lack on Solana |
| **Circle / USDC** | USDC-aware policy primitives with the legacy-Transfer blind spot closed — a building block for compliance conversations with issuers and merchants once the system reaches production maturity |
| **Solend** | An architectural pattern for vault and treasury ops with multi-approver routing and lease-enforced SLAs on approvals — *and now LLM-guided as well*; not yet a deployed Solend integration product |
| **Jito / Marinade / any LST** | Audit-grade evidence and separation-of-duties primitives for validator commission and treasury rebalance flows |

### Treasury technical appendix

For reviewers who want to verify the institutional claims:
- Lease enforcement: `tokio::time::timeout` in `resume_after_approval`, lease seconds threaded from `config.policy.approval_lease_seconds`
- Token decoding: `is_legacy_token_transfer` flag set during SPL Token instruction summarization; `LegacyTokenTransferPresent` is the first guard rule
- Durable layer: SQLite via SQLx + migrations; multi-stage workflows reconstructed from persisted verdict's `approval_chain`. The event bus itself is in-process broadcast (no durable replay — durability lives in SQLite)
- Per-wallet policy: config-time policy store, not runtime / on-chain dynamic update
- Test coverage: 977+ passing tests; regression tests for rule ordering, lease enforcement, legacy-transfer bypass, expired audit
