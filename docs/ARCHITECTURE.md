# Architecture

## Bounded Intent Execution Loop

Solfrontier 2026 implements one loop, end-to-end:

```
chat command  →  LLM draft  →  typed Intent  →  user review card  →
user Confirm (finalize)  →  on-chain Memo anchor  →
user-signed funding  →  backend funding verification  →
budget_reserved  →  watcher  →  CAS gate  →
controlled-wallet execution  →  completed evidence
```

Each step is a module boundary. Each module owns one responsibility
and exposes typed inputs/outputs to its caller. No module reaches
across a boundary; no module synthesises behaviour the next module
should own.

## Step-by-step

### 1. Chat command

The chat surface accepts user instructions and produces a typed
Intent. Two parsers, applied in order:

1. **Deterministic recogniser.** A regex filter matches a small
   set of structured commands (the hackathon's pinned 0.25 USDC
   English form + Chinese form). If the regex matches, the message
   is dispatched directly to the W5h bridge with no LLM call.

2. **LLM-assisted parser (Phase 5b+).** For messages the regex
   does not match, the LLM (`gpt-4o`) extracts structured fields
   (`pool`, `threshold_bps`, `amount_raw`, `expiry_seconds`, …) from
   broader natural-language phrasing using constrained tool-use
   output. The extracted fields are then validated against the
   typed Intent schema in Rust. Anything that fails schema
   validation is rejected; nothing "loosely parsed" leaks
   downstream.

**The hard boundary.** Regardless of which parser produced the
Intent, from canonical hash onward no model output influences
behaviour. The LLM does not read or write persisted state, does
not call any RPC, does not see the controlled-wallet keypair, does
not touch the watcher, the CAS gate, the executor, or any adapter.

### 2. Typed Intent + canonical hash

Each Intent carries `rule_id` (16-byte unique id), structured
fields, and a canonical SHA-256 hash. Both `rule_id` and
`canonical_rule_hash` are reproducible across backend and frontend.

### 3. User review card (Phase 5c-lite gate)

The LLM-extracted Intent is **NOT** persisted yet. It is wrapped in
a `DraftIntent` held in an in-memory store with a 900 s TTL, and
returned to the chat client as:

```
{ "status": "draft_intent_review_required", "draft": { … } }
```

The user reviews the typed fields, the human-readable
`review_copy`, and the `parser_source` label. They click
**Confirm** or **Reject**. **No `stage2_w5h_funding_intents` or
`stage2_watch_rules` row exists at this stage.**

### 4. Finalize

`POST /sessions/:id/stage2/w5h/intent/finalize` with
`{ draft_id, draft_hash, user_confirmed }`. The bridge:

- Looks up the draft by `draft_id` in the in-memory store.
- Asserts `draft_hash` matches the stored canonical hash byte-for-byte.
- On `user_confirmed=true`: drops the draft and persists exactly
  one `funding_intent` row and one `watch_rule` row. TTL is rooted
  at the finalize timestamp.
- On `user_confirmed=false`: drops the draft, returns
  `{ "status": "rejected" }`, no DB row is written.

### 5. On-chain Memo anchor

The user-signed funding transaction includes an SPL Memo
instruction at index 0 with payload:

```
claw:w5h:<rule_id_hex>:<canonical_rule_hash_hex>
```

Memo program: `MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr`.

This anchors the rule identity on-chain alongside the funding
transfer. "This signature funded that rule" is recoverable from the
chain without backend state.

### 6. Phantom funding (user signs once)

The user signs **one** transaction in Phantom, containing:

1. SPL Memo (the rule anchor).
2. (optional) Create-idempotent ATA for the controlled wallet's USDC ATA.
3. SPL `TransferChecked`: user USDC ATA → controlled USDC ATA,
   amount = the typed `amount_raw` the user finalized.

Exactly one Phantom signature. Exactly one `sendRawTransaction`.
The frontend has no other path to invoke `signTransaction` or
`sendRaw*`.

### 7. Funding verifier

The backend verifier reads the user's funding signature via RPC
`getTransaction(maxSupportedTransactionVersion: 0)` and asserts,
conjunctively:

- the memo instruction exists, program id matches, payload is the
  **exact** `claw:w5h:<rule_id>:<canonical_hash>` string;
- `postTokenBalances` minus `preTokenBalances` for the controlled
  USDC ATA equals the persisted `amount_raw`;
- mint matches USDC, owner matches the controlled wallet pubkey;
- `accountIndex → pubkey` resolution uses the tx message keys.

A null `getTransaction` (RPC lag) returns `funding_pending`, **not**
`funding_invalid`. Mismatch on any axis returns `funding_invalid`
with a specific error code.

### 8. State: `budget_reserved`

A persisted intent in `budget_reserved` carries the funding tx
signature, the rule identity, and the expiry timestamp (finalize +
180 s in the current pinned shape).

### 9. Watcher (W5i)

