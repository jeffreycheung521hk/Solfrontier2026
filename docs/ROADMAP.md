# Roadmap

This branch carries the proof of **Phase 5c-lite**. The phases
below name slices, not dates. Each slice ships with a live proof
(Solscan link + parsed on-chain token-balance deltas) or a typed
test artifact.

## Vision — Why this exists

Asia institutional capital is starting to use Solana as a
settlement and custody rail, not a speculative venue. Hong Kong's
VTAP regime is licensing tokenisation and stablecoin issuance,
Japan's FSA reform is opening the path for regulated yen-settled
on-chain instruments, and tokenized treasuries (BUIDL, Ondo, and
the money-market wrappers behind them) are now a real allocation
line for treasurers who measure custody risk before yield.

The gap these treasurers hit is not governance and not custody —
both already have credible answers. Squads gives a multisig the
ability to decide **who** signs. Hardware and MPC custody protect
the keys once a signing decision is made. What sits between them
is missing: a policy-enforcement layer that inspects a transaction
and decides **what** is allowed to be signed in the first place —
catching the wrong transaction *before* it is signed, not
reconciling it after it has settled.

ClawSolana fills that layer. It is a fail-closed policy engine that
sits between a governance quorum and a custody key, built on four
properties that compose: typestate (a transaction cannot reach the
signer unless it passed policy), capability (the signer only ever
holds authority for the specific authorised action), commitment (a
cryptographic binding between what policy approved and what gets
signed), and an audit chain (a tamper-evident record an external
party can verify). For a former treasurer, this is the control he
would have demanded before delegating a payment rail.

> Squads decides who signs. ClawSolana decides what gets signed.

## Hard architectural commitments

These do not move. They are the load-bearing boundaries of the
product, promoted here from the bottom of the document so the
constraints are read before the phases.

- **Arbitrary natural language with no schema validation.** The
  LLM-assisted parser must validate every output against the typed
  Intent schema. There is no path where a loose LLM interpretation
  becomes runtime state.
- **LLM in the autonomous execution hot path.** Watcher, CAS gate,
  executor, and live-transaction broadcast remain pure Rust across
  **all** phases. This is the load-bearing boundary of the
  product, not a deferral that gets revisited later.

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
- **Typestate Constructor Hardening.** Change `pub fn
  new_unchecked` to `pub(crate)` on `ApprovedTransaction` and
  `SimulatedTransaction`; make the wrapped fields private with
  read-only accessors; add a `compile_fail` doctest demonstrating
  that external construction of these typestate markers is now
  rejected by the compiler. Deliverable: closes the Q2/Q3 gap
  where typestate enforcement was naming-convention plus
  `debug_assert!` only — an external crate could fabricate an
  "approved" transaction. After this slice the only path to an
  `ApprovedTransaction` is through the policy gate, enforced at
  compile time, not by convention.
- **Approval-Time Cryptographic Commitment.** `ApprovedTransaction`
  stores a SHA-256 digest of the canonical Solana `Message` bytes,
  computed at policy-approval time under the domain separator
  `b"clawsol-approval-tx-v1"`. `Pipeline::sign()` re-derives the
  digest from the message it is about to sign and fail-closes on
  any drift between approval-time and sign-time bytes. Deliverable:
  closes the Q10 in-memory-object-identity gap — approval was
  previously bound to an in-process object reference, not to the
  transaction content, so a mutation between approve and sign was
  undetectable. Adds a new typed error
  `WalletError::ApprovalTxDrift`; a property test asserts a 100%
  detection rate over 1000 random `Transaction` mutations.

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

## Phase 8b — Audit Log Tamper-Evidence

**Design phase, not implementation in progress.**

The audit log today is an append-only record the operator is
trusted not to rewrite. This phase removes that trust: the log
becomes a cryptographic chain that an external party can verify
without trusting the operator's storage.

- **Per-entry hash chain.** Each audit entry carries a `prev_hash`
  field and a signature over `(prev_hash, entry_bytes)`, so any
  insertion, deletion, or reordering breaks the chain at a
  detectable point. A verifier replays the chain from genesis and
  confirms every link.
- **Daily Merkle root commitment on-chain.** Once per day the
  Merkle root over that day's entries is anchored to Solana via a
  memo instruction, giving each day's log an immutable on-chain
  timestamp the operator cannot backdate.
- **Optional WORM storage adapter.** A write-once-read-many storage
  backend (S3 Object Lock or equivalent) for deployments that
  require retention guarantees at the storage layer, independent of
  the application.

Deliverable: a tamper-evident audit chain that an external verifier
can validate cryptographically — given the chain and the on-chain
anchors, the verifier needs no trust in the operator's database. A
CLI performs the chain-integrity audit and reports the first
divergence if one exists.

## Phase 9 — production scheduler

- Durable job queue (replaces `tokio::time::interval`).
- Leader election (single-writer guarantee preserved across daemon
  restarts).
- Multi-tenant isolation (per-user controlled-wallet model lands
  here too).

Deliverable: the watcher becomes operational infrastructure rather
than a demo loop.

## Phase 10 — Squads Multisig Integration

