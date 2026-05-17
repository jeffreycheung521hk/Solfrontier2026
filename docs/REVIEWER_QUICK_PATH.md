# Reviewer Quick Path

This branch contains the source for the Bounded Intent Execution
Loop and the documented mainnet proof of its Phase 5c-lite slice.
A five-minute guided walk — read in order:

1. **[README.md](../README.md)** — the loop in one paragraph, the
   "no-overclaim" caveats list, and the link to the proof.
2. **[PHASE5C_LITE_MAINNET_PROOF.md](PHASE5C_LITE_MAINNET_PROOF.md)**
   — the headline mainnet evidence (parsed token-balance deltas,
   not Solscan visuals) + the operator-SQL caveat.
3. **[ARCHITECTURE.md](ARCHITECTURE.md)** — the twelve-step loop and
   the "what this architecture says NO to" table.
4. **[SECURITY_BOUNDARIES.md](SECURITY_BOUNDARIES.md)** — what the
   loop enforces (B1–B6) and what it does NOT yet enforce.
5. **[DEMO_EVIDENCE.md](DEMO_EVIDENCE.md)** — pointer summary +
   hackathon predecessors.
6. **[ROADMAP.md](ROADMAP.md)** — what's next.

## What to look at first when auditing the claims

- **`crates/gateway/src/stage2_phase5c_draft.rs`** — the in-memory
  draft store. This is the load-bearing module for the
  "zero DB writes before user confirms" claim.
- **`crates/api/src/routes/stage2_w5h_intent_finalize.rs`** — the
  finalize route. Discriminates `user_confirmed: true/false`.
  Returns `funding_required` (with a nested `funding` body) on
  confirm and `rejected` on reject. Drops the draft idempotently
  in both cases.
- **`crates/gateway/src/stage2_w5h_funding_confirm.rs`** — the
  memo-exact + token-delta-exact verifier (boundary **B3**).
- **`crates/gateway/src/stage2_w5i_auto_execute.rs`** — the
  watcher and its CAS-lease call. The expiry predicate
  (`expires_at_ms > now`) is what made the proof fail-close
  pre-override; see Caveat 1 below.
- **`crates/gateway/src/stage2_chat_execute.rs`** — the executor.
  Holds the controlled-wallet keypair; no frontend path touches
  it.
- **`frontend/src/lib/api.ts`** —
  `normalizeChatResponseWireShape` and `finalizeW5hIntent`. The
  29-line patch in commit `5c37e1a` is what makes the demo render
  against the backend at `ade3d6e`; without it, the UI crashes at
  both the draft card and the funding card.
- **`frontend/src/app/chat/page.tsx`** — the single
  `signTransaction()` + single `sendRawTransaction()` call sites
  (boundary **B1**).

## What I think is worth your attention

- **The boundary commitments are negative, not positive.** The
  architecture commits to what it does NOT do (no LLM in
  execution, no production scheduler, no PDA authority yet, no
  single-sitting watcher-only round-trip yet). That's the
  load-bearing part — the negatives are what make the loop
  evaluatable.
- **The CAS gate is the only protection against double-execution.**
  Both a manual approval path and the autonomous watcher hit the
  same `UPDATE … WHERE status = 'budget_reserved' AND
  expires_at_ms > now`. Two callers, exactly one winner.
- **The Memo anchor makes the funding-to-rule binding recoverable
  from the chain without backend state.** That's a deliberate
  redundancy.
- **The Phase 5c-lite proof required an operator-side SQL
  override.** This is not a footnote; it is the load-bearing gap
  this redesign needs to close. The autonomous loop fail-closed
  correctly against the stale expiry; execution only succeeded
  after the operator mutated state.

## Operator SQL override — when, why, and the reviewer template

### When you need it

You will hit this if you reproduce the proof in a sitting where:

- you typed the structured chat command, hit the draft review card,
- you clicked **Confirm** more than 3 minutes ago,
- you then click **Fund** in Phantom (or paste a funding tx
  signature via the API),
