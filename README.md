# Solfrontier 2026 — Phase 5c-lite

This repository carries the proof and the implementation of the
**Bounded Intent Execution Loop**: a runtime that lets a user
pre-commit to a conditional on-chain action without handing the
agent their main-wallet keys.

The user signs **once**, at funding time. After that, a
controlled-wallet executor handles the chain-side work — but only
within the typed bounds the user explicitly **finalized** in the
chat UI.

> **The first live proof at 0.5 USDC ran on mainnet on 2026-05-16.**
> See [`docs/PHASE5C_LITE_MAINNET_PROOF.md`](docs/PHASE5C_LITE_MAINNET_PROOF.md)
> for the exact transactions, parsed token deltas, and the honest
> operator-SQL caveat.

## The loop in one paragraph

A user types a structured chat command. A schema-validated LLM
extractor (`gpt-4o`) produces a typed `DraftIntent` whose canonical
SHA-256 hash is shown back to the user in a **review card**. **No
DB row exists at this point.** The user clicks **Confirm**; the
backend finalizes the draft into a persisted `funding_required`
intent and surfaces a Phantom funding card. The user clicks
**Fund**; their main wallet signs **one** SPL `TransferChecked`
(plus an SPL Memo anchor binding the canonical hash to the chain).
The backend verifies memo-exact and token-delta-exact before
flipping the state to `budget_reserved`. A polling watcher
(`tokio::time::interval`, 30 s) re-fetches the condition; on a
match it CAS-leases the row (`UPDATE … WHERE
status='budget_reserved' AND expires_at_ms > now`) and the
controlled-wallet executor signs and broadcasts the Solend deposit.
**The user's main wallet never signs a Solend instruction.**

```
chat command (structured)
  ↓
LLM draft (gpt-4o, JSON-schema-bounded)
  ↓
typed Intent + canonical hash → user review card (zero DB writes)
  ↓
user Confirm → finalize (persist 1 watch_rule + 1 funding_intent)
  ↓
Phantom funding (user signs ONE SPL TransferChecked + Memo)
  ↓
backend memo-exact + token-delta-exact verifier
  ↓
state: budget_reserved
  ↓
W5i watcher (30 s tick; CAS gate)
  ↓
controlled-wallet executor (signs + broadcasts the adapter tx)
  ↓
state: completed (on-chain evidence in PHASE5C_LITE_MAINNET_PROOF.md)
```

## What is proven (verbatim, no rounding)

- LLM-drafted intent at **0.5 USDC** with `threshold_bps=50`. See
  [PHASE5C_LITE_MAINNET_PROOF.md §"The exact user input"](docs/PHASE5C_LITE_MAINNET_PROOF.md).
