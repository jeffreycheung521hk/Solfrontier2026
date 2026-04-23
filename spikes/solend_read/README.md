# Solend Read-Only Spike

Throwaway investigation to validate `docs/lending_policy_vocabulary.md` Part 2
(snapshot schema) against a real mainnet lending protocol.

## Status

- **Type:** spike / research. **Disposable.**
- **Not a crate**, not wired into workspace.
- **Not a production module**, not wired into daemon.
- **Read-only only.** No signing, no tx, no state mutation.

## What this answers

- Does Part 2's `LendingSnapshot` schema shape actually fit a real protocol?
- Can Part 3A's V1 mandatory rules be answered from snapshot alone?
- Does Part 3B's three-layer gate model survive real data?

## What this is NOT

- Not Part 4 (no metrics derivation here).
- Not Part 5 (no module placement, no Rust types in production crates).
- Not Part 6 (this spike uses Solend because it has a stable, simple enough
  account model; this is not a protocol choice commitment).
- Not lending integration. Not a framework for future protocols.

## How to run

```bash
cd spikes/solend_read
python spike.py --obligation 8Dn6vWnTqpRTAgxmriP5cBFbL8nvitVUG6o8ReZ3qbqw
```

Requires Python 3 with stdlib only. No pip installs.

## Files

- `spike.py` — the script. Pure stdlib. ~300 lines.
- `spike-report.md` — the report. Evidence from a real run.
- `README.md` — this file.

## Disposal

When the spike is no longer needed, delete `spikes/solend_read/` entirely.
Nothing in the production graph depends on it.
