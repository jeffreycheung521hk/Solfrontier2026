# Reviewer Quick Path

A five-minute guided walk. Read these in order:

1. **[README.md](../README.md)** — the loop in one paragraph, and
   the bullet list of "what this does NOT claim" (the no-overclaim
   discipline).
2. **[docs/ARCHITECTURE.md](ARCHITECTURE.md)** — the ten loop steps,
   the boundary commitments table, and the explicit "what this
   architecture says NO to" table.
3. **[docs/SECURITY_BOUNDARIES.md](SECURITY_BOUNDARIES.md)** — what
   the loop enforces (B1–B6) and what it does NOT yet enforce.
4. **[docs/DEMO_EVIDENCE.md](DEMO_EVIDENCE.md)** — hackathon
   mainnet proofs (Solscan links) and the honest qualification on
   W5i (the one operator SQL `UPDATE` that was needed).
5. **[docs/ROADMAP.md](ROADMAP.md)** — where this scaffold goes
   next.

## What I think is worth your attention

- **The boundary commitments are negative, not positive.** The
  architecture commits to what it does NOT do (no LLM in execution,
  no production scheduler, no PDA authority yet). That's the
  load-bearing part — the negatives are what makes the loop
  evaluatable.
- **The CAS gate is the only protection against double-execution.**
  Both the manual operator-approval path and the autonomous watcher
  hit the same `UPDATE … WHERE status = 'budget_reserved'`. Two
  callers, exactly one winner.
- **The Memo anchor makes the funding-to-rule binding recoverable
  from the chain without backend state.** That's a deliberate
  redundancy.
- **The hackathon W5i proof had a one-line operator footnote** (an
  `UPDATE` to extend an expired intent). The redesign needs to
  close that gap with a single-sitting end-to-end run.

## How to confirm I'm not overclaiming

- Every "proves" line in [`docs/DEMO_EVIDENCE.md`](DEMO_EVIDENCE.md)
  has a matching "does NOT prove" paragraph.
- Every boundary in
  [`docs/SECURITY_BOUNDARIES.md`](SECURITY_BOUNDARIES.md) has
  either a code-enforcement pointer or a "deferred" tag.
- The architecture document explicitly tags every aspiration that
  is **not** implemented in this scaffold.

If you find a claim in this branch that is not backed by either
running code (Phase 1+) or a Solscan link (hackathon evidence),
that's a defect — please open an issue.

## What is NOT here (yet)

- A runnable daemon (Phase 4).
- A runnable frontend (Phase 4).
- Any live mainnet tx originated from this branch (Phase 4).
- Tests beyond the canonical-hash round-trip fixture (Phase 1).
- Any keypair material. (Never. Boundary B2.)
