# Test results snapshot

Most-recent known state of the test suite. This document records what was
observed in a local run; it is **not** produced by CI.

---

## Sources of truth for this document

| Claim category | How it was verified | Trust level |
|---|---|---|
| Test files exist | Direct filesystem inventory (`ls crates/gateway/tests`, `ls frontend/tests/e2e`) | **Fact** |
| Rust test file counts per file | `grep -c '^#\[test\]\|^#\[tokio::test\]'` | **Fact** |
| Rust aggregated pass count (366) | Reported by the local `cargo test -p claw-types -p claw-risk-engine -p claw-state-store -p claw-api -p claw-gateway` run during the Phase 2 working session | **Observed, not re-verified** since no run log was captured to disk |
| Playwright pass count (10/10) | Reported by `npx playwright test` during the Phase 3 working session | **Observed, not re-verified** — `frontend/test-results/` is empty at rest because Playwright only retains artifacts on failure, and no JSON/HTML reporter was enabled |
| Devnet E2E signatures | Captured from test stdout in the Phase 3 session | Verifiable on-chain — see [`DEVNET_PROOF.md`](./DEVNET_PROOF.md) |

> **Artifact wiring is in place** as of the docs refresh:
> - `cargo nextest run --profile ci` → `target/nextest/ci/junit.xml`
> - `npx playwright test` → `frontend/playwright-report/` (HTML + JSON)
> - `bash scripts/capture-run.sh` → `docs/artifacts/<timestamp>/`
>
> A pinned inventory snapshot has been committed at
> [`docs/artifacts/2026-04-15T08-45-59Z/`](./artifacts/2026-04-15T08-45-59Z/)
> closing C4. Future full capture-runs (`rust-nextest.log`,
> `rust-junit.xml`, `playwright-report/`) are run-specific and reviewers
> can force-add additional timestamp dirs if they want to pin a
> particular session's outputs. Numbers in this document are
> cross-checkable against the committed inventory.

---

## Phase 1 — Core correctness

**Scope**: 15 spec cases covering policy evaluation, workflow state machine,
audit invariants, propose-transfer HTTP-layer, and external-wallet signature
verification.

**New test file added in this phase**:
[`crates/gateway/tests/p1_core_correctness.rs`](../crates/gateway/tests/p1_core_correctness.rs)
— 4 tests filling the 4 gaps (cases 2, 10, 12, 13).

**Supporting refactor** (non-behavioural):
[`crates/gateway/src/approval_audit.rs`](../crates/gateway/src/approval_audit.rs) —
extracts the audit-emission logic from `GatewayApprovalHandler::decide_inner`
into a pub function so case 10 can assert the invariant without spinning
up the full daemon.

**Last observed result**: `cargo test -p claw-gateway --test
p1_core_correctness` → **4 passed, 0 failed**.

**Aggregated workspace result at end of Phase 1**:
```
cargo test -p claw-types -p claw-risk-engine -p claw-state-store \
           -p claw-api -p claw-gateway
# → 353 passed, 0 failed (before Phase 2 added 13 tests)
```

---

## Phase 2 — Operator / API black-box

**Scope**: 10 API contract cases — route-level HTTP assertions on status
codes, response bodies, auth gating, pagination, and dashboard composite
correctness.

**New test file added in this phase**:
[`crates/gateway/tests/p2_api_blackbox.rs`](../crates/gateway/tests/p2_api_blackbox.rs)
— 13 tests (some cases split into `a`/`b` sub-tests; case 9 already covered
in Phase 1 so not re-asserted).

**Last observed result**: `cargo test -p claw-gateway --test
p2_api_blackbox` → **13 passed, 0 failed**.

**Aggregated workspace result at end of Phase 2**:
```
cargo test -p claw-types -p claw-risk-engine -p claw-state-store \
           -p claw-api -p claw-gateway
# → 366 passed, 0 failed
```

**Post-C3 inventory** (adds 2 approve-route / 401 tests in
`p2_api_blackbox.rs`): **368 tests across 30 binaries** as enumerated in
[`docs/artifacts/2026-04-15T08-45-59Z/test-inventory.txt`](./artifacts/2026-04-15T08-45-59Z/test-inventory.txt).

**Post-Phase-4 inventory** (adds proptest fuzzer + concurrent quorum
tests — see the new "Phase 4" section below): **374 tests across 32
binaries** as enumerated in
[`docs/artifacts/2026-04-15T14-32-51Z/test-inventory.txt`](./artifacts/2026-04-15T14-32-51Z/test-inventory.txt).

