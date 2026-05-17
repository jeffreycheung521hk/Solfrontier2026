# Curation Manifest — Excluded

Companion to `CURATION_INCLUDED.md`. Lists what was removed from
the `git archive HEAD` snapshot of
`staging2026/integrate-phase5c-lite @ 5c37e1a` and why.

## Phase 2 note — no deletions in the active source repo

Phase 2's sanitization (operator local paths + personal-name
comments) modified files **in the curated tree only**
(`this curated tree`). The active source
worktree and all source remotes
(`origin/testingcrypto2`, `target/Solfrontier1`,
`staging2026/main`) were **not touched**. See the "Phase 2
sanitization" section of `CURATION_INCLUDED.md` for the per-file
replacement table.

## Hard-excluded (per Phase 1 spec)

| Path | Reason |
|---|---|
| `target/` | Rust build artifacts. Not tracked in the source repo anyway. |
| `data/` | Runtime DB + logs (`claw.db`, `clawd-*.log`, etc.) and the operator's `phantom-mainnet.env` (real secrets). Not in `git archive`; mentioned for completeness. |
| `frontend/.next/` | Next.js build artifacts. |
| `frontend/node_modules/` | npm dependency tree (~hundreds of MB). Reproducible via `npm ci`. |
| `frontend/.env.local` | Real frontend env (contains the live `NEXT_PUBLIC_GATEWAY_TOKEN`, `NEXT_PUBLIC_SOLANA_RPC_URL` with Helius api-key). Gitignored at source; not in `git archive`. |
| `test-wallets/` | Real keypair JSON files (lives at `~/test-wallets/`, outside the repo). Not in `git archive`. |
| `*.db`, `*.db-wal`, `*.db-shm` | Local SQLite state. |
| `*.env` with real values | See `data/` above. |
| `share/` | Hackathon scratch — `CORE_CODE.rs`, `TYPES_AND_CONFIG.rs` (snapshots for context-window optimisation, not part of the build). |
| `spikes/` | Hackathon scratch — `solend_read/` exploration code. Not part of the build. |
| `.github/` | Workflows excluded by default per Phase 1 spec. The single file `.github/workflows/ci.yml` was inspected — it does not contain secrets, runner IDs, or internal URLs (it uses standard `ubuntu-latest` + secret-name references like `${{ secrets.OPENAI_API_KEY }}`). It is excluded by default; a reviewer who wants CI can add it back in Phase 2. **Reported, no STOP needed.** |
| `docs/redesign/` | Local scratch dir (was untracked anyway; not in `git archive`). |
| `docs/proofs/` | 21 hackathon-era proof docs (phantom HTML, slice 3G, Phase 4C E2E, Phase 5G, Phase 6I-K, etc.). All hackathon-specific; the curated tree's evidence pointer lives in `docs/PHASE5C_LITE_MAINNET_PROOF.md` + brief hackathon pointers in `docs/DEMO_EVIDENCE.md`. |
| `docs/WEEKLY_UPDATES.md` | 1099-line hackathon chronological log. Internal, references private session names. Not part of Phase 5c-lite. |
| `docs/DEMO_SCRIPT_NL_TO_DEFI.md` | Local-only demo script (was untracked anyway). |
| `docs/STAGE2_OFFICIAL_DOCS_AUDIT.md` | Local-only research notes (was untracked anyway). |
| `frontend/AGENTS.md`, `frontend/CLAUDE.md` | Internal AI-agent meta-instructions. Per Phase 1 spec, "Claude / agent memory files" are excluded. |
| `config/phantom-mainnet-solend.toml` | Real mainnet config (untracked at source — gitignored). |
| `config/w5g-smoke.toml` | Real W5g live-smoke config (untracked at source — gitignored). |

## Hackathon-historical docs excluded (default-to-exclusion for internal/stale)

| Path | Reason |
|---|---|
| `ARCHITECTURE.md` (top-level) | Hackathon-era. Replaced by `docs/ARCHITECTURE.md` (rewritten for Phase 5c-lite). |
| `ROADMAP.md` (top-level) | Hackathon-era. Replaced by `docs/ROADMAP.md`. |
| `CHANGELOG.md` | Hackathon release notes. |
| `DEBT.md` | Hackathon tech-debt tracker. |
| `DEVELOPMENT.md` | Hackathon dev setup notes. Phase 5c-lite reviewers use `docs/WINDOWS_BUILD.md` + the README's "Building and running locally" section. |
| `PITCH.md` | Hackathon pitch deck. |
| `docs/DEVNET_PROOF.md` | Superseded by mainnet proofs. |
| `docs/JUPITER_JIT_PHASE1.md` | Hackathon-era Jupiter release note. Jupiter is roadmap, not in scope. |
| `docs/MILESTONE_CLOSEOUT.md` | Hackathon W2 milestone closeout. Out of scope. |
| `docs/STAGE2_DELEGATED_CONDITIONAL_EXECUTION_SPEC.md` | Hackathon Stage 2 spec. Long; useful context but not part of this curation's reviewer path. |
| `docs/STAGE2_PYTH_ADAPTER_RESEARCH.md` | Hackathon research note. |
| `docs/STAGE2_SOLEND_APY_RESEARCH.md` | Hackathon research note. |
| `docs/TESTING.md` | Hackathon test-tier doc; the README's "Building and running locally" section covers what reviewers need. |
| `docs/TEST_COVERAGE_MAP.md` | Hackathon coverage map. |
| `docs/TEST_RESULTS.md` | Hackathon nextest-output capture, references stale `docs/artifacts/<timestamp>/` paths. |
| `docs/artifacts/` | 5 historical test-inventory snapshots (368-test, 374-test). Obsolete. |
| `docs/integration-architecture.md` | Hackathon-era spec (labels itself "Implemented — Phase 1"). |
| `docs/lending_policy_vocabulary.md` | Hackathon-era spec (~3000 lines). Not in Phase 5c-lite scope. |

## Uncertain files (none — no operator decision required)

All exclusion decisions above followed the v3 Phase 1 spec
("hard exclude" + "default-to-exclusion for data/build/history/internal
docs"). No file in the source archive falls into the
"uncertain — stop and report" bucket. `.github/workflows/ci.yml` is
the closest call; per spec it is excluded by default and the
inspection report is in the table above.

## Excluded by virtue of not being in `git archive` (already untracked)

These never made it into the snapshot:

- `docs/redesign/`
- `docs/DEMO_SCRIPT_NL_TO_DEFI.md`
- `docs/STAGE2_OFFICIAL_DOCS_AUDIT.md`
- `config/phantom-mainnet-solend.toml`
- `config/w5g-smoke.toml`
- `frontend/.env.local`
- `docs/proofs/phantom_a2_bind.html` (was `M` in worktree, but the M was on the working-tree copy, not the HEAD — `git archive HEAD` extracts the HEAD version which was the original)
- `target/`
- `data/`
- `node_modules/`
- `.next/`