- User-finalization gate writes **zero DB rows** until Confirm.
- Memo-anchored funding tx
  [`oF5XWWGa…Je9`](https://solscan.io/tx/oF5XWWGaU7XeHc7rAfYHjekhW2cLM7zxKUReEWPh6U38SmRnDw9FV98xWLs9RNWwyWvXdQe7bJraUhcNquK8Je9)
  at slot 420,130,565 — user USDC ATA `−500,000` raw, controlled
  USDC ATA `+500,000` raw, parsed via
  `meta.preTokenBalances`/`meta.postTokenBalances`.
- Controlled-wallet-only Solend deposit
  [`2jynvLkV…UVnJ`](https://solscan.io/tx/2jynvLkVoD9TQfFH3nvMacNQZoJRpK9146TAonds1LFeBsjkBvzn2jFHsgFZASCSR5LRJGQqNgcLMQN2zSLHUVnJ)
  at slot 420,131,308 (76,289 CU) — controlled USDC ATA
  `−500,000` raw, Solend USDC reserve vault `+500,000` raw, Solend
  collateral pool `+385,544` cUSDC.

**Solend USDC is the first adapter.** Jupiter is a roadmap adapter
and is **not** part of this proof.

## Caveats — read these (the loop is NOT production-ready)

1. **Operator SQL override was required in the proof.** The W5i
   hackathon footnote re-surfaced: the watcher refused to lease the
   intent because `expires_at_ms < now`, and a single
   `UPDATE … SET expires_at_ms = now + 600s` was needed to unblock
   it. **A single-sitting watcher-only round-trip without operator
   SQL is NOT yet proven on Phase 5c-lite.** See
   [PHASE5C_LITE_MAINNET_PROOF.md §"Caveats"](docs/PHASE5C_LITE_MAINNET_PROOF.md#caveats)
   and the reviewer-side SQL template in
   [REVIEWER_QUICK_PATH.md](docs/REVIEWER_QUICK_PATH.md).

2. **`gpt-4o` was used for the proof.** `gpt-4o-mini` consistently
   rejected the spec phrasing on 6/6 calls even though the system
   prompt explicitly authorises 0.5 USDC. Prompt tuning for
   `gpt-4o-mini` is a separate slice. Default the demo daemon to
   `CLAW_CHAT_MODEL=gpt-4o` until that's resolved.

3. **UI copy bug.** The funding card's banner text briefly said
   "controlled wallet holds the 0.25 USDC" even when the on-chain
   amount was 0.5 USDC — the legacy template wasn't refactored for
   a variable amount. On-chain amount is correct; UI display is
   stale.

4. **Not production-ready.** Beyond the above, there is no
   `clawsol-authority` PDA signer, no `AuthorizationRecord`
   on-chain delegation, no production scheduler (watcher is a
   single-process `tokio::time::interval`), no multi-tenant
   controlled-wallet isolation, and no off-chain key-rotation
   policy.

## What this does **NOT** claim

- **No arbitrary natural language without schema validation.** The
  LLM's tool output is validated in Rust against a strict typed
  Intent schema. Anything outside the band is rejected, not parsed
  loosely. The user **CANNOT** instruct the system to relax this.
- **No LLM in the autonomous execution hot path.** The watcher,
  CAS gate, executor, and broadcast layers are pure Rust across
  all phases. The LLM lives only at the chat → DraftIntent
  boundary.
- **No "ChatGPT controls the wallet".** The LLM never touches the
  controlled-wallet keypair, never calls an RPC, never reads or
  writes persisted state. It produces a typed proposal; the user
  must explicitly Confirm; the runtime trusts only the finalized
  canonical intent from that point onward.
- **No Jupiter live.** Jupiter is a planned next adapter.
- **No single-sitting autonomous round-trip** — see Caveat 1.

## Where to start as a reviewer

Read in order:

1. [`docs/PHASE5C_LITE_MAINNET_PROOF.md`](docs/PHASE5C_LITE_MAINNET_PROOF.md)
   — the headline mainnet evidence + the explicit caveats.
2. [`docs/REVIEWER_QUICK_PATH.md`](docs/REVIEWER_QUICK_PATH.md) —
   five-minute guided walk and the operator-SQL workaround.
3. [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — the ten-step
   loop and the boundary commitments table.
4. [`docs/SECURITY_BOUNDARIES.md`](docs/SECURITY_BOUNDARIES.md) —
   B1–B6 enforced + explicit gaps.
5. [`docs/DEMO_EVIDENCE.md`](docs/DEMO_EVIDENCE.md) — hackathon
   mainnet evidence pointers.
6. [`docs/ROADMAP.md`](docs/ROADMAP.md) — what's next.

## Module layout

| Path | Role |
|---|---|
| `crates/types` | typed Intent, canonical hashing primitives |
| `crates/api`, `crates/gateway` | HTTP surface (REST + chat dispatch) + bridge |
| `crates/state-store` | SQLite-backed intent + watch-rule + budget repositories |
| `crates/wallet-engine`, `crates/agent-runtime` | LLM provider plumbing, signing primitives |
| `crates/risk-engine`, `crates/tool-system` | policy / capability gates |
| `bins/clawd` | the gateway daemon |
| `programs/clawsol-*` | on-chain program scaffolds (PDA-authority track; **NOT** signing in this proof) |
| `frontend/src` | Next.js / TypeScript chat UI |
| `frontend/scripts/verify-*.ts` | reviewer parity checks (run with `node`) |

## Building and running locally

See [`docs/WINDOWS_BUILD.md`](docs/WINDOWS_BUILD.md) for the Windows
OpenSSL setup. On macOS / Linux, `cargo check --workspace` should
work straight out of `pkg-config` for OpenSSL.

```bash
# backend
cargo build --release -p clawd --bin clawd
cargo test  -p claw-gateway --lib stage2_phase5c_draft
cargo test  -p claw-gateway --lib stage2_w5h_bridge
cargo test  -p claw-gateway --lib stage2_w5h_funding_confirm
cargo test  -p claw-gateway --lib stage2_w5i_auto_execute
cargo test  -p claw-gateway --lib chat_wiring
cargo test  -p claw-api     --lib

# frontend
cd frontend
npm ci             # or npm install
npx tsc --noEmit
npx eslint src
npm run build
node scripts/verify-w5h-draft-intent.ts
node scripts/verify-w5h-tx-shape.ts
node scripts/verify-stage2-funding-poll.ts
```

For environment variable templates, see:

- `config/example.toml` (gateway config) and the existing
  `config/jupiter-mainnet-e2e.example.toml` /
  `config/phantom-e2e.example.toml`.
- `.env.example` (gateway env vars at run-time).
- `frontend/.env.example` (frontend gateway URL + token).

**Never commit real keys, real wallet keypair files, or real
mainnet config files.** See `.gitignore` and
[`docs/SECURITY_BOUNDARIES.md`](docs/SECURITY_BOUNDARIES.md).

## License

[MIT](LICENSE).
