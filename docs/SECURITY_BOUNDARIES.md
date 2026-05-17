# Security Boundaries

## Threat model in one paragraph

A user wants to authorise a future conditional on-chain action
without handing the executing agent their main wallet keys. The
agent should be able to execute the action when the condition
becomes true, but only within bounds the user pre-committed at
authorisation time, and only for the specific action the user
authorised.

## Boundaries this loop enforces

### B1. The user's main wallet signs only at funding time, and only one transaction

The user's Phantom signature event is bounded:

- Exactly one `signTransaction()` call per funding action (frontend).
- Exactly one `sendRawTransaction()` per funding action (frontend).
- The transaction contains a memo anchor + an SPL `TransferChecked`
  to the controlled wallet's USDC ATA, and (optionally) a
  create-idempotent ATA. No Solend, no Jupiter, no ClawSol program
  is invoked at this step.

There is no second `signTransaction()` site in the frontend; the
hackathon prototype enforces this via grep-asserted source tests.

### B2. The controlled wallet's keypair lives only in the daemon process

- Loaded from disk (env-pointed path) at daemon startup.
- Never serialised to a frontend response.
- Never accepted via an HTTP request body.
- Never logged.

The frontend has no APIs that touch keypair bytes. There is no
"import keypair" surface.

### B3. The funding verifier is a strict equality check

The verifier
(`crates/gateway/src/stage2_w5h_funding_confirm.rs`) refuses to
enter `budget_reserved` unless **all** of the following hold on the
on-chain transaction:

- Memo instruction at any index, program == Memo program
  (`MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr`), payload == exact
  `claw:w5h:<rule_id>:<canonical_hash>` for the persisted Intent.
- `postTokenBalances` minus `preTokenBalances` for the controlled
  USDC ATA equals the persisted `amount_raw`.
- Mint == USDC pin; owner == controlled wallet pubkey.
- `accountIndex → pubkey` resolved against the tx message keys.

A null `getTransaction` (RPC lag) returns `funding_pending`, **not**
`funding_invalid`. Mismatch on any axis returns `funding_invalid`
with a specific error code.

### B4. The execution lease is a single CAS

```sql
UPDATE stage2_w5h_funding_intents
SET status = 'executing', updated_at_ms = :now
WHERE rule_id_hex = :rule_id
  AND status = 'budget_reserved'
  AND expires_at_ms > :now
```

`rows_affected = 1` is the right to execute. Any racing manual
approval path and the autonomous watcher path call the same UPDATE;
they cannot both win.

### B5. The executor adapter is action-typed

The executor (`crates/gateway/src/stage2_chat_execute.rs`) does not
accept arbitrary instructions. It accepts an `ActionRequest` keyed
by adapter (`SolendDeposit`, planned `SolendWithdrawAll`, planned
`JupiterSwap`). Each adapter validates its own inputs against the
pinned demo shape **before** signing. The controlled wallet does
not sign an opaque blob.

### B6. The expiry refund leg is explicitly deferred and gated when it lands

**Automatic** expiry refund — the loop closing without operator
action when a funded condition never triggers — was deliberately
deferred in the hackathon prototype and remains deferred here.
**Operator-initiated** refund was proved once on mainnet during
the hackathon (W6 refund tx `2DybD46v…hNQCd`). The two are
distinct: a funded budget whose condition never triggers stays
`budget_reserved` until the operator runs the (gated) refund path
manually.

When the refund leg lands cleanly, it will be a separate gated
path:

- requires the daemon's `CLAW_STAGE2_REFUND_APPROVED` env to be set;
- requires a literal approval phrase in the refund request body;
- broadcasts a single `TransferChecked` from the controlled wallet
  back to the user-bound wallet's USDC ATA;
- writes the refund signature to the intent before clearing
  `budget_reserved`.

No automatic refund is claimed and the UI must not promise one.

## Boundaries this loop does NOT yet enforce

These are documented gaps. They are the next slices, not silent
weaknesses.

- **PDA-authority execution.** The controlled wallet today is a
  plain keypair under operator control. A genuinely user-delegated
  controlled wallet — where the user authorises a PDA-derived
  authority bound to the rule, and the program enforces the bound
  on-chain — is post-Hackathon work. Until it lands, the controlled
  wallet is trusted operator infrastructure.
- **Multi-tenant controlled-wallet isolation.** One controlled
  wallet serves all current users. Hardening to per-user controlled
  wallets (or PDA-derived per-rule signers) is downstream.
- **Off-chain key rotation policy.** The controlled-wallet keypair
  is loaded from a fixed env path. There is no rotation cadence and
  no HSM integration in the design as documented.
- **Memo anchor uniqueness.** A user could in principle replay an
  identical memo+amount transfer to inflate a single intent's
  budget. The verifier rejects on `funding_signature` re-use, but
  the anchor itself does not include a nonce. A nonce field on the
  Intent (and in the memo payload) is on the roadmap.
- **Refresh-TTL on re-Confirm.** As surfaced in the Phase 5c-lite
  proof, an idempotent re-Confirm of an existing rule does **not**
  refresh `expires_at_ms`. The watcher then fail-closes forever on
  the CAS gate's expiry predicate. Either refresh on re-Confirm,
  or hard-reject re-Confirm of expired rows with a typed error.
- **Frontend RPC pinning.** The browser uses a configured RPC for
  the `getLatestBlockhash` step; a malicious RPC could in theory
  serve a stale blockhash to slow the flow. End-to-end this is
  recoverable (the user just retries), but the frontend does not
  yet enforce a specific RPC provider.

## Reading source

If you want to audit the boundary claims directly:

- `crates/gateway/src/stage2_w5h_funding_confirm.rs` — funding
  verifier (B3).
- `crates/gateway/src/stage2_w5i_auto_execute.rs` — watcher + CAS
  call site (B4).
- `crates/gateway/src/stage2_chat_execute.rs` — executor signer (B2, B5).
- `crates/gateway/src/stage2_phase5c_draft.rs` — in-memory draft
  store (Phase 5c-lite gate).
- `crates/api/src/routes/stage2_w5h_intent_finalize.rs` — finalize
  route (Phase 5c-lite gate).
- `frontend/src/app/chat/page.tsx` — single `signTransaction` and
  single `sendRawTransaction` call sites (B1).
- `frontend/src/lib/stage2-funding.ts` — funding-tx builder + memo
  ix (B1 + B3).