No production code was changed in this phase — the only fix-ups were to
three test imports (`c1_rate_limit_runtime.rs`, `n10b_bridge_simulation.rs`,
`n23_operator_authz_runtime.rs`) after earlier phases extended `AppState`
with new fields.

---

## Phase 3 — Frontend E2E (Playwright)

**Scope**: 8 spec cases covering showcase mode rendering, live mode
hydration, dashboard correctness, proposal degradation, approval chain view,
lease countdown, audit page, and policy page.

**Files added**:
- [`frontend/playwright.config.ts`](../frontend/playwright.config.ts)
- [`frontend/tests/e2e/global-setup.ts`](../frontend/tests/e2e/global-setup.ts)
- [`frontend/tests/e2e/global-teardown.ts`](../frontend/tests/e2e/global-teardown.ts)
- [`frontend/tests/e2e/dashboard.showcase.spec.ts`](../frontend/tests/e2e/dashboard.showcase.spec.ts) — 3 tests
- [`frontend/tests/e2e/dashboard.live.spec.ts`](../frontend/tests/e2e/dashboard.live.spec.ts) — 7 tests

**Ancillary product-code changes** (kept minimal, test-driven):
- [`frontend/next.config.ts`](../frontend/next.config.ts): honour
  `CLAW_E2E_DIST_DIR` env var so two Next instances can coexist in the
  same source tree without fighting over `.next/`.
- [`frontend/src/app/page.tsx`](../frontend/src/app/page.tsx): `StatCard`
  gains a `data-testid` so the Phase 3 tests can target the correct card
  unambiguously. Visual output unchanged.

**Last observed result** (three consecutive runs during the Phase 3
session):

| Run | Command | Result | Duration |
|---|---|---|---|
| Targeted (the 3 remaining failures) | `npx playwright test dashboard.live.spec.ts --project=live -g "case[456]" --trace=off` | **3/3 pass** | 4.7 min (first build) |
| Stability × 3 | `... --repeat-each=3` | **9/9 pass** | 24.2 s (cached build) |
| Full suite | `npx playwright test` | **10/10 pass** | 14.3 s |

**Durable artifact**: none. `frontend/test-results/` is empty at rest;
`frontend/playwright-report/` does not exist. The numbers above are from
stdout captured in the working session. To reproduce, run the commands
above — the build cache under `.next-e2e-3000/` and `.next-e2e-3001/` is
already warm.

---

## Phase 4 — Property-based + concurrent robustness

**Scope**: 6 tests in 2 new integration files, covering the two test
categories flagged as genuinely new signal (proptest fuzzing for Policy
Engine panic/ordering invariants, concurrent race for quorum dedup).

**New test files added in this phase**:
- [`crates/risk-engine/tests/p4_policy_proptest.rs`](../crates/risk-engine/tests/p4_policy_proptest.rs)
  — 3 `proptest!` blocks (256 + 128 + 128 cases each):
  1. `evaluate_never_panics` — random rule sets × random proposals, assert
     `PolicySet::evaluate` returns for every input
  2. `first_match_wins` — `matched_rule_index = Some(k)` implies no rule at
     index `< k` matches (ordering invariant)
  3. `human_approval_verdict_has_legitimate_origin` — `RequiresHumanApproval`
     verdict must come from a `RequireHumanApproval` action or the
     fail-closed default
- [`crates/gateway/tests/p3_quorum_concurrent.rs`](../crates/gateway/tests/p3_quorum_concurrent.rs)
  — 3 `#[tokio::test(flavor = "multi_thread")]` tests hammering
  `ApprovalStore::decide`:
  1. 3-of-5 quorum race → exactly 3 progress, 2 `AlreadyDecided`, state
     `Approved`, stage records exactly 3 distinct operators
  2. Same-operator-id spam (5×) → exactly 1 accepted, 4 `DuplicateOperator`,
     workflow stays `Pending`
  3. Concurrent approve + reject × 16 iterations → final state is never
     `Approved` (only `Rejected` or `Pending`)

**Production-code change driven by this phase** (non-trivial):
[`crates/risk-engine/src/policy.rs`](../crates/risk-engine/src/policy.rs)
`transfer_amount_lamports` was summing `Vec<u64>` with `.sum()` which
panics in debug builds on u64 overflow. Proptest found this on its first
serious sweep. Fixed to `fold(0u64, saturating_add)` — semantically
correct because an overflowing total still exceeds any sane
`AmountExceedsLamports` threshold, so `fire-when-high` rules still fire.

