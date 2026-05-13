# Roadmap

This scaffold is **Phase 0**. The phases below name slices, not dates.
Each slice ships with a live proof (Solscan link + on-chain delta) or
a typed test artifact.

## Phase 0 — scaffold (this branch, current)

- [x] Workspace + crate layout
- [x] Architecture spec
- [x] Security-boundaries spec
- [x] Demo-evidence pointer (hackathon proofs)
- [x] Reviewer quick path
- [x] Empty `lib.rs` / `main.rs` with module-level docs only
- [x] Initial `Cargo.toml` declarations (no behaviour yet)

No live tx is expected from Phase 0. The deliverable is a
human-readable architecture and an empty skeleton.

## Phase 1 — intent-core

- Typed `Intent` enum (start with `SolendDeposit`).
- Canonical serialization → SHA-256 hash.
- Round-trip parity fixture (Rust ↔ TypeScript identical bytes).

Deliverable: `crates/intent-core` compiles; the canonical hash for
each fixture matches a frontend stub. A CI fixture (to be added
alongside the frontend stub) gates this parity.

## Phase 2 — funding

- Memo instruction builder (frontend + backend share one source of
  truth).
- Funding-tx verifier: `getTransaction` → memo exact + token delta
  exact.
- Negative tests: wrong memo, wrong amount, wrong mint, wrong ATA,
  RPC lag.

Deliverable: the hackathon W5h-lite memo+delta verification path,
reimplemented with the cleaner boundary.

## Phase 3 — watcher

- `tokio::time::interval` ticker.
- Eligible-intent scanner (filtered by pinned demo shape).
- Condition re-fetch (Solend Save APY).
- CAS gate (`lease_execution_if_budget_reserved`).
- Race-safety unit tests (one rule, both paths, single winner).

Deliverable: the hackathon W5i watcher path, with the CAS gate
proven in tests.

## Phase 4 — executor + Solend adapter (first adapter)

- Controlled-wallet keypair load (env path; never embedded).
- `SolendDeposit` action: tx build + sign + broadcast + poll.
- Pinned demo shape only (0.25 USDC into Solend Main Pool USDC).

Deliverable: localnet end-to-end (deterministic chat command →
funding → watcher → Solend deposit), then a **single-sitting**
mainnet run with full Solscan evidence — explicitly **without** the
operator-SQL-override the hackathon W5i smoke required (see
[`DEMO_EVIDENCE.md`](DEMO_EVIDENCE.md) for what that override was).
This becomes this scaffold's first
[`docs/DEMO_EVIDENCE.md`](DEMO_EVIDENCE.md) entry.

## Phase 5 — LLM-assisted intent parsing

For chat messages the deterministic recogniser doesn't match, an
LLM extracts structured fields (`pool`, `threshold`, `amount`,
`expiry`, …) from broader natural-language phrasing. The extracted
fields are validated against the typed Intent schema; validation
failure rejects the message rather than parsing it loosely.

- Constrained LLM output (JSON-schema response_format or tool-use
  mode), so the parser cannot return free-form text that bypasses
  the typed Intent.
- A strict schema validator in Rust owns the truth: any field
  missing, wrong type, out-of-range, or unknown → rejected with a
  typed error.
- Identical downstream pipeline: the validated Intent goes through
  the same canonical hash + memo + funding + CAS + executor path
  as a regex-produced Intent. No new code path bypasses any
  verifier.
- LLM provider is env-gated (no silent fallback). Missing key →
  the LLM-assisted parser disables itself and the deterministic
  recogniser remains the only path. The daemon still runs.
- **Hard boundary preserved.** The LLM does not read or write any
  persisted state, does not call any RPC, does not see the
  controlled-wallet keypair, does not touch the watcher, the CAS
  gate, the executor, or any adapter. It sits exclusively at the
  chat → Intent boundary.

Deliverable: an extended chat surface that accepts broader
phrasings (e.g. `"park 1 USDC in Solend whenever yield clears
2 percent"`) and produces the same typed Intent + canonical hash
as the deterministic equivalent — verified by a fixture suite that
round-trips representative phrasings to a known-good canonical
hash.

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
- **Jupiter conditional swap** as the second adapter:
  - sizing harness (already prototyped during the hackathon —
    port the artefacts);
  - pre/post checkpoint PDA for delta enforcement;
  - SPL Token unpacking;
  - instructions-sysvar adapter for sibling-instruction
    verification.
- **Generalise the adapter pattern.** The loop is adapter-agnostic
  by design — every adapter exposes a typed action, validates it
  against a pinned shape, and hands a pre-built unsigned tx to the
  executor. The controlled wallet signs and broadcasts via the
  same single code path. Adding a new adapter does not modify the
  watcher, the CAS gate, or the executor's signing path.

Deliverable: a second adapter exercising the same loop, plus a
short "how to add an adapter" guide in the docs. The product
shows it generalises beyond Solend.

## Phase 8 — PDA authority (the boundary upgrade)

- `clawsol-authority` program (PDA signs the executor tx).
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

**Deferred (moved into a phase above):**

- Cross-chain adapters — no current phase; would need a separate
  threat model.
- A user-facing rule editor (current path is chat-only) — could
  follow Phase 5 if needed, not yet scheduled.

**Hard architectural commitments (NOT deferrals):**

- **Arbitrary natural language with no schema validation.** Phase 5
  adds an LLM parser, but every parsed output must validate
  against the typed Intent schema. There is no path where a loose
  LLM interpretation becomes runtime state.
- **LLM in the autonomous execution hot path.** Watcher, CAS gate,
  executor, and live-transaction broadcast remain pure Rust across
  **all** phases. This is the load-bearing boundary of the
  product, not a deferral that gets revisited later.