- the W5h funding-confirm verifier accepts the funding tx and
  transitions the row to `budget_reserved`,
- and the watcher then refuses to lease the row on every tick
  because `expires_at_ms < now`.

Alternatively, you will hit it if a frontend crash forces you to
retype your input and re-Confirm: Phase 5c-lite's bridge
idempotently reuses the existing rule row and does **not** refresh
`expires_at_ms` — so the row's expiry stays rooted at the original
Confirm, not the most recent one.

### Why it's needed

Refreshing `expires_at_ms` on re-Confirm, or hard-rejecting
re-Confirm of expired rows with a typed error, is a required
follow-up slice. It is not done in Phase 5c-lite. **This is a
known design gap, not production behaviour.**

### Reviewer template SQL

Do **not** literally copy the SQL from
[`PHASE5C_LITE_MAINNET_PROOF.md`](PHASE5C_LITE_MAINNET_PROOF.md) —
the `rule_id_hex` and timestamps are specific to the original run.

For a fresh run, first inspect:

```sql
-- Replace <YOUR_RULE_ID> with the rule_id_hex the chat UI logged on
-- Confirm. It appears in the funding-card memo as
-- claw:w5h:<rule_id_hex>:<canonical_hash>, and in the daemon log
-- line "phase5c-lite finalize: confirm → W5h funding_required
-- minted, rule_id: ...".

SELECT rule_id_hex, status, expires_at_ms, created_at_ms
FROM stage2_w5h_funding_intents
WHERE rule_id_hex = '<YOUR_RULE_ID>';
```

If `status = 'budget_reserved'` AND `expires_at_ms < <NOW_MS>`,
**and only then**, run:

```sql
UPDATE stage2_w5h_funding_intents
SET expires_at_ms = <NOW_MS> + 600000,   -- 10 minutes from "now"
    updated_at_ms = <NOW_MS>
WHERE rule_id_hex = '<YOUR_RULE_ID>'
  AND status = 'budget_reserved';
-- rows_affected MUST equal 1; abort otherwise.
```

Where `<NOW_MS>` is the current unix milliseconds (Python:
`int(time.time()*1000)`).

The override changes only the two timestamp columns. **Do not
modify** `rule_id_hex`, `canonical_rule_hash_hex`, `amount_raw`,
`funding_signature`, `controlled_wallet`, `controlled_usdc_ata`,
or `status`. The W5i watcher will pick up the now-eligible row on
the next 30-second tick (look for
`TickReport { dispatched: 1, completed: 1, prechecks_failed: 0 }`
in the daemon log).

**Warning.** This workaround is documented as a known Phase 5c-lite
gap. Running it is reasonable for reproducing the documented proof.
It is **not** acceptable in a production deployment. Production-
mode would either refresh TTL on re-Confirm or hard-reject the
re-Confirm with a typed error so the user knows to type a fresh
intent.

## How to confirm I'm not overclaiming

- Every "proves" line in
  [PHASE5C_LITE_MAINNET_PROOF.md](PHASE5C_LITE_MAINNET_PROOF.md)
  has a matching "does NOT prove" paragraph.
- Every boundary in [SECURITY_BOUNDARIES.md](SECURITY_BOUNDARIES.md)
  has either a code-enforcement pointer or a "deferred" tag.
- The architecture document explicitly tags every aspiration that
  is **not** implemented in the loop today.

If you find a claim in this branch that is not backed by either
running code OR a Solscan link with parsed token-balance deltas,
that's a defect — please open an issue.

## What is NOT here (yet)

- A demo video.
- A `clawsol-authority` PDA on-chain signer (the `programs/`
  scaffolds compile, but are NOT signing the live deposit).
- A user-facing rule editor (current path is chat-only).
- A durable / multi-tenant / distributed scheduler (current
  watcher is a single-process `tokio::time::interval`).
- A single-sitting watcher-only mainnet round-trip without the
  operator SQL override. **Closing this is the next milestone.**