`crates/gateway/src/stage2_w5i_auto_execute.rs` runs a
`tokio::time::interval(30s)` ticker. Each tick:

- lists `budget_reserved` intents;
- filters by the pinned demo shape (band `[100_000, 1_000_000]` raw
  USDC; controlled-wallet pubkey + USDC ATA);
- re-fetches the condition value (Save APY for the Solend USDC
  reserve);
- if the condition is met, attempts to claim the execution lease.

The watcher is **not a production scheduler.** It is a
single-process ticker.

### 10. CAS gate

`lease_execution_if_budget_reserved(rule_id, now)` is a single
atomic update:

```sql
UPDATE stage2_w5h_funding_intents
SET status = 'executing', updated_at_ms = :now
WHERE rule_id_hex = :rule_id
  AND status = 'budget_reserved'
  AND expires_at_ms > :now
```

`rows_affected = 1` is the right to execute. The CAS gate enforces
single-execution: a manual approval path and the autonomous
watcher path share the same CAS update; they cannot both win.

**The expiry predicate is load-bearing.** If a re-Confirm of an
existing rule does not refresh `expires_at_ms`, the watcher will
fail-closed on every tick until an operator extends it. That is
exactly what the Phase 5c-lite mainnet proof had to do; see
[PHASE5C_LITE_MAINNET_PROOF.md](PHASE5C_LITE_MAINNET_PROOF.md).

### 11. Controlled-wallet executor

The executor (`crates/gateway/src/stage2_chat_execute.rs`) holds
the controlled-wallet keypair (loaded from disk at daemon startup,
never embedded in source, never exposed to the frontend). On a
successful lease claim, the executor:

- delegates tx construction to the adapter for the action (Solend
  deposit in the current scaffold);
- signs with the controlled wallet;
- broadcasts via `sendRawTransaction` against the configured RPC;
- polls `getSignatureStatuses` for finalization;
- writes the resulting signature to the intent's
  `execution_signature` field.

The controlled wallet is the **only** signer for the execution leg.
The user's main wallet is not touched, not delegated, not required
at execute time.

### 12. Completed evidence

A completed intent carries `execution_signature`,
`confirmation_slot`, and adapter-specific deltas. The frontend
polls a read-only order-status endpoint and surfaces the evidence in
the chat card.

## Boundary commitments

### What this architecture says NO to

| Aspiration | Status | Notes |
|---|---|---|
| LLM parses arbitrary NL without schema validation | **No across all phases** | Phase 5b+ adds an LLM parser, but every output validates against the typed Intent schema or is rejected. |
| LLM in the execution hot path (watcher / CAS / executor / adapter / broadcast) | **No across all phases** | Pure Rust; no model output reaches these layers. |
| Production scheduler (durable, distributed) | No | Single-process `tokio::time::interval`. |
| `clawsol-authority` PDA signer | No | Controlled wallet is a plain keypair under operator control. |
| `AuthorizationRecord` PDA-on-chain delegation | No | Design only. |
| Multi-tenant controlled-wallet isolation | No | One controlled wallet serves all current users. |
| Automatic expiry refund | No | Operator-initiated refund only (the hackathon W6 proof). |
| Single-sitting watcher-only round-trip without operator SQL | **Not yet proven on mainnet** | The Phase 5c-lite proof needed an operator SQL override; see Caveat 1 in [PHASE5C_LITE_MAINNET_PROOF.md](PHASE5C_LITE_MAINNET_PROOF.md). |

### What this architecture commits to

| Commitment | Where enforced |
|---|---|
| User signs once, at funding time only | frontend: exactly one `signTransaction`, one `sendRawTransaction` call site (CI grep-asserted in the hackathon prototype; the Phase 4 frontend rebuild re-adds the fixture). |
| Funding has an on-chain memo anchor | funding-tx builder; verified by `crates/gateway/src/stage2_w5h_funding_confirm.rs`. |
| Funding verifier requires exact memo + token-delta match | `crates/gateway` test suite (39 positive + negative tests). |
| CAS gate shared by autonomous watcher and any manual path | `crates/gateway` test suite; the lease SQL is the same UPDATE in both call sites. |
| Controlled-wallet keypair never leaves the daemon process | no frontend signing primitives; the keypair is loaded from an env-pointed disk path. |
| LLM never reaches watcher / CAS / executor / adapter / broadcast | `crates/gateway` watcher + executor accept only `intent-core` types; any LLM output that has not been schema-validated cannot construct one. |
| Solend is the first adapter; Jupiter is the next; the loop is adapter-agnostic | `crates/gateway` Solend deposit adapter (live); `crates/gateway` Jupiter integration (legacy, not used by Phase 5c-lite). |

## Live proof pointer

The Phase 5c-lite end-to-end mainnet proof at 0.5 USDC ran on
2026-05-16. See [`PHASE5C_LITE_MAINNET_PROOF.md`](PHASE5C_LITE_MAINNET_PROOF.md)
for the exact transactions and parsed token-balance deltas.