**Design phase, not implementation in progress.**

This phase places ClawSolana inside an institutional governance
flow rather than treating the controlled wallet as the sole
authority. Squads decides who signs; ClawSolana decides what gets
signed; the two compose as defense in depth.

- **Squads adapter for the executor.** Instead of broadcasting a
  single controlled-wallet signature, the executor proposes the
  action to a Squads multisig, where members co-sign within the
  ClawSolana policy boundary. A proposal that fails policy is never
  raised to the multisig.
- **Squads-aware funding verifier.** The funding verifier accepts
  proposals funded against a Squads vault address, resolving the
  vault's transfer the same way it resolves a controlled-wallet
  `TransferChecked` today.
- **Squads policy-as-code.** Multisig policies are expressed as
  `PolicyRule` input, so the governance quorum and the execution
  rule are checked at two distinct layers — quorum approval is
  necessary but not sufficient; the action must still satisfy the
  policy engine.

Deliverable: an institutional treasury controlled by a Squads
multisig executes a fail-closed Solend deposit through the
ClawSolana policy engine, demonstrated with a live mainnet proof
using a real Squads vault and a small amount.

---

---

The three phases below extend the architecture past the execution-safety
work of Phases 1–10 toward an external-verifiability boundary: the point
where an institutional auditor or governance party can check ClawSolana's
behaviour without trusting the operator. All three are **design phase, not
implementation in progress** — no code is building against them yet.

---

## Phase X — Policy Config Audit Trail

Approvals today are bound to the runtime policy in effect, but nothing
records *which* policy version produced a given verdict. An operator who
loosens a cap, restarts, and tightens it again leaves no trace that the
loosened window ever existed. This phase binds every approval to a specific,
hashed policy version and makes policy changes themselves an append-only,
operator-signed chain.

- `policy_version_hash` (32-byte digest of the canonicalised policy set)
  baked into every audit entry and every `ApprovedTransaction` commitment.
- Policy TOML changes require an operator signature over the new version
  hash plus the prior version hash — signed policy versions form their own
  append-only chain, parallel to the audit chain of Phase Y.
- Per-policy emergency-pause requires a *separate* emergency-authority
  signature, distinct from the operator key — operational separation of
  duties, so the party who can loosen a cap is not the party who can halt.
- Verdict re-derivation (Phase Z) consumes the published policy version,
  not live daemon state.

Deliverable: every approval is bound to the specific policy version hash
that produced it; the "operator silently loosens the cap, executes, then
reverts" attack surface is closed because the loosened version is itself a
signed, ordered, externally visible link in the policy chain.

Status: design phase, not implementation in progress.

---

## Phase Y — Tamper-Evident Audit Chain

The audit log is currently an ordered table the operator could in principle
rewrite. This phase converts it into a hash chain whose integrity an outside
party can verify against an on-chain anchor, so a silent rewrite becomes
detectable rather than merely discouraged.

- Each audit entry carries `prev_hash` (digest of the preceding entry) and
  an `entry_signature`, forming a per-entry hash chain.
- A daily Merkle root over the day's entries is committed on-chain as a
  Solana memo; the committing transaction signature is stored back on the
  last entry of the window.
- Optional WORM storage adapter (S3 Object Lock or equivalent) for the raw
  entry stream, so the at-rest copy is write-once independent of the chain.
- External auditor CLI walks the local chain, recomputes each `prev_hash`
  and Merkle root, and checks the root against the on-chain anchor — no
  operator cooperation required beyond read access.

Deliverable: a tamper-evident audit chain that an external verifier can
validate against an on-chain anchor without trusting the operator; any
post-hoc edit either breaks the local hash chain or fails to match the
already-committed Merkle root.

Status: design phase, not implementation in progress.

---

## Phase Z — Independent Verifier (Python / TS)

Phases X and Y make the commitments; this phase builds the small, separable
tool that *checks* them. The verifier is stateless and re-derives outcomes
from public inputs, so its correctness does not depend on the daemon being
honest — only on the published policy and the committed chain.

- Stateless re-derivation: canonical `Message` hash from the proposal,
  policy verdict from the public policy TOML version named in the entry,
  compared against the operator-claimed approval state.
- Deliberately an order of magnitude smaller than the main Rust codebase,
  and scoped to be formally specifiable on its own.
- Distributable as open source so institutional auditors and governance can
  run it independently, in an ecosystem (Python) they already trust.
- Reads only public artefacts — proposal, approval commitment, policy
  version, audit chain — and emits a verdict plus a discrepancy log.

Deliverable: an open-source verifier that re-derives each approval from
public inputs, moving the institutional trust boundary from "trust the
daemon" to "verify the commitment chain."

Status: design phase, not implementation in progress.

---

## Out of scope (deferred or hard boundary)

Two flavours of "not happening": deferrals that move to a phase
when prioritised, and hard architectural commitments that do not
move at all. The hard commitments are now stated at the top of this
document under **Hard architectural commitments**; the deferrals
remain below.

**Deferred (could become future phases):**

- Cross-chain adapters — no current phase; would need a separate
  threat model.
- A user-facing rule editor (current path is chat-only).