**Last observed result**: all 6 tests pass locally.

**Aggregated workspace result at end of Phase 4**:
```
cargo test -p claw-types -p claw-risk-engine -p claw-state-store \
           -p claw-api -p claw-gateway
# → 374 passed, 0 failed
```

**Out of scope for Phase 4** (documented as current non-closures):
- Hurl/Newman HTTP scripts — Rust `p2_api_blackbox.rs` already covers
  the same contracts at the route layer; a `.hurl` lane would duplicate
  without new signal. Flagged as deliberate non-work.
- Wallet-adapter mocking in Playwright, visual-regression screenshots,
  Toxiproxy/RPC chaos, simulate-OK-but-broadcast-fail end-to-end — all
  listed as C6 in the risk table below.

---

## Devnet integration

Three test files under `crates/solana-core/tests/`:

| File | Needs funds? | Last observed |
|---|---|---|
| `devnet_orca_check.rs` | ❌ | **passed** — Orca program + pool + config accounts exist on devnet |
| `devnet_orca_swap_sim.rs` | ❌ | **passed** — simulation rejects with `AccountNotInitialized` (expected for a dummy wallet), proving the instruction format is accepted by RPC |
| `devnet_orca_swap_e2e.rs` | ✅ (≥ 0.5 SOL on the keypair) | **passed** — real SOL → devUSDC swap; 3 on-chain signatures captured in [`DEVNET_PROOF.md`](./DEVNET_PROOF.md) |

The E2E test was run against the keypair at `data/devnet.json`
(pubkey `7YThgGWKvKP1YY8E5Pod9CeaHj2kHA1H3Z4gKwfyPHcT`) after funding it
with 1 SOL from the Solana faucet.

---

## Remaining risk — categorised

Risks are split into three buckets. **Accepted trade-offs** are deliberate
design choices; they do not require action. **Operational rules** are
failure modes that process discipline solves — see the runbook at the top
of [`TESTING.md`](./TESTING.md). **Open gaps** are the only items that
justify future code or infrastructure work.

### A. Accepted trade-offs (no action — document and move on)

| # | Item | Why we accept it |
|---|---|---|
| A1 | Case 13 in Phase 1 uses `ApprovalRegisteringProposer` stub, not the full simulate→policy→register path | The full pipeline is exercised by `n20_external_wallet_e2e.rs` (case 15). Case 13 is explicitly a **contract test** for the HTTP-route / pending-store boundary. Annotated as *contract* in `TESTING.md`. |
| A2 | Case 10 in Phase 2 opens a fresh `AuditRepository` over the same SQLite pool rather than restarting `clawd.exe` | Proves the **persistence-semantics** contract: repository reads back what was written. A full OS-process restart test would trigger LNK1104 on Windows (see rule 1). Out of scope for Phase 2 by design. |
| A3 | `AccumulativeSeeder` in `p2_api_blackbox.rs` is a test-local copy of `GatewayDemoSeeder`'s semantics, not the same struct | `GatewayDemoSeeder` is `daemon.rs`-private. Sharing the struct would couple integration tests to implementation details. The *contract* "each call appends one approval + one audit + one spend bump" is the tested invariant; the implementation is free to change. |
| A4 | Case 6 (lease countdown) re-seeds at test start | Absorbs build-time variability. The extra append is additive-safe for later tests (case 7 uses `toContainText`). |
| A5 | Playwright uses `next build` + `next start`, not `next dev` | Avoids the double Turbopack OOM on this machine. Revisit if upstream dev mode stabilises. |
| A6 | Chromium only | Text- / role-based selectors make cross-browser drift unlikely. Move to release-gate or nightly cadence rather than per-PR. |
| A7 | `OperatorIdentity.roles: Vec<String>` (not `HashSet`) | `approve.rs` uses `first()` to auto-fill approver role when body omits it — the order carries semantics. `Vec` matches the code. |
| A8 | Playwright depends on a pre-built `target/debug/clawd.exe` | Keeping Playwright decoupled from `cargo build` is deliberate; it's an onboarding step, not a test dependency. `globalSetup` already panics with a clear message if the binary is missing. |
| A9 | No dedicated `SubmitForSigningTool`-level integration test | The real simulate→policy→register→approve→sign path is already covered by `n6_signing_smoke.rs` and `n20_external_wallet_e2e.rs` at the `TransactionReviewPipeline` layer. A tool-level test would re-wire the same 15+ dependencies (pipeline, orchestrator, stores, event bus, repos, metrics, alerts) and re-assert the same invariants without exercising anything the pipeline-layer tests don't. High coupling, zero new signal. |

