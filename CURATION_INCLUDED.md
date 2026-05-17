# Curation Manifest — Included

Source: `staging2026/integrate-phase5c-lite` @ `5c37e1a`
(reproduced via `git archive HEAD | tar -x -C <curated-dir>` from the
SolFrontier working clone of testingcrypto2 on branch
`integrate-phase5c-lite`).

This file lists what was kept after the hard-include / hard-exclude
pass.

## Phase 2 sanitization (applied in the curated tree only)

The active source worktree was **NOT** modified by Phase 2 — every
sanitization edit landed in this curated directory and only this
directory. Specifically:

- Two operator-local-path strings were replaced. See "Path
  replacements" below.
- Five operator personal-name comment references were neutralised.
  See "Name replacements" below.
- No logic code was touched. No test fixture string was rewritten.
  No `string assertion` was edited.
- The controlled-wallet public key `BPfDMmeMBmCbMC1rWh7hwigMBoKGBrKwXxSeUu9hhs5L`
  is preserved verbatim across all files — it is on-chain public
  evidence and identifies the controlled wallet in the proof doc.

### Path replacements

The literal Phase 1 strings were operator-local Windows paths
identifying a personal home directory + wallet folder. Both have
been replaced by placeholders. The originals are intentionally not
reproduced verbatim here — that would re-leak the operator's path
through the audit trail.

| File | Original (Phase 1 — shape) | Replacement |
|---|---|---|
| `crates/gateway/src/stage2_demo_apr_bridge.rs` (doc comment) | a forward-slash absolute path under a Windows user home pointing at a `*-dryrun.json` keypair file | `` `<CONTROLLED_WALLET_KEYPAIR_PATH>` `` |
| `crates/gateway/tests/stage2_live_controlled_wallet_solend_deposit.rs` (PowerShell example in doc comment) | a backslash absolute path under a Windows user home pointing at the same keypair file | `"C:\path\to\controlled-wallet-keypair.json"` |

### Name replacements

Per the same rule as the path replacements above, the Phase 1
literal strings (each of which contained the operator's personal
name) are intentionally not reproduced verbatim — that would
re-leak the name through the audit trail. The shape of each
before-string and the exact after-string are recorded below.

| File | Before (shape) | After |
|---|---|---|
| `crates/gateway/src/stage2_demo_apr_bridge.rs` | doc comment naming the operator + an internal Claude-memory artefact | "the operator's local fs" + "a separate operator-managed reference" |
| `crates/gateway/tests/stage2_live_controlled_wallet_solend_deposit.rs` | doc-comment heading naming the operator as the live-test approver | "Phase 2 (after the operator explicitly approves)" |
| `crates/gateway/src/integrations/jupiter_jit.rs` | comment crediting the operator for the state-machine taxonomy | "Per the operator's state list" |
| `programs/clawsol-authority/src/solend_cpi_builder.rs` | dated authorization line naming the operator | "Authorized by the operator during the May 2026 demo window" |
| `programs/clawsol-authority/tests/p5c_demo_pinned_oracle.rs` | dated test comment naming the operator | "the operator specifically authorized in-place pinning for during the May 2026 demo window" |

## Included top-level directories

| Path | Reason |
|---|---|
| `bins/` | Workspace member root (4 binaries: `clawd`, `claw`, `keygen`, `sign-pending`). Required by root `Cargo.toml [workspace] members`. |
| `crates/` | Workspace member root (11 crates: `types`, `observability`, `state-store`, `solana-core`, `wallet-engine`, `risk-engine`, `tool-system`, `agent-runtime`, `channels`, `api`, `gateway`). Required by root `Cargo.toml`. |
| `programs/` | Workspace member root (2 on-chain programs: `clawsol-intent`, `clawsol-authority`). Required by root `Cargo.toml`. **NOT** signing the live deposit in Phase 5c-lite — kept as compile-time-clean scaffolds for Phase 8. |
| `frontend/` | Next.js / TypeScript UI. Required for the demo's chat surface, draft review card, funding card. Includes `src/`, `scripts/verify-*.ts` (reviewer parity checks), `public/`, `tests/`, and the Playwright + ESLint + TS configs. |
| `bridge/` | Single file `index.html` — standalone Phantom signing bridge from the hackathon. Not part of the live demo path but small, self-contained, and useful as a Phantom-flow reference. |
| `scripts/` | `capture-run.sh` (local test capture), `gen_keypair.rs` (devnet keypair generator helper), `simulate_bridge.sh` (manual bridge sim). Inspected — no embedded secrets or local paths. |
| `config/` | Gateway config templates only — `default.toml`, `jupiter-mainnet-e2e.example.toml`, `phantom-e2e.example.toml`, and the new `example.toml`. All use placeholders for secrets. |
| `docs/` | The 5 redesign docs + the new Phase 5c-lite proof doc + Windows build notes. See "Included docs" below. |
| `.cargo/` | `config.toml` — workspace build aliases (`cargo clawd`, `cargo claw`), no secrets. |
| `.config/` | `nextest.toml` — `cargo-nextest` profile config (default + ci profiles for JUnit output). |

