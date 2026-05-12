# ClawSolana

> **An off-chain, LLM-driven transaction control plane for Solana DeFi. Fail-closed by design — verified on mainnet, hackathon prototype today.**

🟢 **Phase 5G Mainnet Proven** &nbsp;·&nbsp; 🛡️ **Zero-Loss Fail-Closed Architecture** &nbsp;·&nbsp; 🦀 **Compile-Time Safety Boundary**

> **Maturity disclaimer.** This is a hackathon project demonstrating verified mainnet behaviour with controlled-test-wallet amounts (≤ 0.25 USDC per transaction; bounded by hard caps in the harnesses). It is **not** production-deployed, **not** audited by a third party, and **not** intended to manage real treasury assets in its current form. The mainnet proofs validate the control-plane mechanics; they are not a green light to point at live treasury balances.

---

## Mainnet Proven, Today

A natural-language user message drove a real OpenAI provider through the strict one-turn `ConversationHandler` and the production Solend pipeline, finalizing a USDC deposit on Solana mainnet. **The LLM proposes only — approval and wallet signing remain human-controlled at every step.** A subsequent slice extended this to autonomous watcher-triggered execution within hard, pre-set bounds on a controlled wallet.

| Proof | What it shows | On-chain outcome |
|---|---|---|
| **W5i** — first autonomous Solend deposit ★★ | 30-second polling watcher brokered a live Solend deposit on the controlled wallet's keypair — no human signature between funding and execution | tx [`takmz89b…jTv5`](https://solscan.io/tx/takmz89b5hLHgJ9jHVBbqJogxdvu8xuMdUgZLEpdHhTD7zvRswrrk1UnxfZf7pbsaM4fygqvDFwFbph5YH2jTv5) — Finalized at slot 419,200,198 |
| **Phase 6I-K** — Solend withdraw-all mainnet | LLM-guided withdraw round-trip closed after a 4-attempt processor-parity fail-closed sequence | tx [`3tMpTSEn…EZPJZ`](https://solscan.io/tx/3tMpTSEnmvszjSwgow43KMd4dLqnZwwSvnTbHkcPsCiyfizLf2h7kcMc1M7LwokHLC3SBPzpTfykjsc48z8EZPJZ) — Finalized at slot 418,395,346 |
| **Phase 5G** — first LLM-guided mainnet deposit | NL → live OpenAI → strict 1-turn handler → Solend → Finalized | tx [`4M4ezLgm…Py3y`](https://solscan.io/tx/4M4ezLgm1mFpGmUpLJdDAVhfXYwUxjS2ZMkjKprBiWzsfgNudPkhEvBr6GdJbh1zBscKLF6kpUBhZg7tAm3ePy3y) — Finalized at slot 415,571,964 |
| **Phase 4C** — non-LLM mainnet baseline | Solend execution rail correct end-to-end without an LLM in the loop | tx [`2QqSfDq…pxALs`](https://solscan.io/tx/2QqSfDq53a34WDXVkvZeyW59eFJcxZtkQtjZw25CoUizEmzDjahusKvBT8eNb4XdpKYwrgGcTyBwYXgth5wpxALs) — Finalized at slot 415,475,589 |
| **Stage 2 W5b** — controlled-wallet Solend deposit substrate | Live Solend deposit substrate via controlled wallet (does NOT prove clawsol-authority `ExecuteAction`) | tx [`5BiNNVcK…aLwa`](https://solscan.io/tx/5BiNNVcKfrRkzx16kM3aTYfEYwP4uCk9grSZ2Jvsig2RQEs4Lgw8uv61hqzzBa2Dqzx5EWHCmbqpdo9w8rDSaLwa) — Finalized at slot 418,985,346 |
| **Jupiter A1** — daemon-driven Phantom swap | Full daemon `complete_v0` + `send_raw_v0_transaction` live | tx [`Q5pbBmjj…1zK6H`](https://solscan.io/tx/Q5pbBmjjn7tbFKEyswzyFsAkYoGHVJe4TT2m2viGbBCa2j9LXrFQ8X8AeKjdXw3seom2YYezFdtS8jBbpA1zK6H) — Finalized |
| **Jupiter A2** — LLM-driven Phantom swap | NL → OpenAI → daemon → Phantom → mainnet | tx [`52bvUZ1e…D2dE1`](https://solscan.io/tx/52bvUZ1eEHjrbxRBQBssYWg7BrFNoAY1Zeynw44hr3BKTAe9zTHv9Qz4wQsNi7Jw6sbXpb8zehHafn1gFzLD2dE1) — Finalized |

→ See the full [Proof Index](docs/proofs/INDEX.md) and [Phase 5 Closeout](docs/proofs/PHASE5_CLOSEOUT.md).

---

## 📅 Weekly Engineering Record — Start Here

> **For the dated, chronological view of every milestone, scope decision, and
> fail-closed defense, read [`docs/WEEKLY_UPDATES.md`](docs/WEEKLY_UPDATES.md).**
> It is the most concentrated single read in this repo for evaluating
> ClawSolana's pace, discipline, and honesty.

Among other things, `WEEKLY_UPDATES.md` walks through:

- **W2 (Apr 14–20)** — wire-shape milestone (374 tests) + first Jupiter mainnet finalizations (A1 daemon-driven, A2 LLM-driven)
- **W3 (Apr 21–27)** — Solend full pipeline + the first natural-language → live OpenAI → mainnet-finalized Solend deposit (Phase 5G), with a 4-attempt fail-closed defense record on the deposit path
- **W5 May 8** — Phase 6I-K Solend withdraw-all mainnet recovery, with a second 4-attempt fail-closed sequence against the deployed Solend processor's account-meta requirements
- **W5 May 12** — W5e/W5f/W5g chat-first conditional execution shipped end-to-end; the W5h-lite final-sprint compromise (funding-gated; auto-refund deferred); and ★★ **W5i — first live autonomous Solend deposit on mainnet**, where a 30-second polling watcher brokered the deposit using only the controlled wallet's keypair, with no human signature between funding and execution

**Final-sprint scope decisions** (May 10–11, recorded honestly in `WEEKLY_UPDATES.md`):

- ✅ **Solend is the Stage 2 execution spine.** Deposit + withdraw round-trip, W5h-lite chat-first funding gate, and W5i autonomous watcher execution are all mainnet-proven.
- 🟡 **Jupiter conditional auto-execute was deliberately de-scoped from the live demo.** The J4 sibling-ix verifier skeleton + J5 dual-bracket prototype + v2 `/build` sizing landed as substrate; a trustless live conditional swap needs work that can't be safely rushed under deadline pressure. **Plain LLM-driven Jupiter swap (Phase A1 / A2) remains mainnet-proven** and is part of the demo.
- 🟡 **W5h auto-refund (3-minute expiry) is deferred.** Funding popup, budget reservation, W5g approval, and W5i autonomous execution are all live; the refund leg is post-Hackathon.

See the May 10 entry (*"Stage 2 conditional Solend demo plan locked"*) and the May 11–12 entry (*"Final sprint compromise — W5h-lite"*) in [`docs/WEEKLY_UPDATES.md`](docs/WEEKLY_UPDATES.md) for the full deliberations.

Every dated entry quotes its on-chain tx signature (with Solscan link), names
exactly what was proven, and **explicitly names what was not proven** in the
same breath — the no-overclaim discipline runs throughout. **Read that file
first.**

---

## Why This Matters

AI agents are getting access to wallets. Most agent-wallet integrations are one of two extremes: **the agent holds the private key** (zero governance), or **a human approves every action** (zero autonomy). Neither scales.

ClawSolana sits in the gap: **the agent proposes, the control plane governs, the human signs.**

- DeFi treasuries are deploying agents — they need policy rails, not a hot wallet and a prayer
- Solana's 400 ms slots mean a rogue transaction lands before a human can react — pre-submission enforcement is the only viable control point
- The signing boundary must be **structural**, not prompt-engineered — ClawSolana enforces it at compile time

## How It Stays Safe

The system invariants from [`ARCHITECTURE.md`](ARCHITECTURE.md) are not policy text — they are **load-bearing code-level guarantees**:

- **Compile-time typestate pipeline** — `TransactionProposal → SimulatedTransaction → ApprovedTransaction → SignedTransaction`. You cannot call `sign()` on a `TransactionProposal`. The method does not exist on that type.
- **Capability-gated tool dispatch** — every tool declares `required_capabilities`; the LLM-driven path holds `ProposeSigning` only. `SignTransaction` and `SendTransaction` are not in the LLM's capability set, ever.
- **Append-only audit** — `AuditRepository` has no `update` or `delete` methods. Every propose, simulate, policy decision, approval, and signature is logged before execution.
- **Strict tool schema** — `additionalProperties: false`. The LLM cannot inject `wallet_pubkey`, `tx_bytes`, `submit: true` — schema rejects before any approval is created.
- **One-turn LLM dispatch** — `ConversationHandler` calls the provider exactly once per user turn. Tool output is **never** re-fed to the model. No autonomous loop.

## Fail-Closed Defense Record

The system has accumulated **two independent multi-attempt fail-closed sequences** on live mainnet, with **zero funds lost** across both. Each documented incident surfaced *before* an unsafe action could complete.

### Sequence 1 — Phase 5G deposit path (3 incidents)

- **Propose-stage panic** — a 33-byte PDA seed would have panicked `find_program_address`. Caught at the propose stage; no approval was ever created.
- **Tx dropped** — a zero-priority-fee broadcast was dropped from the cluster mempool. Confirmation tracker observed timeout, transitioned the lifecycle to a non-success terminal, no automatic retry, no double-submit.
- **Pyth oracle staleness** — a live deposit proposal failed the lending policy evaluator because the USDC oracle's slot age exceeded the staleness window. The proposal was rejected at the policy stage, before any approval.

### Sequence 2 — Phase 6I-K withdraw processor-parity (4 incidents, May 8)

Each attempt reverted on chain; user funds were never at risk, only per-tx fees were paid.

- **`RefreshObligation` `Custom(5) InvalidAccountOwner`** — builder included `Clock` in the ix, shifting reserve-account positions Solend reads. Fix: `RefreshObligation` is `[obligation, ...referenced_reserves]`, no Clock (Phase 6I-H).
- **Withdraw `NotEnoughAccountKeys`** — withdraw ix omitted the remaining `deposit_reserve_infos` after the token program (Phase 6I-I).
- **Withdraw `Custom(9) InvalidTokenProgram`** — the 6I-I fix put `Clock` at account slot 11, but the deployed Solend processor expects the SPL Token program at slot 11. Fix: no Clock; slot 11 = `Tokenkeg…VQ5DA`; deposit reserves appended from slot 12 (Phase 6I-J).
- **Withdraw `ReadonlyDataModified`** — slot 4 `lending_market` was readonly in the builder, but the deployed processor writes to `LendingMarket` as part of its rate-limiter update during withdraw. Fix: slot 4 must be writable (Phase 6I-K). The next attempt finalized.

**Process lesson — protocol parity must be source-grade, not SDK-grade.** Solend's deployed `processor.rs` enforces a different layout in practice than the SDK helper docs suggest (e.g., `lending_market` writable because of the rate-limiter write — invisible from the SDK). Stage 2 work now treats `processor.rs` as the source of truth and pins `verification_level == Full` for Pyth.

**Seven live failure modes. Seven correct rejections.** Each diagnosed, calibrated, and re-proven on chain. **That each one surfaced *before* an unsafe action is the system's point.**

## What This Is / Is Not

| Is | Is Not |
|----|--------|
| Transaction control plane for AI-driven DeFi | A wallet |
| Compile-time safety boundary | A prompt-engineered guardrail |
| Signing orchestrator (local + Phantom external wallets) | A signing proxy |
| Append-only audit system | A frontend dApp |
| Production-quality mainnet pipeline | Production-deployed |

## Capabilities

| Layer | Status |
|-------|--------|
| Typestate pipeline (simulate → policy → approve → sign) | Compile-time enforced |
| TOML-driven policy rules (amount thresholds, allowlist, denylist) | Live |
| Per-session policy overrides (layered: session → global → defaults) | Live |
| Approval matrix (role-based approver gating, M-of-N quorum) | Live (M-of-N infrastructure; not exercised as a primary user-facing flow yet) |
| Policy observability (per-rule metrics, structured audit) | Live |
| Durable pending state (SQLite, restart-safe) | Live |
| Signature orchestrator (local + external Phantom, exactly-once) | Live |
| Wallet ownership proof (challenge-response) | Live |
| Canonical intent hash + frontend re-hash (tamper-evident Stage 1 Tail) | Live (devnet substrate; mainnet promotion is post-Hackathon) |
| **Solend USDC deposit, mainnet finalized** | **Live, mainnet proven (Phase 4C + 5G)** |
| **Solend USDC withdraw-all, mainnet finalized** | **Live, mainnet proven (Phase 6I-K)** |
| **Jupiter swap, mainnet finalized via Phantom bridge** | **Live, mainnet proven (A1 + A2)** |
| Orca Whirlpool swap | Live, devnet confirmed |
| **LLM-driven `solend_deposit_usdc` chat route** | **Live, mainnet proven (Phase 5G)** |
| LLM-driven `solend_withdraw_all_usdc` chat route | Live, mainnet proven (Phase 6I-K) |
| **Stage 2 W5h-lite chat-first funding-gated flow** | **Live, mainnet proven — Phantom funding + budget reservation; auto-refund deferred** |
| **Stage 2 W5i autonomous watcher execution ★★** | **Live, mainnet proven — 30s polling watcher brokered Solend deposit with no human signature between funding and execution** |
| OpenAI + Anthropic provider support (env-gated, fail-closed) | Live, OpenAI mainnet proven; Anthropic unit-test green |
| Rate limiting, compute budget optimization, retry/rebroadcast | Live |
| clawsol-authority `ExecuteAction` (on-chain program signer) | Design only; not on mainnet |
| `AuthorizationRecord` PDA live execution | Design only; not on mainnet |
| Jupiter conditional auto-execute (sibling-ix bracket) | Skeleton + sizing-measured; live conditional swap deferred |

## Feature Status — honest tier separation

For reviewers who want a single table that separates *mainnet-proven* from *designed* from *not-yet*. Same content as the Capabilities table above, grouped by maturity.

| Tier | Capability | Evidence |
|---|---|---|
| **★ Mainnet proven** | Solend USDC deposit (non-LLM baseline) | Phase 4C — tx `2QqSfDq…pxALs` |
| **★ Mainnet proven** | LLM-guided Solend USDC deposit | Phase 5G — tx `4M4ezLgm…Py3y` |
| **★ Mainnet proven** | LLM-guided Solend withdraw-all | Phase 6I-K — tx `3tMpTSEn…EZPJZ` |
| **★ Mainnet proven** | Jupiter swap (daemon-driven) | Jupiter A1 — tx `Q5pb…1zK6H` |
| **★ Mainnet proven** | Jupiter swap (LLM-driven through Phantom) | Jupiter A2 — tx `52bvUZ1e…D2dE1` |
| **★ Mainnet proven** | Controlled-wallet Stage 2 substrate (deposit leg) | W5b — tx `5BiNNVcK…aLwa` |
| **★★ Mainnet proven** | Chat-first funding-gated flow (Phantom funding → `budget_reserved`) | W5h-lite — funding tx `8XjiMX3G…oQen` |
| **★★ Mainnet proven** | Autonomous watcher-triggered Solend deposit (no human signature between funding and execution) | W5i — execute tx `takmz89b…jTv5`, slot 419,200,198 |
| **🟢 Devnet proven** | Orca Whirlpool swap | Devnet message-driven swap |
| **🟢 Live (controlled-wallet substrate)** | Canonical intent hash + frontend re-hash (tamper-evident) | Stage 1 Tail; on-chain anchor pending mainnet promotion |
| **🟢 Live** | Rust typestate pipeline (`proposal → simulation → policy → approval → signing`) | Source + tests |
| **🟢 Live** | Capability-gated LLM tool dispatch (LLM holds `ProposeSigning` only) | Source + tests; Phase 5G mainnet evidence |
| **🟢 Live** | Append-only `AuditRepository` (no update / delete) | Source + tests |
| **🟢 Live** | Strict closed-schema LLM tool dispatch (`additionalProperties: false`) | Phase 5A + Phase 5G |
| **🟢 Live** | Strict one-turn `ConversationHandler` (no autonomous loop) | Phase 5C + Phase 5G |
| **🟢 Live** | Phantom challenge-response wallet bind (`/wallet-bind-challenge` + `/wallet-bind-confirm`) | Source + tests |
| **🟢 Live** | Provider factory (`Disabled` / `Scripted` / `OpenAi` / `Anthropic`); fail-closed credentials | Source + tests; OpenAI mainnet, Anthropic unit-test green |
| **🟡 Prototype / partial** | Multi-step approval chain (agent → risk → treasury) | Data model + ordering tests in place; live demo / operator UX deferred to Phase 7 |
| **🟡 Prototype / partial** | M-of-N quorum approval | Infrastructure + proptest fuzzer + concurrent test; not exercised as a primary user-facing flow |
| **🟡 Prototype / partial** | Policy violation alerting (webhook on rejection) | `PolicyAlertSender` + `AlertDispatcher` live; production webhook profile adapters + retry/backoff deferred |
| **🔬 Design only / not yet built** | clawsol-authority `ExecuteAction` (on-chain program signer for Stage 2) | Spec + ProgramTest stub; not on mainnet |
| **🔬 Design only / not yet built** | `AuthorizationRecord` PDA live execution | Stage 2 spec only |
| **🔬 Design only / not yet built** | Jupiter conditional auto-execute (sibling-ix bracket on a live swap) | J4 verifier skeleton + J5 prototype + v2 `/build` sizing measured; live conditional swap explicitly preview-only until bracket fits tx-size budget |
| **🔬 Design only / not yet built** | Automatic 3-minute expiry refund on funded conditional orders | W5h-lite final-sprint compromise; refund leg deferred |
| **🔬 Design only / not yet built** | Real third-party user delegation flow (vs controlled-wallet demo) | Phase 6.2-Polish post-Hackathon |
| **🔬 Roadmap** | Squads V4 multisig adapter | Phase 7 |
| **🔬 Roadmap** | MPC / hardware signing adapters (Fireblocks / Turnkey / Fordefi / Ledger) | Phase 8 |
| **🚫 Out of scope** | Production deployment, third-party audit, treasury-grade SLA, on-call rotation | Explicitly not claimed |
| **🚫 Out of scope** | General-purpose autonomous trading loop (this is a *control plane*, not an autonomous trader) | Explicitly not claimed |
| **🚫 Out of scope** | A user-facing polished consumer wallet app | This is infrastructure |

The 🚫 row exists on purpose — a hackathon prototype that doesn't say what it *isn't* invites overclaim. See [`ROADMAP.md`](ROADMAP.md) for the post-Hackathon credibility track and the Long-term Vision.

## Protocol Standards In Use

Stage 2 work pins protocol assumptions to official sources before live execution
is claimed:

- **Pyth**: Solana price conditions target Pyth Pull Oracle /
  `pyth-solana-receiver-sdk` `PriceUpdateV2`, with feed-id verification,
  `verification_level == Full`, bounded confidence, integer math, and a
  30-second freshness target for trading triggers.
- **Solend / Save**: Solend execution is delegated-position-only. The system
  must not touch a user's existing main-wallet obligation. Withdraw paths are
  designed around Solend mainnet token-lending semantics: refresh and withdraw
  in the same transaction, delegated-wallet-owned obligation, and derived supply
  APR from reserve primitives rather than a non-existent on-chain supply-APY
  helper.
- **Jupiter**: Existing Jupiter JIT code uses the production `api.jup.ag`
  path and v0 / ALT transaction assembly. Stage 2 conditional Jupiter execution
  is not claimed yet: the next Jupiter slice must explicitly pin Swap API V2
  Router `/build`, account for `tipInstruction` and `blockhashWithMetadata`,
  measure ALT / transaction-size feasibility, and prove sibling-instruction
  verification fits. If it does not fit safely, Jupiter conditional buy remains
  preview-only.

## Natural-Language Command Samples

The `/chat` surface accepts open-ended natural-language requests; it does not
present a fixed set of buttons. The samples below show the kinds of intents
the LLM dispatcher recognises today.

**Read-only / discovery**
- `Show my Solend position.`
- `Where is my Solend USDC deposit?`
- `Preview withdraw-all for Solend obligation HcKrv5Jo5f6qvzSGhJVYTNSqwKudRizn6fxbjPW7M8SV`
- `How much USDC would I get for 0.001 SOL?`

**Solend deposit proposal**
- `Deposit 0.001 USDC into Solend.`
- `Propose a 0.001 USDC Solend deposit. Don't approve, sign, or broadcast.`

**Solend withdraw-all proposal**
- `Withdraw all USDC from Solend obligation HcKrv5Jo5f6qvzSGhJVYTNSqwKudRizn6fxbjPW7M8SV`

**Jupiter**
- `Quote 0.001 SOL to USDC.`
- `Swap 0.001 SOL to USDC with 0.5% slippage.`

**Stage 2 conditional (chat-first, funding-gated)**
- `If Save APY > 1%, deposit 0.25 USDC, expires in 3 minutes.` — creates a
  conditional order in `/chat`, opens a Phantom popup for a single 0.25 USDC
  `TransferChecked` to the controlled wallet, transitions the state machine
  from `funding_pending` → `budget_reserved`, then either renders a W5g
  approval chat command for the user to paste, **or** (when W5i auto-execution
  gates are armed) lets a 30-second polling watcher autonomously broker the
  Solend deposit within the pre-set bounds — no further user signature
  required.

**Safety / refusal examples**
- `Withdraw 5 USDC from Solend.` — assistant explains that partial Solend
  withdraw is not supported yet; only withdraw-all by explicit obligation
  is supported.
- `Deposit 0.001 USDC into Solend and approve it, sign it, and broadcast it
  for me.` — assistant explains that it can propose only; human approval
  and Phantom signing remain required.

These are examples, not buttons. The chat surface intentionally keeps the
prompt box open-ended so users can type their own request.

## Run / Verify

Pick the reviewer path that matches how deep you want to look.

### A. Quick verification — build, test, read proofs (no wallet, no RPC key, no LLM key)

```bash
git clone https://github.com/jeffreycheung521hk/testingcrypto2.git
cd testingcrypto2

cargo check                              # workspace builds
cargo test                               # 374-test pinned suite; devnet harnesses are #[ignore] by default

cat docs/proofs/INDEX.md                 # every mainnet / live-LLM proof at a glance
cat docs/proofs/PHASE5_CLOSEOUT.md       # milestone seal + security model
```

`cargo test` should report something like `… passed; 0 failed; 1 ignored` on a fresh clone — the **ignored** entry is `devnet_orca_swap_e2e`, which requires a local devnet keypair and is opt-in.

**Test-environment notes:**

- **Most live Solana / LLM harnesses are gated** behind `CLAW_LIVE_*` env vars and skip by default — `cargo test` performs no network calls and no broadcasts for the gated paths.
- **`devnet_orca_swap_e2e`** is marked `#[ignore]` because it requires a local devnet keypair. To run it, generate one and use `--ignored`:

  ```bash
  mkdir -p data
  solana-keygen new --outfile data/devnet.json
  solana airdrop 1 $(solana-keygen pubkey data/devnet.json) --url devnet

  # Run the devnet swap test specifically:
  cargo test -p claw-solana-core --test devnet_orca_swap_e2e -- --ignored --nocapture

  # Or run everything including ignored tests:
  cargo test -- --include-ignored
  ```

  Alternative: override the keypair location with `CLAW_DEVNET_KEYPAIR=/path/to/keypair.json`.

> **Windows reviewers:** if `cargo check` fails with an `openssl-sys` linker error, see [`docs/WINDOWS_BUILD.md`](docs/WINDOWS_BUILD.md) — you need to export `OPENSSL_DIR` / `OPENSSL_LIB_DIR` / `OPENSSL_INCLUDE_DIR` in your shell. macOS / Linux clones work out of the box.

### B. Run the local daemon (no live signing, no broadcast)

```bash
cp config/default.toml claw.toml         # local config — claw.toml is gitignored

export CLAW_API_TOKEN=devtest123         # any opaque string; daemon prints
                                         # an ephemeral one if you don't set this

export OPENAI_API_KEY=sk-...             # OpenAI provider (Phase 5G mainnet-proven path)
# OR
export ANTHROPIC_API_KEY=sk-ant-...      # Anthropic provider (wired, unit-test green)
# The daemon starts without either; LLM routes fail-closed until one is set.

export CLAW_RPC_URL=https://api.mainnet-beta.solana.com
                                         # or a Helius / private RPC URL.
                                         # Defaults to the public mainnet endpoint
                                         # in config/default.toml (rate-limited).

cargo run --release --bin clawd          # binds 127.0.0.1:7070 by default
```

In another terminal, open a session and send a chat message:

```bash
# Open a session — returns { "session_id": "<uuid>", "role": "execution" }
curl -s -X POST http://127.0.0.1:7070/sessions \
  -H "Authorization: Bearer devtest123" \
  -H "Content-Type: application/json" \
  -d '{"role":"execution"}'

# Send an intent (no live broadcast — the LLM proposes only)
curl -s -X POST http://127.0.0.1:7070/sessions/<SESSION_ID>/chat \
  -H "Authorization: Bearer devtest123" \
  -H "Content-Type: application/json" \
  -d '{"message":"Show my Solend position."}'
```

Without a configured wallet (see § C), signing actions fail-closed with a typed configuration error — this is the intended default for a fresh clone.

### B-2. Run the frontend (optional — adds the `/chat` UI)

The Next.js frontend at `frontend/` renders the same `/chat` state machine the curl commands in § B exercise, plus the Phantom funding popup flow used by Stage 2 conditional orders.

```bash
cd frontend
cp .env.example .env.local

# Edit .env.local with your daemon's address + token:
#   NEXT_PUBLIC_MODE=live
#   NEXT_PUBLIC_GATEWAY_URL=http://127.0.0.1:7070
#   NEXT_PUBLIC_GATEWAY_TOKEN=devtest123     (must match CLAW_API_TOKEN from § B)
#   NEXT_PUBLIC_SOLANA_RPC_URL=https://api.mainnet-beta.solana.com
#                                             (or your Helius URL)

npm install
npm run dev                              # http://localhost:3000
```

Open `http://localhost:3000/chat`. The page connects to the running daemon and exposes the LLM-driven chat surface plus the Phantom wallet-bind challenge flow (`/wallet-bind-challenge` + `/wallet-bind-confirm`). Stage 2 conditional orders (W5h-lite funding popup, W5g approval command) render here.

The frontend is stateless from the chain's perspective: it never holds private keys, never signs, and only routes signing requests through Phantom. All state-of-record lives in the daemon's SQLite store; the frontend is a thin render layer over the `ChatResponse` state machine.

### C. Configure a wallet (required for any signing path)

`config/default.toml` ships **without an active wallet block**. To exercise signing actions locally, uncomment one of the example wallet blocks in your `claw.toml` and provide credentials:

```bash
# Example: devnet keypair
mkdir -p data
solana-keygen new --outfile data/devnet.json
solana airdrop 1 $(solana-keygen pubkey data/devnet.json) --url devnet
# Then uncomment the [[wallets]] block in claw.toml that references
# data/devnet.json and restart the daemon.
```

For Phantom external signing, configure a `signer_type = "external"` wallet block with the Phantom pubkey and use the `/wallet-bind-challenge` + `/wallet-bind-confirm` flow at session open.

### D. Verify mainnet proofs (no setup required)

Every mainnet finalization is anchored to a public Solscan link, and each proof doc states **what was proven** *and* **what was not proven**:

```bash
cat docs/proofs/INDEX.md                                            # proof ledger entry-point
cat docs/proofs/LLM_SOLEND_MAINNET_E2E_PHASE5G.md                   # LLM-guided Solend deposit
cat docs/proofs/LLM_SOLEND_WITHDRAW_MAINNET_E2E_PHASE6I.md          # Solend withdraw-all
cat docs/proofs/STAGE2_W5B_SOLEND_DEPOSIT_MAINNET.md                # Stage 2 controlled-wallet substrate
cat docs/proofs/JUPITER_LIVE_MAINNET_PROOF.md                       # Jupiter A1 / A2
```

## Architecture

Four-plane architecture: Intent → Control → Execution → Observer, with explicit contracts. The Observer plane cannot sign. The Intent plane cannot bypass control. See [`ARCHITECTURE.md`](ARCHITECTURE.md) for the full crate map, security posture, and ADRs.

## Documentation

| Doc | Purpose |
|-----|---------|
| **[`docs/WEEKLY_UPDATES.md`](docs/WEEKLY_UPDATES.md)** | **★ Start here** — dated chronological record of every milestone, mainnet finalization, fail-closed defense, and scope decision (W1 through W5 / May 12 W5i autonomous-execution) |
| [`docs/proofs/INDEX.md`](docs/proofs/INDEX.md) | Every mainnet / live-LLM artifact in one table |
| [`docs/proofs/PHASE5_CLOSEOUT.md`](docs/proofs/PHASE5_CLOSEOUT.md) | Phase 5 milestone seal — security model, fail-closed record, components |
| [`ROADMAP.md`](ROADMAP.md) | Phase 6 Stage 1 Tail framing + Long-term Vision (Stage 2) |
| [`ARCHITECTURE.md`](ARCHITECTURE.md) | System invariants, security posture, crate map, API surface, ADRs |
| [`DEBT.md`](DEBT.md) | Technical debt ledger with explicit migration triggers |
| [`PITCH.md`](PITCH.md) | Hackathon pitch — problem, solution, demo path, proof |
| [`CHANGELOG.md`](CHANGELOG.md) | Versioned change log |

## Tech Stack

**Backend (Rust, Edition 2021)** · Tokio async runtime · Axum 0.7 HTTP server · `solana-sdk` 2.1.21 (V0 transactions + Address Lookup Tables) · SQLite (SQLx 0.8) durable approval / lifecycle state · ed25519-dalek 2 (4-step external-wallet signature verification) · DashMap 5 · `tracing` · compile-time typestate + `#![forbid(unsafe_code)]` across critical crates · Borsh (canonical intent serialization + SHA-256 `canonical_intent_hash`).

**Solana protocols (mainnet)** · Solend `token-lending` with processor-grade parity (account ordering / writable / signer flags verified against the deployed `processor.rs`, not just SDK helpers) · Jupiter v6 / `api.jup.ag` with V0 transactions and ALT assembly · Pyth Solana Receiver SDK (`PriceUpdateV2` decode, staleness + confidence gates, fixed-point math, `verification_level == Full`) · Helius mainnet RPC · Phantom external wallet via `signMessage` challenge-response bind + V0 `sendTransaction`.

**Solana program (Stage 2 substrate, devnet)** · `programs/` workspace housing the canonical intent / condition-gated `ExecuteAction` skeleton (P5a/P5b/P5c/P5d-lite); ProgramTest-proven success path; mainnet promotion is post-Hackathon.

**LLM** · Provider factory (`Disabled` / `Scripted` / `OpenAi` / `Anthropic`) with `ApiKey` redacting wrapper, fail-closed credential handling, no silent fallback · strict one-turn `ConversationHandler` (provider called exactly once per user turn; tool output never re-fed) · OpenAI `gpt-4o-mini` mainnet-proven (Phase 5G); Anthropic `claude-sonnet-4-6` wired and unit-test green.

**Frontend** · Next.js 16 · React 19 · shadcn/ui · Tailwind 4 · `@solana/wallet-adapter` (Phantom integration) · strict `ChatResponse` state-machine rendering on the `/chat` surface.

**Testing / CI** · 374-test pinned suite · GitHub Actions (nextest JUnit + Playwright reporters) · default-skip env gates on every live harness (`CLAW_LIVE_*` master + `*_GO` execute) — `cargo test` performs zero network calls by default · append-only `AuditRepository` (no update/delete methods) · programmatic proof-doc writes (only after `Finalized` observed).

## License

MIT — see [`LICENSE`](LICENSE).