### B. Operational rules (process, not code)

See [`TESTING.md` § Operational rules](./TESTING.md#operational-rules-non-negotiable)
for the exact wording. Summary of the three:

| Rule | Failure mode it prevents |
|---|---|
| **1. `taskkill /F /IM clawd.exe` before `cargo test`** | False red: LNK1104 linker file-lock reported as a test failure |
| **2. `devnet_orca_swap_e2e` is opt-in + requires `CLAW_DEVNET_KEYPAIR`** | False red in CI + unintended on-chain spend |
| **3. `CLAW_E2E_FORCE_BUILD=1` on any Playwright run after editing `frontend/src/`** | False green: tests passing against a stale cached build |

### C. Open gaps (worth future work)

| # | Gap | Suggested direction |
|---|---|---|
| C1 | **No run artifact persisted.** `frontend/test-results/` is empty at rest; `cargo test` stdout is not saved anywhere. | **Closed.** Playwright now writes `html` + `json` reports to `frontend/playwright-report/`; `.config/nextest.toml` emits `target/nextest/ci/junit.xml`; `scripts/capture-run.sh` snapshots both into `docs/artifacts/<timestamp>/`. |
| C2 | **No CI.** | **Closed.** `.github/workflows/ci.yml` runs `cargo fmt --check`, `cargo clippy`, and `cargo nextest run --profile ci`, uploading `junit.xml` as a workflow artifact. Playwright and devnet E2E remain opt-in (see `capture-run.sh`). |
| C3 | **Invalid-token path on `/sessions/:id/approve` is not independently asserted.** | **Closed.** `p2_api_blackbox.rs` gained `approve_route_without_token_returns_401` and `approve_route_with_wrong_token_returns_401`. Both pass in the local run (17 tests in the file now). |
| C4 | **Inline `#[cfg(test)]` counts per module are not inventoried.** | **Closed.** `docs/artifacts/2026-04-15T08-45-59Z/test-inventory.txt` captures the per-binary inventory from `cargo test -- --list`. Headline **368 tests across 30 binaries** — matches the 366-passed baseline plus the 2 new C3 approve-route tests. Regenerate by re-running the enumeration steps in the artifact's `README.md`. |
| C5 | **Devnet E2E cannot run on a fresh clone** (deliberate — no funded wallet). The only durable evidence is the three signatures in `DEVNET_PROOF.md`. | **Accepted as nightly / manual job.** If a CI lane is added later, either pre-fund a test wallet via secret injection, or gate the job on a repository variable. |
| C6 | **Web3 E2E + chaos layer is missing.** No wallet-adapter mocking in Playwright (Phantom flow is manual-only), no visual-regression screenshots, no Toxiproxy-based RPC chaos tests, no simulate-OK-but-broadcast-fail → Dashboard-status end-to-end test. | **Open, scoped for future sprint.** Biggest ROI first: (a) wallet-adapter mock in Playwright so the sign path can be run headlessly; (b) one test asserting `TransactionFailed` → Dashboard status mapping when broadcast fails post-approval. Toxiproxy and visual-regression are nice-to-have after (a)+(b). |

---

## Recommendation for sponsor / reviewer evidence

Use the capture script — it wires the same commands plus the artifact
collection into one invocation:

```bash
bash scripts/capture-run.sh                # Rust + Playwright into docs/artifacts/<ts>/
bash scripts/capture-run.sh --skip-playwright  # Rust only (fastest)
bash scripts/capture-run.sh --force-build      # rebuild frontend before Playwright

# Devnet E2E remains opt-in; run separately and copy its stdout into
# the same artifact dir if you want a single bundle:
CLAW_DEVNET_KEYPAIR=$(pwd)/data/devnet.json \
  cargo test -p claw-solana-core --test devnet_orca_swap_e2e -- --nocapture \
  2>&1 | tee docs/artifacts/$(date -u +%Y-%m-%dT%H-%M-%SZ)/devnet-e2e.log
```

CI runs a subset of this automatically on every PR (see `TESTING.md § CI`);
the uploaded `rust-junit` artifact is the authoritative Rust signal.
