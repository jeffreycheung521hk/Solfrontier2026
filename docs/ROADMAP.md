# Roadmap

This branch carries the proof of **Phase 5c-lite**. The phases
below name slices, not dates. Each slice ships with a live proof
(Solscan link + parsed on-chain token-balance deltas) or a typed
test artifact.

## Phase 5c-lite — proven on mainnet 2026-05-16 ✓

The current shipped slice. LLM-drafted intent at 0.5 USDC →
user-finalization gate (zero DB writes pre-Confirm) → on-chain
memo-anchored funding → backend token-delta verification → W5i
watcher → controlled-wallet Solend deposit.

Headline:

- [`docs/PHASE5C_LITE_MAINNET_PROOF.md`](PHASE5C_LITE_MAINNET_PROOF.md)
- Funding tx [`oF5XWWGa…Je9`](https://solscan.io/tx/oF5XWWGaU7XeHc7rAfYHjekhW2cLM7zxKUReEWPh6U38SmRnDw9FV98xWLs9RNWwyWvXdQe7bJraUhcNquK8Je9)
- Execute tx [`2jynvLkV…UVnJ`](https://solscan.io/tx/2jynvLkVoD9TQfFH3nvMacNQZoJRpK9146TAonds1LFeBsjkBvzn2jFHsgFZASCSR5LRJGQqNgcLMQN2zSLHUVnJ)

**Known gaps that block production claims** (see
[REVIEWER_QUICK_PATH.md](REVIEWER_QUICK_PATH.md)):

- Operator SQL override was required.
- `gpt-4o-mini` rejects the spec phrasing; only `gpt-4o` was proven.
- UI copy bug on the funding card.
- Re-Confirm of an expired row is silently undefined behaviour.

## Phase 5c-lite-cleanup — close the operator-SQL gap

The next slice. Smallest meaningful diff that lets the same
end-to-end loop run **without** the operator SQL override:

- **Refresh `expires_at_ms` on re-Confirm.** When the bridge
  matches an existing `funding_required` row on canonical-hash,
  bump `expires_at_ms` to `finalize_now + 180_000`. Alternatively:
  hard-reject re-Confirm of expired rows with a typed
  `409 draft_replaces_expired_intent` and require the user to
  retype.
- **Refactor the funding-card amount banner** to template against
  the typed amount (fixes the "0.25 USDC" copy bug).
- **Tune the LLM extractor system prompt** so `gpt-4o-mini`
  accepts the spec phrasing reliably, OR default the demo daemon
  to `CLAW_CHAT_MODEL=gpt-4o` and document the cost difference.
- **Add a backend test** for the re-Confirm-after-expiry path so
  this gap cannot regress silently.

Deliverable: a second mainnet run reproducing the loop **without**
an operator SQL UPDATE between Confirm and watcher dispatch. New
entry in [`PHASE5C_LITE_MAINNET_PROOF.md`](PHASE5C_LITE_MAINNET_PROOF.md)
or a follow-up proof doc; old entries preserved.

## Phase 6 — refund leg

- Operator-initiated refund (env-gated + approval-phrase-gated).
- Refund-tx builder (`TransferChecked` back to user-bound wallet).
- Refund integration test against an existing `budget_reserved`
  intent.

Deliverable: the leg the hackathon W5h-lite explicitly deferred,
reimplemented with the cleaner boundary. (Operator-initiated
refund was proved once on mainnet during the hackathon — W6 tx
`2DybD46v…hNQCd` — but is rewritten from scratch here on top of
the new module boundaries.)

## Phase 7 — Solend withdraw-all + Jupiter adapter (second adapter)

- **Solend withdraw-all.** The hackathon proved this end-to-end on
  mainnet (Phase 6I-K, tx `3tMpTSEn…EZPJZ`); port the
  processor-parity audit notes (Phase 6I-H/I/J/K) so the Solend
  withdraw layout is derived from `processor.rs`, not the SDK
  helpers.
- **Jupiter conditional swap** as the second adapter. **Not part
  of any current proof.**
- **Generalise the adapter pattern.** The loop is adapter-agnostic
  by design — every adapter exposes a typed action, validates it
  against a pinned shape, and hands a pre-built unsigned tx to the
  executor. The controlled wallet signs and broadcasts via the
  same single code path. Adding a new adapter does not modify the
  watcher, the CAS gate, or the executor's signing path.

Deliverable: a second adapter exercising the same loop, plus a
short "how to add an adapter" guide in the docs.

## Phase 8 — PDA authority (the boundary upgrade)

- `clawsol-authority` program (PDA signs the executor tx). The
  scaffold lives in `programs/clawsol-authority/` but is **not**
  signing the live deposit in Phase 5c-lite.
- `AuthorizationRecord` PDA (user-delegated bound on chain).
- Program-enforced rule lookup at execute time.

Deliverable: the controlled wallet stops being trusted operator
infrastructure. The execution authority is on-chain and
program-enforced.

## Phase 9 — production scheduler

- Durable job queue (replaces `tokio::time::interval`).
- Leader election (single-writer guarantee preserved across daemon
  restarts).
- Multi-tenant isolation (per-user controlled-wallet model lands
  here too).

Deliverable: the watcher becomes operational infrastructure rather
than a demo loop.

---

## Out of scope (deferred or hard boundary)

Two flavours of "not happening": deferrals that move to a phase
when prioritised, and hard architectural commitments that do not
move at all.

**Deferred (could become future phases):**

- Cross-chain adapters — no current phase; would need a separate
  threat model.
- A user-facing rule editor (current path is chat-only).

**Hard architectural commitments (NOT deferrals):**

- **Arbitrary natural language with no schema validation.** The
  LLM-assisted parser must validate every output against the typed
  Intent schema. There is no path where a loose LLM interpretation
  becomes runtime state.
- **LLM in the autonomous execution hot path.** Watcher, CAS gate,
  executor, and live-transaction broadcast remain pure Rust across
  **all** phases. This is the load-bearing boundary of the
  product, not a deferral that gets revisited later.
