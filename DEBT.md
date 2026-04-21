# ClawSolana — Technical Debt & Migration Ledger

**Updated:** 2026-04-17 | **Companion:** [ARCHITECTURE.md](ARCHITECTURE.md)

---

## What this file is

Known gaps between the codebase as it exists today and the target shape described in [ARCHITECTURE.md §7](ARCHITECTURE.md#7-module-placement-principles).

**Not a TODO list.** Every entry must include:

- **Why not fixed:** the reason it is still here
- **Trigger to fix:** the condition that makes deferral no longer acceptable

Items without a trigger are wishes, not debt, and do not belong here.

---

## 1. Debt Items

### D-1: Tool placement asymmetry (Orca vs Jupiter)

- **Current:** Orca swap lives in `crates/tool-system/src/tools/protocols/orca.rs`. Jupiter swap lives in `crates/gateway/src/tools/jupiter_swap.rs` (1294 lines). Two DeFi integrations, two different crates, no documented reason in-repo.
- **Target:** Both conform to the builders/executors split. Jupiter's intent-first + JIT is the reference shape for executors. Orca should migrate toward the same pattern when it next sees substantive work.
- **Why not fixed:** Jupiter's pattern is newer. Orca works, is devnet-confirmed, and migration is not blocking any current milestone.
- **Trigger to fix:** Before adding the next DeFi integration, OR before any reorg of `crates/gateway/src/tools/`.
- **Migration precondition:** Before actually splitting Jupiter into builder + executor, verify the candidate builder passes the [ARCHITECTURE.md §7.1](ARCHITECTURE.md#71-builders-vs-executors) test — it must produce a valid `VersionedTransaction` from pure inputs, with no access to `approval_store`, `park_store`, or pending state. If the builder cannot be made pure, do not split. A dishonest builder/executor boundary is worse than the current documented asymmetry.

### D-2: `crates/gateway/src/daemon.rs` scope

- **Current:** 2341 lines. Owns startup wiring, signature completion dispatch, rebroadcast loop, startup recovery, and Jupiter-specific wiring.
- **Target:** `daemon.rs` retains wiring + main loop only. Background loops, recovery, and integration-specific glue live in dedicated modules under `crates/gateway/src/runtime/` (or equivalent).
- **Why not fixed:** No concrete pain today. A big-bang split during maintenance mode is pure risk.
- **Trigger to fix:** Before Phase 2 adds a new background loop or signature path.
- **Approach:** Attrition, not refactor. New work lands in new files; `daemon.rs` shrinks as existing pieces migrate out when touched for other reasons. Do not add new responsibilities to `daemon.rs` in the meantime.

### D-3: Test file names encode prompt history

- **Current:** 27 integration tests under `crates/gateway/tests/` named with prompt-session prefixes: `p1_*`, `p2_*`, `p3_*`, `p5_*`, `c1_*`, `n6_*` through `n28_*`. The prefixes mean something to the people who ran those prompts; they mean nothing to a new reader.
- **Target:** Semantic names organized into subfolders by domain (see Migration Posture table below).
- **Why not fixed:** Internal contributors know the convention. External reviewers do not.
- **Trigger to fix:** Before the next external review round (OpenAI / Gemini / Frontier judges), OR when any test file in the area is edited for Phase 2 work.

### D-4: `docs/` mixes living docs with milestone snapshots

- **Current:** 7 files in `docs/` mix living documents (`integration-architecture.md`, `TESTING.md`, `TEST_COVERAGE_MAP.md`) with frozen snapshots (`MILESTONE_CLOSEOUT.md`, `JUPITER_JIT_PHASE1.md`, `DEVNET_PROOF.md`, `TEST_RESULTS.md`). A reader cannot tell at a glance which are authoritative.
- **Target:** `docs/reference/` (living), `docs/milestones/` (frozen milestone snapshots), `docs/proofs/` (devnet evidence and test results).
- **Why not fixed:** No reader has been observably misled yet. Convenience debt, not correctness debt.
- **Trigger to fix:** Before the next external review, OR when `docs/` accumulates one more milestone snapshot.

---

## 2. Reorg Preconditions

These unlock only when a reorg is actually happening. Do not apply for day-to-day work.

### PC-1: Path reference audit

Before moving any file, grep for path references in:

- `.github/workflows/`
- `Cargo.toml` (workspace members, exclude paths)
- `justfile` / scripts / CI invocations
- `.gitignore`
- Cross-document links in `docs/`
- Playwright / testing tooling configs

Typical audit time: 10 minutes. Catches ~80% of the "CI went red after git mv" surprises before they happen.

### PC-2: One PR, one commit, one theme

Any reorg touching `crates/gateway/src/` module paths must:

- Complete in a single PR
- Not mix file moves with logic changes
- Keep `use crate::…` fixups atomic with the moves

Half-finished reorg state colliding with a feature branch is merge hell. A full reorg as a single reviewable diff is cheap; a partial one is expensive forever.

**Scope:** This rule applies to *large* reorgs that cascade through `use crate::…` paths. Large reorgs should also bundle with a feature PR when possible — standalone no-payload reorg PRs carry review cost without matching benefit. *Small housekeeping* (`docs/` restructure, test filename renames, non-cascading `git mv`) may ship as independent PRs and often should: they are cheap, reviewer-visible, and worth doing on their own.

### PC-3: Reorg order

1. `docs/` — lowest risk, no imports depend on paths
2. `crates/gateway/tests/` — folder-level split only, filename renames deferrable
3. `crates/gateway/src/` — high blast radius, every `use crate::…` moves
4. Tool classification and placement — last, because it is concept-level and should not lead

Follow this order even when it feels backwards from "fix the worst thing first." Lowest-risk moves absorb reviewer attention cheaply and surface path-audit gaps before they hit the hard cases.

---

## 3. Migration Posture

Per-area handling when a trigger fires:

| Area | Action | Notes |
|------|--------|-------|
| `docs/` | Reorg when trigger fires | Pure `git mv`; update cross-links |
| `crates/gateway/tests/` | Folder split when trigger fires | Filename renames can defer to a second pass |
| `crates/gateway/src/` flat modules | Touch-migrate only | Relocate when being edited for other reasons |
| `daemon.rs` | Attrition only | No standalone refactor; new loops go in new files |
| Orca tool | Migrate toward executor shape | Align with Jupiter when next DeFi integration lands |
| Jupiter | Reference shape | Do not migrate backwards for consistency |
| `claw-types`, `claw-state-store`, `claw-observability` | Do not touch | Crate boundaries are correct |
| Typestate signing pipeline | Do not touch | Load-bearing compile-time invariant |
| `AppState` (api) / traits (gateway) split | Do not touch | Correct dependency inversion |

---

## 4. How to Use This File

- **Adding a new item:** include current state, target, why-not-fixed, and trigger. No trigger → it is a wish, not debt.
- **Closing an item:** remove the entry. Do not leave `✅ fixed` markers — git history is authoritative.
- **Periodic review:** check whether any trigger has fired. If yes, the item stops being deferrable and should be scheduled.
- **Disagreement:** argue it down in a PR. The principle is that all deferred work is explicit and challengeable.
