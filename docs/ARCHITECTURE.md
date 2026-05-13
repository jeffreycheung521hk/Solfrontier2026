# Architecture

## Bounded Intent Execution Loop

Solfrontier 2026 implements one loop, end-to-end:

```
chat command  →  typed Intent  →  on-chain Memo anchor  →
user-signed funding  →  backend funding verification  →
budget_reserved  →  watcher  →  CAS gate  →
controlled-wallet execution  →  completed evidence
```

Each step is a module boundary. Each module owns one responsibility and
exposes typed inputs/outputs to its caller. No module reaches across a
boundary; no module synthesises behaviour the next module should own.

## Step-by-step

### 1. Chat command

The chat surface accepts user instructions and produces a typed
Intent. Two parsers, applied in order:

1. **Deterministic recogniser** (Phases 0–4). A regex filter matches
   a small set of structured commands. The hackathon prototype
   recognised:

   - English: `If Save APY > <X>%, deposit <Y> USDC`
   - Chinese: `如果 Save APY > <X>%, deposit <Y> USDC`

   If the regex matches, the message is materialised into an Intent
   immediately. No LLM call.

2. **LLM-assisted parser** (Phase 5+). For messages the regex
   doesn't match, an LLM extracts structured fields (`pool`,
   `threshold`, `amount`, `expiry`, …) from broader natural-language
   phrasing using constrained output (JSON schema or tool-use mode).
   The extracted fields are then validated against the typed Intent
   schema in Rust. Anything that fails schema validation is
   rejected, not parsed loosely. The LLM's only output that
   survives validation is the same typed Intent the regex
   produces.

**The hard boundary.** Regardless of which parser produced the
Intent, from canonical hash onward no model output influences
behaviour. The LLM does not read or write persisted state, does not
call any RPC, does not see the controlled-wallet keypair, does not
touch the watcher, the CAS gate, the executor, or any adapter.
**Phase 5 expands what the chat surface *accepts*; it does not
relax what the runtime *trusts*.**

### 2. Typed Intent + canonical hash

Each Intent carries:

- `rule_id` (16-byte unique id, hex-encoded);
- structured fields (pool, threshold, amount, expiry);
- canonical serialization → SHA-256 → `canonical_rule_hash`.

Both `rule_id` and `canonical_rule_hash` are reproducible across the
backend, the chat client, and on-chain verifiers. Anyone holding the
Intent can recompute the hash and detect tampering.

Rust ↔ TypeScript parity is a hard requirement: identical bytes in,
identical hash out. A round-trip fixture in CI gates every change to
the canonical encoder.

### 3. On-chain Memo anchor

The user-signed funding transaction includes an SPL Memo instruction
at index 0 with payload:

```
claw:w5h:<rule_id_hex>:<canonical_rule_hash_hex>
```

Memo program: `MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr`.

This anchors the rule identity on-chain alongside the funding
transfer. "This signature funded that rule" is recoverable from the
chain without backend state — a deliberate redundancy.

### 4. Phantom funding

The user signs **one** transaction, in Phantom, containing:

1. SPL Memo (the rule anchor, above);
2. (optional) Create-idempotent ATA for the controlled wallet's USDC ATA;
3. SPL `TransferChecked`: user USDC ATA → controlled USDC ATA, amount = pinned.

Exactly one Phantom signature. Exactly one `sendRawTransaction`. The
frontend has no other path to invoke `signTransaction` or `sendRaw*`.

### 5. Funding verifier

The backend funding verifier (planned `crates/funding`, Phase 2)
will read the user's funding signature via RPC `getTransaction`
(with
`maxSupportedTransactionVersion: 0`) and asserts, conjunctively:

- the memo instruction exists, program matches, payload is the
  **exact** `claw:w5h:<rule_id>:<canonical_hash>` string for the
  persisted Intent;
- `postTokenBalances` − `preTokenBalances` for the controlled USDC
  ATA equals the pinned amount in raw lamports;
- mint matches USDC, owner matches the controlled wallet pubkey;
- `accountIndex → pubkey` resolution uses the tx message keys, not
  positional guesses.

A null `getTransaction` (RPC lag) returns `funding_pending`, **not**
`funding_invalid`. Mismatch on any axis returns `funding_invalid`
with a specific error code, and the intent does **not** transition
to `budget_reserved`.

### 6. State: `budget_reserved`

A persisted intent in `budget_reserved` carries:

- the funding tx signature (proof the user transferred the budget);
- the rule identity (proof what the budget is earmarked for);
- the expiry timestamp (after which the budget should be refundable).

Only intents in `budget_reserved` are eligible for watcher pickup.

### 7. Watcher

The watcher (planned `crates/watcher`, Phase 3) will run a
`tokio::time::interval` ticker (30-second period initially). Each
tick:

- lists `budget_reserved` intents;
- filters by the pinned demo shape (the initial pinned shape is
  `0.25 USDC into Solend Main Pool USDC` — the same shape the
  hackathon proved);
