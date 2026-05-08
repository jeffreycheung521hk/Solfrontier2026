# Stage 1 Tail — Canonical Intent Demo: Proof & Evidence Plan

**Status:** planning document for Hackathon Stage 1 Tail D' submission.
**This file is a plan, not a verified result.**
For verified live mainnet finalizations, see [`INDEX.md`](INDEX.md). The
proof posture in [`INDEX.md`](INDEX.md) is preserved: only artifacts whose
success criterion has been observed land in the ledger.

This document collects:
1. The mainnet evidence we already hold (used as the "we ship live" foundation).
2. The Stage 1 Tail devnet demo we will produce (canonical intent + tamper-evident binding).
3. The 3-minute attack demo script.
4. Security framing — what Stage 1 Tail does and does **not** prove.
5. Compile-time safety rules for the demo-only attack toggle.
6. Wording guardrails to prevent overclaim.

---

## 1. Mainnet Evidence (existing, verified live, used as ground truth)

These are the finalized chain artifacts the demo uses as the "we ship to
mainnet" anchor. Each tx is independently verifiable on Solscan; the
proof docs were written programmatically by their harnesses only after
the success criterion was observed.

| # | What it proves | Public Evidence | Proof Doc Status |
|---|---|---|---|
| 1 | Non-LLM Solend mainnet baseline (Phase 4C) | [`2QqSfDq…pxALs`](https://solscan.io/tx/2QqSfDq53a34WDXVkvZeyW59eFJcxZtkQtjZw25CoUizEmzDjahusKvBT8eNb4XdpKYwrgGcTyBwYXgth5wpxALs) | [`SOLEND_USDC_DEPOSIT_PHASE4C_E2E.md`](SOLEND_USDC_DEPOSIT_PHASE4C_E2E.md) |
| 2 | LLM-guided Solend mainnet (Phase 5G — NL → OpenAI → daemon → Phantom → finalized) | [`4M4ezLgm…Py3y`](https://solscan.io/tx/4M4ezLgm1mFpGmUpLJdDAVhfXYwUxjS2ZMkjKprBiWzsfgNudPkhEvBr6GdJbh1zBscKLF6kpUBhZg7tAm3ePy3y) | [`LLM_SOLEND_MAINNET_E2E_PHASE5G.md`](LLM_SOLEND_MAINNET_E2E_PHASE5G.md) |
| 3 | Jupiter A1 — daemon-path swap (`complete_v0` + `send_raw_v0_transaction`) | [`Q5pbBmjj…1zK6H`](https://solscan.io/tx/Q5pbBmjjn7tbFKEyswzyFsAkYoGHVJe4TT2m2viGbBCa2j9LXrFQ8X8AeKjdXw3seom2YYezFdtS8jBbpA1zK6H) | [`JUPITER_LIVE_MAINNET_PROOF.md`](JUPITER_LIVE_MAINNET_PROOF.md) |
| 4 | Jupiter A2 — LLM-driven Phantom swap | [`52bvUZ1e…D2dE1`](https://solscan.io/tx/52bvUZ1eEHjrbxRBQBssYWg7BrFNoAY1Zeynw44hr3BKTAe9zTHv9Qz4wQsNi7Jw6sbXpb8zehHafn1gFzLD2dE1) | [`JUPITER_LIVE_MAINNET_PROOF.md`](JUPITER_LIVE_MAINNET_PROOF.md) |
| 5 | Solend withdraw-all (Phase 6I-K) — round-trip closure | [`3tMpTSEn…EZPJZ`](https://solscan.io/tx/3tMpTSEnmvszjSwgow43KMd4dLqnZwwSvnTbHkcPsCiyfizLf2h7kcMc1M7LwokHLC3SBPzpTfykjsc48z8EZPJZ) | **observed live; proof doc entry pending before submission** |

**Note on item 5:** The withdraw was finalized on mainnet on 2026-05-08
(slot 418,395,346) and is referenced in `docs/WEEKLY_UPDATES.md`. A
formal proof-doc entry under `docs/proofs/` and a row in
[`INDEX.md`](INDEX.md) should be added before submission. The Stage 1
Tail demo may quote this tx as the "we shipped a real round-trip to
mainnet" anchor; the wording must say "observed live, proof doc
pending" until the entry exists.

---

## 2. Stage 1 Tail Devnet Demo — Planned Evidence Checklist

Stage 1 Tail introduces a **canonical intent** layer between the user
and signing. The intent is hashed deterministically, the hash is
recorded on-chain in the same atomic tx as the action, and the frontend
re-hashes the canonical intent immediately before signing to detect
backend tampering.

The demo will produce devnet evidence for each of the following items.
Each row should land in a follow-up proof doc with a Solscan-equivalent
devnet explorer link before submission.

| # | Demo evidence item | Status |
|---|---|---|
| 1 | Canonical intent JSON schema (versioned, stable field order) | spec drafted; capture in proof doc |
| 2 | `expires_at_slot` is part of the canonical hash input | spec drafted; capture in proof doc |
| 3 | Rust ↔ TypeScript hash parity (same canonical bytes ⇒ identical 32-byte hash) | unit-test pair planned |
| 4 | `record_intent` devnet program account list + ix data layout | source pending; capture as struct ref |
| 5 | Intent PDA derivation seed (`intent_v1`, intent_id, slot) | spec drafted; capture in proof doc |
| 6 | Atomic tx: `record_intent` + action ix in the **same** tx | devnet finalized tx hash to capture |
| 7 | Frontend canonical preview rendered before signing | screenshot/recording planned |
| 8 | Frontend re-hash of the canonical intent **after** receiving the unsigned tx, **before** Phantom popup; refuse signing on mismatch | code path + recording planned |
| 9 | Attack demo: backend tampers with action ix → frontend re-hash mismatch → **Sign disabled / refused** | recording planned (compile-time-gated) |
| 10 | On-chain Intent PDA visible on devnet explorer (forensic anchor after signing) | devnet account address to capture |

Each completed item should be reflected with a one-line entry in a
new `STAGE1_TAIL_DEVNET_PROOF.md` (yet to be written) and, where
appropriate, cross-linked from [`INDEX.md`](INDEX.md) under a clearly
labelled "Stage 1 Tail (devnet)" sub-section. Until that doc exists,
this plan is the operative scoping document.

---

## 3. Security Framing — Stage 1 Tail is *tamper-evident*, not *action-enforcing*

This framing is non-negotiable in narration, the demo voice-over, and
any pitch deck slide that touches Stage 1 Tail.

**What Stage 1 Tail proves:**

- **Tamper-evident binding** — the canonical intent's hash is recorded
  on-chain alongside the action. After signing, anyone can read the
  Intent PDA and verify what intent was claimed.
- **Frontend re-hash detects backend tampering** — if the backend
  serves an `intent` for the canonical preview but later swaps the
  underlying unsigned tx bytes, the frontend's pre-sign re-hash will
  not equal the recorded `intent.hash`, and the Sign button refuses to
  fire. This catches a **compromised backend** in the time window
  between user preview and Phantom signature.
- **Forensic / audit anchor after signing** — the on-chain Intent PDA
  is a public, immutable witness of "the user's stated intent at this
  slot." Even if the action ix later proves malicious, the canonical
  intent record exists on-chain to support attribution and post-hoc
  audit.

**What Stage 1 Tail does NOT prove (be explicit in narration):**

- **Weak binding only.** `record_intent` and the action ix are atomic
  in the same tx, but `record_intent` does **not** decode or enforce
  the action ix's bytes. A malicious backend that produces an action ix
  whose bytes are *consistent with* a canonical intent the user
  approved (same amount, same destination, but, say, a different
  inner-instruction call structure) will pass the frontend re-hash.
- **No on-chain action enforcement.** The Solana runtime has no
  knowledge that the action ix should match the intent. There is no
  on-chain assertion that the action and the intent agree at the
  semantic level — only that both were submitted in the same tx.
- **Strong action-binding is Stage 2.** Stage 2 is where the on-chain
  intent record actually constrains the action ix (e.g., a thin wrapper
  program that validates the action ix's accounts, program id, and
  data prefix against the intent before allowing it to proceed).

**Stage 1 Tail's contribution to the overall safety story:**

- Adds a *user-readable preview* that is hashable, reproducible, and
  recorded on-chain.
- Makes one important class of attack (backend swap-of-bytes after
  preview) demonstrably catchable in the frontend before signing.
- Does **not** replace the existing safety pipeline (policy engine →
  human approval → Phantom signature → daemon submit). It is **an
  additional layer**, not a substitute.

If a question is asked in the form "does Stage 1 Tail stop a malicious
LLM from inserting a bad action?", the honest answer is: **only if the
bad action would change the canonical preview the user already
approved**; if the LLM produces an action consistent with the previewed
intent, Stage 1 Tail will pass. That is what Stage 2 is for.

---

## 4. Attack Demo Script (3 minutes)

This is the recording / live-pitch script. Deliver in three minutes
with one operator at the keyboard.

### 0:00 – 0:30 — Problem framing

> "LLMs and backend services can misparse intent or, in the worst
> case, be compromised. The user sees one description in the UI, but
> the actual transaction the wallet signs may be different. Most DeFi
> chat front-ends don't surface that gap. We're going to show what we
> do about it."

### 0:30 – 1:10 — Existing mainnet proof

Show, on screen:

- The Phase 5G mainnet tx
  [`4M4ezLgm…Py3y`](https://solscan.io/tx/4M4ezLgm1mFpGmUpLJdDAVhfXYwUxjS2ZMkjKprBiWzsfgNudPkhEvBr6GdJbh1zBscKLF6kpUBhZg7tAm3ePy3y)
  on Solscan ("LLM proposed, human approved, Phantom signed, mainnet
  finalized").
- Optionally the Phase 6I-K withdraw round-trip
  [`3tMpTSEn…EZPJZ`](https://solscan.io/tx/3tMpTSEnmvszjSwgow43KMd4dLqnZwwSvnTbHkcPsCiyfizLf2h7kcMc1M7LwokHLC3SBPzpTfykjsc48z8EZPJZ)
  ("the same flow gives funds back").

> "Mainnet path is real. We have finalized live txs for deposit, swap,
> and withdraw, end-to-end through OpenAI and Phantom."

### 1:10 – 2:10 — Stage 1 Tail devnet demo (happy path)

Switch to the devnet demo build. Walk through:

1. User asks the chat to do something (e.g., "deposit 0.005 USDC into
   Solend").
2. The frontend renders the **canonical intent preview**: action,
   amount, destination, expires_at_slot, intent_id, and the canonical
   intent's 32-byte hash.
3. User clicks "Approve & Sign".
4. **Before** Phantom opens, the frontend re-hashes the canonical
   intent and compares it to what the daemon claims.
5. Phantom popup appears; user signs; daemon submits an atomic
   tx containing both `record_intent` and the action ix.
6. Devnet explorer shows the Intent PDA written, and the action ix
   succeeded.

> "What you just saw: the user's intent was hashed, written on-chain
> in the same atomic tx as the action, and the frontend re-hashed
> right before signing to make sure nothing changed."

### 2:10 – 2:40 — Attack mode (the lean-in beat)

Toggle the **demo-only attack mode** (compile-time gated; see §5
below). Repeat the same chat prompt:

1. User asks the same thing.
2. The frontend renders the same canonical preview, same hash.
3. User clicks "Approve & Sign".
4. The backend (in attack mode) **silently swaps** the action ix's
   `amount` field (or `slippage_bps`) to a different value before
   serving the unsigned tx.
5. The frontend re-hashes — the hash **does not match** the canonical
   preview the user already saw.
6. **Phantom never opens.** The Sign button refuses. The UI shows a
   "tamper detected" banner referencing the mismatch.

> "Same UI, same backend, same wallet — but the swap happened. The
> frontend caught it before Phantom ever saw a signing prompt."

### 2:40 – 3:00 — Roadmap (close honest)

> "What you just saw is **tamper-evident**. We catch backend tampering
> in the frontend, and we leave a forensic record on-chain. That's
> not the same as **action-enforcing** — for that, the on-chain intent
> record has to actually constrain the action bytes. Stage 2 adds
> strong action-binding, conditions, and authorize-now / execute-later.
> The on-chain proof you'll see today is one step on that path, not the
> finish line."

---

## 5. Attack-Mode Safety — Compile-Time Gating

The "demo attack mode" must not be reachable in any production build.
The acceptable gating posture is:

- **Mandatory:** the entire attack-mode code path must be behind a
  Cargo feature flag, e.g. `#[cfg(feature = "demo-attack")]`. The
  feature must be off by default.
- **Insufficient on its own:** runtime env vars (`DEMO_ATTACK=1`,
  config flags, hidden URL params). These are easy to flip in
  production by accident or by attacker. Do **not** rely on them as the
  sole gate.
- **Production build invariant:** `cargo build --release` without
  `--features demo-attack` must **not** include the attack-mode
  code path in the resulting binary. CI should run a check that
  fails if production builds detect the demo-attack symbol.
- **Frontend mirror:** if the attack toggle has any frontend
  counterpart, gate it via build-time `process.env.NEXT_PUBLIC_DEMO_ATTACK`
  consumed only inside a `// @ts-expect-error / build-time-only` block
  *and* only emitted by a separate "demo build" target, not the
  normal production target.
- **Observability:** when attack mode is active, the daemon must
  emit a clearly distinguishable startup log line (e.g.,
  `WARNING: demo-attack feature compiled in — DO NOT DEPLOY`)
  and stamp every audit row with an `attack_mode: true` marker.
- **Repository hygiene:** the attack-toggle source files should
  carry a banner comment `THIS FILE IS DEMO-ONLY. DO NOT INVOKE FROM
  PRODUCTION CODE PATHS.` and a unit test that asserts the symbol
  is unreachable when the feature is off.

If any of these invariants is violated, the attack-mode demo **must
not** ship — the risk of leaving a tamper-injection point in a real
binary outweighs the demo value.

---

## 6. Wording Guardrails (apply to script, slides, and any narration)

These phrases are **safe** to use in narration:

- "tamper-evident binding"
- "frontend re-hash catches backend tampering before signing"
- "on-chain forensic anchor"
- "weak binding — atomic but not enforcing"
- "Stage 2 adds strong action-binding"
- "no funds were at risk in any failed attempt; only the per-tx fee was paid"

These phrases are **not safe** and must be avoided:

- "tamper-proof" → use "tamper-evident".
- "secure against malicious LLMs" → too broad; we only catch one
  attack class (preview/sign mismatch).
- "the on-chain record prevents bad actions" → it does not. It
  prevents *deniable* bad actions; it doesn't prevent execution.
- "trustless" → we still trust the wallet, the user's review, and the
  Solana runtime; "trust-minimised" is more honest if needed.
- "production-ready" → this is hackathon-stage prototype evidence.

If a question can't be answered safely from the safe list, the honest
answer is "Stage 1 Tail is tamper-evident, not action-enforcing — that
class of guarantee is Stage 2."

---

## 7. Cross-References

- [`INDEX.md`](INDEX.md) — verified live mainnet & live-LLM proof
  ledger (the source of truth this plan is anchored to)
- [`PHASE5_CLOSEOUT.md`](PHASE5_CLOSEOUT.md) §5 — fail-closed defence
  record (three documented mainnet incidents, zero funds lost) — the
  prior art the Stage 1 Tail demo extends
- [`LLM_SOLEND_MAINNET_E2E_PHASE5G.md`](LLM_SOLEND_MAINNET_E2E_PHASE5G.md)
  — the reference proof for the natural-language → mainnet flow the
  attack demo extends with a tamper-evident layer
- [`JUPITER_LIVE_MAINNET_PROOF.md`](JUPITER_LIVE_MAINNET_PROOF.md) —
  the swap reference proof
- `docs/DEMO_SCRIPT_NL_TO_DEFI.md` — sister demo script for the
  policy/control-plane narrative; coordinate framing so the two
  demos do not contradict each other

---

## 8. Open Items Before Submission

These should be resolved (and reflected in `INDEX.md` / a new proof
doc) before the demo records:

- [ ] Phase 6I-K Solend withdraw round-trip — write
      `SOLEND_WITHDRAW_MAINNET_E2E_PHASE6I.md` and add a row to
      [`INDEX.md`](INDEX.md). Wallet, obligation, reserve, slot, and
      fee-only failed attempts are already captured in
      `docs/WEEKLY_UPDATES.md`'s May 8 entry; a programmatic proof
      writer should be the canonical source.
- [ ] Stage 1 Tail devnet capture: produce `STAGE1_TAIL_DEVNET_PROOF.md`
      with the 10 evidence items from §2 above filled in with
      devnet tx hashes / Intent PDA addresses.
- [ ] Compile-time-gating audit: confirm
      `cargo build --release` produces a binary with **no**
      `demo-attack` symbol; CI failure on regression.
- [ ] Demo recording rehearsal: confirm the attack-mode toggle is
      off in the recording machine's default build, and on only inside
      the rehearsed window.

---

## Document provenance

- Author: Stage 1 Tail submission planning, 2026-05-08.
- This document is **a plan**, not an observed-result proof. It
  references existing live-mainnet proofs (items 1–4 in §1) and one
  observed-but-not-yet-doc'd live mainnet artifact (item 5). It does
  not itself produce or claim any new mainnet result.