## Included top-level files

| Path | Reason |
|---|---|
| `Cargo.toml` | Workspace root. |
| `Cargo.lock` | Lockfile (reproducible build). |
| `rust-toolchain.toml` | Pinned Rust version. |
| `.gitignore` | Carry-over from source repo; covers `data/`, `*.db*`, `.env*`, `*.pem/key/p8`, `.claude/`, etc. |
| `LICENSE` | MIT. |
| `README.md` | **Rewritten** from scratch for Phase 5c-lite focus. |
| `.env.example` | **New.** Environment variable template for the demo daemon (placeholders only, no real secrets). |
| `CURATION_INCLUDED.md` | This file. |
| `CURATION_EXCLUDED.md` | The companion manifest. |

## Included docs

| Path | Source | Notes |
|---|---|---|
| `docs/PHASE5C_LITE_MAINNET_PROOF.md` | **New (written for this curation).** | Headline mainnet-evidence doc — parsed token deltas, exact SQL override, reviewer template. |
| `docs/README.md` (top-level `README.md` rewritten) | New | Phase 5c-lite framing, caveats list, link to proof. |
| `docs/ARCHITECTURE.md` | New (adapted from the redesign scaffold's ARCHITECTURE) | 12-step loop, boundary commitments table, points at code locations. |
| `docs/SECURITY_BOUNDARIES.md` | New (adapted from the redesign scaffold) | B1–B6 enforced + explicit gaps including the refresh-TTL-on-re-Confirm slice. |
| `docs/DEMO_EVIDENCE.md` | New (adapted from the redesign scaffold) | Short pointer to PHASE5C proof + hackathon predecessor pointers. |
| `docs/REVIEWER_QUICK_PATH.md` | New (adapted, augmented with **operator-SQL override** section) | Includes the **reviewer template SQL** (parameterised, not the literal proof values) and the warning that this is a known Phase 5c-lite gap. |
| `docs/ROADMAP.md` | New (adapted from the redesign scaffold) | Phase 5c-lite proven → Phase 5c-lite-cleanup → Phase 6 refund → Phase 7 Jupiter/Solend-withdraw → Phase 8 PDA → Phase 9 production scheduler. |
| `docs/WINDOWS_BUILD.md` | Carry-over from source repo | Required for Windows builders (OpenSSL env-var workaround). |

## Included config examples

| Path | Source | Notes |
|---|---|---|
| `config/example.toml` | **New (written for this curation).** | Minimal starting kit pointing at `default.toml` for the full schema. |
| `config/default.toml` | Carry-over | Already safe-by-default; uses `"YOUR_KEY"` / `"PASTE_BASE58_KEY_HERE"` placeholders. |
| `config/jupiter-mainnet-e2e.example.toml` | Carry-over | Already named `*.example.*`. |
| `config/phantom-e2e.example.toml` | Carry-over | Already named `*.example.*`. |
| `.env.example` | **New.** | Root-level env-var template. |
| `frontend/.env.example` | Carry-over | Already safe (NEXT_PUBLIC_GATEWAY_TOKEN empty by default). |

## Workspace members (Rust)

From root `Cargo.toml [workspace] members`, all 17 are included in
full source form:

```
crates/types
crates/observability
crates/state-store
crates/solana-core
crates/wallet-engine
crates/risk-engine
crates/tool-system
crates/agent-runtime
crates/channels
crates/api
crates/gateway
bins/clawd
bins/claw
bins/keygen
bins/sign-pending
programs/clawsol-intent
programs/clawsol-authority
```

## Frontend scripts (reviewer parity checks)

| Path | Purpose |
|---|---|
| `frontend/scripts/verify-w5h-draft-intent.ts` | Asserts the Phase 5c-lite draft DTO invariants. |
| `frontend/scripts/verify-w5h-tx-shape.ts` | Asserts the W5h funding-tx byte shape (memo at ix 0, USDC mint, decimals). |
| `frontend/scripts/verify-stage2-funding-poll.ts` | Asserts the `pollSignatureStatus` branches. |

## Reason for each category

1. **Build-required source** (workspace members + Cargo manifests +
   toolchain pin + frontend src + lockfiles) — without these the
   reviewer cannot reproduce `cargo build`, `cargo test`, or
   `npm run build`.
2. **Reviewer-quick-start docs** (README + 6 docs + WINDOWS_BUILD) —
   without these the reviewer cannot evaluate the loop's claims.
3. **Live-evidence doc** (PHASE5C_LITE_MAINNET_PROOF.md) — without
   this the reviewer cannot map source → on-chain effects.
4. **Safe config examples** (`.env.example`, `config/example.toml`,
   `*.example.toml`) — without these the reviewer cannot run the
   daemon without a copy-pasted real-config recipe.
5. **Reviewer-utility scripts** (`scripts/*.sh`, `frontend/scripts/verify-*.ts`)
   — small, secret-free, useful for cross-checking.
