# Phase 5c-lite — Mainnet Proof

**Date of run:** 2026-05-16 (UTC). **Cluster:** `mainnet-beta`.
**Source commit:** `5c37e1a` on `staging2026/integrate-phase5c-lite`.

This is the first end-to-end mainnet round-trip of the **Bounded Intent
Execution Loop** at a variable amount (0.5 USDC, not the 0.25 USDC the
hackathon was pinned to). It exercises every layer that Phase 5c-lite
introduced: an LLM-assisted intent extractor at the chat surface, a
user-finalization gate that writes zero DB rows until the user
confirms, and the same canonical-hash → memo-anchor → token-delta
verifier → CAS-gated watcher → controlled-wallet executor pipeline
inherited from W5h-lite / W5i.

It is **not production-ready.** Read the [Caveats](#caveats) section
before drawing any conclusions about autonomy.

## The exact user input

```
put 0.5 USDC in Solend if APY exceeds 0.5%
```

No "Save APY" prefix. No literal `>` symbol. No explicit expiry. This
is paraphrased English that the deterministic 0.25-USDC regex
**cannot** match — it falls through to the LLM extractor.

## Draft stage (LLM-assisted parse, zero on-chain side effects)

Backend response shape:

```jsonc
{
  "status": "draft_intent_review_required",
  "draft": {
    "parser_source":     "llm_extractor",
    "draft_id":          "a3407002fbd645c18e8732483ade959c",
    "draft_hash":        "0e4f3ce85d318f7ee6aee7483c192ea08d64e6bff747e49049e53bc5bc214015",
    "protocol":          "solend",
    "asset":             "USDC",
    "display_source":    "save",
    "comparison":        "gt",
    "threshold_bps":     50,
    "threshold_pct_label": "0.5",
    "amount_raw":        "500000",
    "amount_usdc_label": "0.5",
    "expiry_seconds_after_finalize": 180,
    "controlled_wallet":   "BPfDMmeMBmCbMC1rWh7hwigMBoKGBrKwXxSeUu9hhs5L",
    "controlled_usdc_ata": "7LFdKcSV7JQYi3or5y9phHVPjhGigu5DDjUakAFbBmk3",
    "review_copy":       "If Save APY > 0.5%, deposit 0.5 USDC (= 500000 raw) into Solend Main Pool USDC. Funding window 3 minutes after confirm."
  }
}
```

**SQL state during the draft stage** (baseline 5 / 5; counts unchanged
across the draft call and four LLM retries):

```
stage2_w5h_funding_intents : 5  (unchanged)
stage2_watch_rules          : 5  (unchanged)
```

Zero DB writes before the user confirms. The draft lives in an
in-memory store with a 900 s TTL.

## User confirmation (finalize)

The user clicks **Confirm** in the chat UI. The frontend posts:

```
POST /sessions/:id/stage2/w5h/intent/finalize
{
  "draft_id":       "a3407002fbd645c18e8732483ade959c",
  "draft_hash":     "0e4f3ce85d318f7ee6aee7483c192ea08d64e6bff747e49049e53bc5bc214015",
  "user_confirmed": true
}
```

Backend response (200 OK, `W5hIntentFinalizeResultDto`):

```jsonc
{
  "status": "funding_required",
  "funding": { /* W5hConditionalOrderResultDto with rule_id_hex, memo, etc. */ },
  "finalization": { /* audit metadata */ }
}
```

DB state after Confirm:

```
rule_id_hex           : 3200000020a10700000000009a62dace
canonical_rule_hash   : f0af71705821fc0ecc389a3cede4fbc9a7611faba744ed82028b9056bd13f0ab
status                : funding_required
amount_raw            : 500000
threshold_bps         : 50
expires_at_ms - created_at_ms : 180000  (= 3 min, finalize-rooted TTL)
```

Exactly **+1** row in `stage2_w5h_funding_intents` and **+1** row in
`stage2_watch_rules`.

## Funding leg (user-signed)

The user clicks **Fund** in the chat UI. Phantom signs **one**
transaction (an SPL `TransferChecked` plus an SPL Memo anchor).
Backend funding-confirm verifies the memo payload exact + token-delta
exact via `getTransaction` with `maxSupportedTransactionVersion: 0`
and `accountIndex → message.accountKeys` resolution, then transitions
the intent to `budget_reserved`.

**Funding tx:**

| Field | Value |
|---|---|
| Signature | [`oF5XWWGaU7XeHc7rAfYHjekhW2cLM7zxKUReEWPh6U38SmRnDw9FV98xWLs9RNWwyWvXdQe7bJraUhcNquK8Je9`](https://solscan.io/tx/oF5XWWGaU7XeHc7rAfYHjekhW2cLM7zxKUReEWPh6U38SmRnDw9FV98xWLs9RNWwyWvXdQe7bJraUhcNquK8Je9) |
| Slot | 420,130,565 |
| `meta.err` | `null` |
| Memo payload | `claw:w5h:3200000020a10700000000009a62dace:f0af71705821fc0ecc389a3cede4fbc9a7611faba744ed82028b9056bd13f0ab` |

**Token-balance deltas** (parsed from `meta.preTokenBalances` /
`meta.postTokenBalances`):

| Account | Owner | Mint | Pre (raw) | Post (raw) | Δ |
|---|---|---|---|---|---|
| `4bDArC4UVoBjecHUZaCKqhuhTYPoiKS4qpQQpxuvm1qn` (user USDC ATA) | `C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW` (user main wallet) | USDC | 2,750,000 | 2,250,000 | **−500,000** |
| `7LFdKcSV7JQYi3or5y9phHVPjhGigu5DDjUakAFbBmk3` (controlled USDC ATA) | `BPfDMmeMBmCbMC1rWh7hwigMBoKGBrKwXxSeUu9hhs5L` (controlled wallet) | USDC | 653,487 | 1,153,487 | **+500,000** |

State transition: `funding_required → budget_reserved`.

## Auto-execution (W5i watcher → controlled-wallet Solend deposit)

The W5i watcher polls `stage2_w5h_funding_intents` every 30 seconds.
On a tick where (a) the intent is in `budget_reserved`, (b)
`expires_at_ms > now`, and (c) the configured Save APY exceeds the
intent's `threshold_bps`, the watcher attempts a CAS lease
(`UPDATE … SET status='executing' WHERE … status='budget_reserved'
AND expires_at_ms > now`) and on a successful lease the executor
signs and broadcasts the Solend deposit using the controlled-wallet
keypair only.

**Auto-execute tx:**

| Field | Value |
|---|---|
| Signature | [`2jynvLkVoD9TQfFH3nvMacNQZoJRpK9146TAonds1LFeBsjkBvzn2jFHsgFZASCSR5LRJGQqNgcLMQN2zSLHUVnJ`](https://solscan.io/tx/2jynvLkVoD9TQfFH3nvMacNQZoJRpK9146TAonds1LFeBsjkBvzn2jFHsgFZASCSR5LRJGQqNgcLMQN2zSLHUVnJ) |
| Slot | 420,131,308 |
| `meta.err` | `null` |
| Compute units consumed | **76,289** |
| Outer instructions | 4 |
| Inner instruction groups | 1 |

**Token-balance deltas** (parsed from `meta.preTokenBalances` /
`meta.postTokenBalances`):

| Account | Owner | Mint | Pre (raw) | Post (raw) | Δ |
|---|---|---|---|---|---|
| `7LFdKcSV7JQYi3or5y9phHVPjhGigu5DDjUakAFbBmk3` (controlled USDC ATA) | `BPfDMmeMBmCbMC1rWh7hwigMBoKGBrKwXxSeUu9hhs5L` (controlled wallet) | USDC | 1,153,487 | 653,487 | **−500,000** |
| `8SheGtsopRUDzdiD6v6BR9a6bqZ9QwywYQY99Fp5meNf` (Solend reserve USDC vault) | `DdZR6zRFiUt4S5mg7AV1uKB2z1f1WzcNYCaTEEWPAuby` (Solend market) | USDC | 10,563,382,431,973 | 10,563,382,931,973 | **+500,000** |
| `UtRy8gcEu9fCkDuUrU8EmC7Uc6FZy5NCwttzG7i6nkw` (Solend collateral pool) | `DdZR6zRFiUt4S5mg7AV1uKB2z1f1WzcNYCaTEEWPAuby` (Solend market) | cUSDC (`993dVFL2uXWYeoXuEBFXR4BijeXdTv4s6BzsCjJZuwqk`) | 23,778,468,215,888 | 23,778,468,601,432 | **+385,544** |

Exchange rate observed: **500,000 USDC → 385,544 cUSDC ≈ 1.297 USDC per cUSDC**
(current Solend Main Pool USDC supply-interest accrual).

State transition: `budget_reserved → executing → completed`.

## Verification method

All token-balance deltas above were computed programmatically from
`getTransaction(maxSupportedTransactionVersion: 0)` results:

1. `meta.preTokenBalances` and `meta.postTokenBalances` give
   `{accountIndex, mint, owner, uiTokenAmount.amount}` per row.
2. Each `accountIndex` is resolved against
   `transaction.message.accountKeys` for the pubkey.
3. `delta = int(post.uiTokenAmount.amount) - int(pre.uiTokenAmount.amount)`.
4. Mint and owner are asserted before reporting the delta.

**Solscan visual inspection alone was not used as proof.** The
Solscan links above are for human cross-checking only; the numbers
here come from the parsed RPC response.

## Caveats

### 1. Operator SQL override was required (the W5i hackathon footnote, re-surfaced)

The intent row's `expires_at_ms` was stale by the time the user
funded, because Phase 5c-lite's bridge idempotently re-used an
existing `funding_required` row from an earlier-today aborted Confirm
attempt (a TypeError in the frontend forced a retype + re-Confirm,
and the bridge matched on canonical-hash and reused the row instead
of refreshing TTL). The W5i CAS gate correctly fail-closed (5 ticks
returning `prechecks_failed: 1` because `expires_at_ms < now`) until
the operator ran the exact SQL override below:

```sql
UPDATE stage2_w5h_funding_intents
SET expires_at_ms = 1778939868387,    -- = now_ms + 600_000
    updated_at_ms = 1778939268387
WHERE rule_id_hex = '3200000020a10700000000009a62dace'
  AND status = 'budget_reserved';
-- rows_affected = 1
```

Only the two timestamp columns changed. `rule_id_hex`,
`canonical_rule_hash_hex`, `amount_raw=500000`, `funding_signature`,
controlled wallet, USDC ATA, and `status='budget_reserved'` were all
**unchanged** by the override — the CAS gate's strictness against
expiry is what the 5 pre-update ticks demonstrated, the loop's
freedom from operator state mutation is **not**.

**This is the same gap documented for the hackathon W5i proof.**
Phase 5c-lite did not close it. A single-sitting watcher-only
round-trip — fresh chat command → Confirm → Fund → autonomous
execute — **without any operator state mutation** is **not yet
proven on mainnet** for Phase 5c-lite.

#### Reviewer template (for reproducing this proof)

If you reproduce the loop, **do not literally copy the SQL above** —
the `rule_id_hex` and timestamps are specific to the original run.
Use this template:

```sql
-- Look up your row first (replace <YOUR_RULE_ID> with the rule_id_hex
-- the chat UI logged on Confirm; it appears in the funding card's
-- memo and in the daemon log line
-- "phase5c-lite finalize: confirm → W5h funding_required minted").
SELECT rule_id_hex, status, expires_at_ms, created_at_ms
FROM stage2_w5h_funding_intents
WHERE rule_id_hex = '<YOUR_RULE_ID>';

-- Only run the override if status='budget_reserved' AND expires_at_ms < now.
-- The override extends expiry; everything else is identity-preserving.
UPDATE stage2_w5h_funding_intents
SET expires_at_ms = <NOW_MS> + 600000,    -- 10 minutes from "now"
    updated_at_ms = <NOW_MS>
WHERE rule_id_hex = '<YOUR_RULE_ID>'
  AND status = 'budget_reserved';
-- rows_affected MUST equal 1; abort otherwise.
```

This is documented as a **known Phase 5c-lite gap, not production
behaviour**. See [docs/REVIEWER_QUICK_PATH.md](REVIEWER_QUICK_PATH.md)
for the reviewer-side workflow.

### 2. `gpt-4o-mini` is too conservative for the new amount band

The Phase 5c-lite system prompt explicitly authorises 0.5 USDC. But
`gpt-4o-mini` consistently refused the exact input above on 6/6
calls (including its own suggested canonical example). Switching to
`gpt-4o` at startup (`CLAW_CHAT_MODEL=gpt-4o`) was required to
unblock the run. Prompt tuning to make `gpt-4o-mini` accept the
spec phrasing is a separate slice.

### 3. UI copy bug on the funding card

The funding card's banner text hardcoded the string "0.25 USDC" in
the legacy template, so the live demo briefly showed "controlled
wallet holds the 0.25 USDC" even though the on-chain transfer was
500,000 raw. The on-chain amount is correct; the UI banner needs
templating against the typed amount.

### 4. Re-Confirm of an expired row is silently undefined

When a user re-Confirms an existing `funding_required` row whose
`expires_at_ms` has lapsed (as happened here after the frontend
TypeError forced a retype), Phase 5c-lite's bridge accepts the
Confirm, the watcher silently refuses to lease, and the UI surfaces
no diagnostic. **Refreshing TTL on re-Confirm, or hard-rejecting
re-Confirm of expired rows with a typed error, is a required
follow-up slice before this can be claimed as a clean loop.**

### 5. Two frontend wire-shape fixes were needed at integration time

The Agent-A frontend deliveries (`a469817`, `be1c5dc`, `1bb178e`)
realigned only the finalize REQUEST body and missed both response
shapes. The integration commit `5c37e1a` on
`integrate-phase5c-lite` ships the 29-line patch in
`frontend/src/lib/api.ts` that makes the UI render against Agent D's
backend at `ade3d6e`. Without that patch the demo crashes at the
draft card AND again at the funding card.

## What this proves

- **LLM-drafted intent at a variable amount.** `gpt-4o` extracted
  `{amount_raw: 500000, threshold_bps: 50, …}` from English
  paraphrase, schema-validated against the Phase 5c-lite typed
  Intent (band `[100_000, 1_000_000]` raw USDC), and minted a draft
  with zero DB side effects.
- **User finalization gate.** No `stage2_w5h_funding_intents` or
  `stage2_watch_rules` row was written until the user clicked
  Confirm. The draft store is in-memory with a 900 s TTL.
- **Memo-anchored funding.** The user-signed funding transaction
  included an SPL Memo at instruction index 0 with payload
  `claw:w5h:<rule_id>:<canonical_hash>`. The backend's
  funding-confirm asserts memo-payload-exact and token-delta-exact
  before transitioning to `budget_reserved`.
- **CAS-gated execution.** The W5i watcher's lease query
  (`UPDATE … WHERE status='budget_reserved' AND expires_at_ms > now`)
  enforces single-execution. The 5 pre-override ticks fail-closed on
  expiry; the 6th tick (post-override) succeeded. The CAS is doing
  its job.
- **Controlled-wallet-only signer for execution.** The Solend
  deposit at slot 420,131,308 was signed by the controlled wallet
  (`BPfDMm…hhs5L`) only. The user's main wallet
  (`C4QQ…GPNW`) signed only the funding transfer at slot
  420,130,565 — never the Solend deposit.
- **The loop generalises beyond 0.25 USDC.** Same code, different
  amount, same canonical-hash + memo + funding verifier + CAS +
  executor pipeline.

## What this does NOT prove

- **No single-sitting watcher-only round-trip** — see Caveat 1.
- **No production-ready posture** — the operator-SQL gap, the
  `gpt-4o-mini` conservatism, the UI copy bug, the silent
  re-Confirm-after-expiry behaviour, and the two integration-time
  wire-shape fixes are all defects of varying severity.
- **No `clawsol-authority` PDA signer** — the controlled wallet is
  still a plain keypair under operator control.
- **No `AuthorizationRecord` PDA on-chain delegation.**
- **No Jupiter execution** — Jupiter is a planned next adapter, not
  part of this proof.
- **No multi-protocol live** — Solend is the only adapter exercised.
- **No durable / multi-tenant / distributed scheduler** — the
  watcher is a single-process `tokio::time::interval` loop.
