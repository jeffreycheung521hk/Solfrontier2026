# Test inventory capture

- Timestamp (UTC): 2026-04-15T08-45-59Z
- Source: `cargo test -- --list --format terse` across the 5 workspace
  packages (claw-types, claw-risk-engine, claw-state-store, claw-api,
  claw-gateway), plus their integration test binaries.
- Total tests: **368** across 30 binaries.
- Purpose: closes C4 from `docs/TEST_RESULTS.md` — provides a
  per-binary inventory so the headline number ("x passed, 0 failed") can
  be cross-checked against which tests actually exist.

## What lives here

`test-inventory.txt`
  The authoritative list. Each binary has a `## <stem> — N tests`
  heading followed by the test names emitted by `--list --format terse`.
  Ends with `# TOTAL: N tests`.

## How to regenerate

```bash
# 1. Kill any live daemon (Windows LNK1104 mitigation)
taskkill /F /IM clawd.exe >/dev/null 2>&1 || true

# 2. Build the test binaries
cargo test -p claw-types -p claw-risk-engine -p claw-state-store            -p claw-api -p claw-gateway --no-run

# 3. Re-run the per-binary enumeration (see scripts/capture-run.sh for
#    the full capture flow — this step is `--list`-only and does not
#    execute any tests).
```

A future run should update this file in place; if the number diverges
significantly from the headline figure in `docs/TEST_RESULTS.md`, that
document's `Aggregated workspace result` section needs to be refreshed
with a new capture-run.
