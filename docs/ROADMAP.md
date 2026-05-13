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
each fixture matches a frontend stub. CI gates this parity.

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

Deliverable: localnet end-to-end (chat command → funding → watcher
→ Solend deposit), then a single mainnet run with full Solscan
evidence, then this scaffold's first
[`docs/DEMO_EVIDENCE.md`](DEMO_EVIDENCE.md) entry.

## Phase 5 — refund leg

- Operator-initiated refund (env-gated + approval-phrase-gated).
- Refund-tx builder (`TransferChecked` back to user-bound wallet).
- Refund integration test against an existing `budget_reserved`
  intent.

Deliverable: the leg the hackathon W5h-lite explicitly deferred.

## Phase 6 — Solend withdraw-all + Jupiter adapter (next adapter)

- Solend withdraw-all (hackathon proved this end-to-end on mainnet;
  port the processor-parity audit notes).
- Jupiter conditional swap (the path the hackathon scoped out):
  - sizing harness (already prototyped — port the artefacts);
  - pre/post checkpoint PDA;
  - SPL Token unpacking for delta enforcement;
  - instructions-sysvar adapter for sibling-instruction
    verification.

Deliverable: a second adapter exercising the same loop.

## Phase 7 — PDA authority (the boundary upgrade)

- `clawsol-authority` program (PDA signs the executor tx).
- `AuthorizationRecord` PDA (user-delegated bound on chain).
- Program-enforced rule lookup at execute time.

Deliverable: the controlled wallet stops being trusted operator
infrastructure. The execution authority is on-chain and
program-enforced.

## Phase 8 — production scheduler

- Durable job queue (replaces `tokio::time::interval`).
- Leader election (single-writer guarantee preserved across daemon
  restarts).
- Multi-tenant isolation (per-user controlled-wallet model lands
  here too).

Deliverable: the watcher becomes operational infrastructure rather
than a demo loop.

---

## Out of scope (for now)

These have been discussed and explicitly deferred:

- Arbitrary natural-language → typed Intent parsing.
- LLM-in-the-loop autonomous execution.
- Cross-chain adapters.
- A user-facing rule editor (current path is chat-only).