- re-fetches the current condition value (Save APY for the Solend
  USDC reserve);
- if the condition is met, attempts to claim the execution lease.

The watcher is **not a production scheduler**. It is a single-process
ticker. Persistence, leader election, durable retries, and
multi-tenant isolation are explicitly out of scope.

### 8. CAS gate

`lease_execution_if_budget_reserved(rule_id, now)` is a single atomic
update:

```sql
UPDATE intents
SET status = 'executing', updated_at_ms = :now
WHERE rule_id = :rule_id
  AND status = 'budget_reserved'
  AND expires_at_ms > :now
```

`rows_affected = 1` is the right to execute. The state store enforces:

- exactly one transition `budget_reserved → executing` per `rule_id`;
- a manual approval path (operator chat command) and the autonomous
  watcher path **share the same CAS**. They cannot double-execute.

### 9. Controlled-wallet executor

The executor (planned `crates/executor`, Phase 4) will hold the
controlled-wallet keypair (loaded from disk at daemon startup,
never embedded in source, never exposed to the frontend). On a
successful lease claim, the executor:

- delegates tx construction to the adapter for the action (Solend
  deposit is the initial adapter set);
- signs with the controlled wallet;
- broadcasts via `sendRawTransaction` against the configured RPC;
- polls `getSignatureStatuses` for finalization;
- writes the resulting signature to the intent's
  `execution_signature`.

The controlled wallet is the **only** signer for the execution leg.
The user's main wallet is not touched, not delegated, not required
at execute time.

### 10. Completed evidence

A completed intent carries:

- `execution_signature` (Solscan-verifiable);
- `confirmation_slot`;
- adapter-specific deltas (USDC vault deposit; cToken collateral
  pool increase).

The frontend polls a read-only order-status endpoint and surfaces
the evidence in the chat card.

## Boundary commitments

### What this architecture says NO to

| Aspiration | Status in this scaffold | Status elsewhere |
|---|---|---|
| LLM parses arbitrary NL without schema validation | **No across all phases** — Phase 5+ adds an LLM parser, but every output must validate against the typed Intent schema or be rejected | LLM-driven product copy elsewhere in the chat surface (pre-Intent) is allowed |
| LLM in the execution hot path (watcher / CAS / executor / adapter / broadcast) | **No across all phases** — pure Rust; no model output reaches these layers | n/a — this is a hard architectural commitment, not a deferral |
| Production scheduler (durable, distributed) | **No** — single-process `tokio::time::interval` | post-Hackathon work; not yet specified |
| `clawsol-authority` PDA signer | **No** — controlled wallet is a plain keypair | designed but not implemented |
| `AuthorizationRecord` PDA-on-chain delegation | **No** — design only | post-Hackathon work |
| Multi-tenant controlled-wallet management | **No** — single operator wallet | post-Hackathon work |
| Automatic expiry refund | **No** — operator-initiated only | hackathon-deferred; operator-initiated refund was proved once on mainnet (W6) |

These are not omissions to be filled in silently. They are the
boundary of what the loop currently proves. Each future expansion
will move one line from "No" to "Yes" with a corresponding live
proof artifact.

### What this architecture commits to

| Commitment | Where enforced |
|---|---|
| User signs once, at funding time only | frontend: exactly one `signTransaction`, one `sendRawTransaction` call site. The hackathon prototype enforces this via grep-asserted source tests; the Phase 4 frontend rebuild will re-add the fixture |
| Funding has an on-chain memo anchor | funding-tx builder; verified by the funding crate (Phase 2) |
| Funding verifier requires exact memo + token-delta match | funding-crate test suite (positive + negative cases), Phase 2 |
| CAS gate shared by manual approval and autonomous watcher | executor + watcher crate test suites (Phase 3 / Phase 4) |
| Controlled-wallet keypair never leaves the daemon process | executor crate (Phase 4); no frontend signing primitives |
| LLM never reaches watcher / CAS / executor / adapter / broadcast | watcher and executor crates accept only `intent-core` types; any LLM output that has not been schema-validated cannot construct one |
| Solend is the first adapter; Jupiter is the next; the loop is adapter-agnostic | Solend adapter (Phase 4 deposit; Phase 7 withdraw); Jupiter adapter (Phase 7); subsequent adapters follow the same template |

## Hackathon evidence pointer

The hackathon prototype proved the loop end-to-end on mainnet:

- **W5h-lite** funding round-trip — chat → Phantom funding → backend
  verify → `budget_reserved`.
- **W5i** autonomous execution — watcher tick → CAS gate →
  controlled-wallet Solend deposit finalized on mainnet, with no
  human signature between funding and execution.

Hackathon repo:
[`testingcrypto2/WEEKLY_UPDATES.md`](https://github.com/jeffreycheung521hk/testingcrypto2/blob/main/docs/WEEKLY_UPDATES.md)
(frozen for judging through 2026-06-23).

The redesign rebuilds the same loop with cleaner module boundaries;
see [`ROADMAP.md`](ROADMAP.md) for the per-phase plan.
