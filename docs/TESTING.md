# Testing overview

Index of every test layer in the repo, what it covers, where it lives, and how
to run it. This file is a **catalogue of what exists** — for the most recent
known run status, see [`TEST_RESULTS.md`](./TEST_RESULTS.md). For on-chain
evidence of end-to-end execution, see [`DEVNET_PROOF.md`](./DEVNET_PROOF.md).

---

## Operational rules (non-negotiable)

Three rules exist purely to prevent *false red* or *false green* outcomes
when running the current suite. These are process rules, not code changes;
violating them produces misleading results even though nothing is broken.

> **Rule 1 — Kill any live `clawd.exe` before `cargo test`.**
> Windows linker produces `LNK1104 "cannot open file ... .exe"` when a
> prior daemon process still holds shared `.rlib` deps in `target/debug/`.
> This is a build-tooling file-lock, **not** a test-assertion failure.
> Always prefix a fresh run with:
>
> ```bash
> taskkill /F /IM clawd.exe   # no-op if nothing is running
> cargo test ...
> ```

> **Rule 2 — `devnet_orca_swap_e2e` is opt-in. It requires `CLAW_DEVNET_KEYPAIR` pointing at a devnet-funded keypair.**
> The test signs and submits real transactions to devnet. Running it
> without a funded keypair will panic or fail to submit — that is not a
> product regression. Run it only in a **manual** or **nightly** lane and
> capture the produced signatures as artifacts (see `DEVNET_PROOF.md`).
> It does not belong in the default per-PR lane.

> **Rule 3 — Any change under `frontend/src/` requires `CLAW_E2E_FORCE_BUILD=1` on the next Playwright run.**
> `globalSetup` caches the two `next build` outputs under
> `.next-e2e-3000/` and `.next-e2e-3001/` keyed only by the presence of
> `BUILD_ID`. It does **not** inspect source mtimes. Running Playwright
> after a frontend edit without this env var will test the *previous*
> build — a green run in that situation proves nothing about the change.
>
> ```bash
> CLAW_E2E_FORCE_BUILD=1 npx playwright test
> ```

---

---

## Test layers

| Layer | What it proves | Location | Mode |
|---|---|---|---|
| **Unit / inline** | Pure function correctness, trait implementations, serde round-trips | `crates/*/src/**/*.rs` (inside `#[cfg(test)]` modules) | `cargo test` |
| **Rust integration** | Multi-crate flows: policy → approval store → workflow state machine → audit | `crates/gateway/tests/*.rs`, `crates/solana-core/tests/*.rs` | `cargo test --test <name>` |
| **API black-box** | HTTP route contract: status codes, body shape, auth gating, pagination | `crates/gateway/tests/p2_api_blackbox.rs` | `cargo test` |
| **Devnet integration** | Real on-chain execution against Solana devnet RPC | `crates/solana-core/tests/devnet_*.rs` | `cargo test` (requires funded keypair) |
| **Frontend E2E (Playwright)** | Live mode dashboard renders real gateway data; showcase mode renders fixtures | `frontend/tests/e2e/*.spec.ts` | `npx playwright test` |

### Test kinds — what each test is actually proving

To prevent "coverage inflation" misreads, every test should be understood as
one of these shapes. **Passing a contract test does not imply full-pipeline
correctness**, and vice versa.

| Kind | Meaning | Examples |
|---|---|---|
| **Unit** | Pure function or small struct, no I/O | `risk-engine/src/policy.rs : rule_priority_is_first_match_wins` |
| **Integration** | Multiple in-process modules stitched through real in-memory state | `n26_multi_stage_and_quorum_runtime.rs : multi_step_chain_happy_path_completes_signing` |
| **Contract** | Enforces one edge of a boundary (HTTP status, return-shape, side-effect row) with the *other* side stubbed | `p1_core_correctness.rs : propose_transfer_success_surfaces_approval_in_pending_list` — proves the route + `/pending-approvals` contract; the proposer itself is a stub that registers the approval. Full simulate→policy→register pipeline is covered separately (see `n20_external_wallet_e2e.rs`). |
| **Persistence semantics** | Proves durability behaviour by reopening the repository over the same storage pool — **not** an OS-level process restart | `p2_api_blackbox.rs : case10_audit_rows_survive_restart_via_fresh_repository`; `n25_approval_workflow_restart.rs` |
| **Full-pipeline in-memory** | End-to-end through the real production code path, but without touching RPC or a real browser | `n20_external_wallet_e2e.rs : external_wallet_full_e2e_challenge_bind_approve_sign_complete` |
| **Devnet on-chain** | Real Solana devnet RPC, real keypair signing, real submitted transaction | `crates/solana-core/tests/devnet_orca_swap_e2e.rs` |
| **UI E2E (Playwright)** | Real browser against the real gateway (or against fixtures in `showcase` mode) | `frontend/tests/e2e/dashboard.live.spec.ts` |

---

## Spec-case coverage matrix

Cases below are grouped by the three delivery phases described below. **Phase
labels (Phase 1 / 2 / 3) are internal to this working session and are NOT the
same as the "P1-A / P2" labels that appear in commit `555e4df`** — that commit
uses P1/P2 for *reviewer finding severity*, unrelated to this test taxonomy.

### Phase 1 — Core correctness (Rust unit / integration)

| # | Case | Test file : fn |
|---|---|---|
| 1 | SOL transfer ≥ 0.01 SOL triggers single-stage human approval | `n22_config_policy_wiring.rs : large_transfer_triggers_human_approval`; `risk-engine/src/policy.rs : amount_threshold_matches_transfer_amount` |
| 2 | USDC ≥ 10,000 triggers risk → treasury chain end-to-end | `n22_config_policy_wiring.rs : usdc_50k_usdc_triggers_multi_stage_chain`; `p1_core_correctness.rs : usdc_10k_triggers_chain_and_completes_through_risk_and_treasury` |
| 3 | Legacy SPL Token Transfer rejected by guard rule | `risk-engine/src/policy.rs : legacy_token_transfer_present_fires_for_legacy_transfer` + `legacy_token_transfer_guard_before_token_amount_closes_bypass` |
| 4 | First-match-wins rule ordering | `risk-engine/src/policy.rs : rule_priority_is_first_match_wins`; `n22_config_policy_wiring.rs : unlisted_program_is_rejected` |
| 5 | Stage 0 approve → `StageAdvanced`; stage 1 → `Approved` | `n26_multi_stage_and_quorum_runtime.rs : multi_step_chain_happy_path_completes_signing` |
| 6 | Wrong role → `RoleMismatch` | `n26_multi_stage_and_quorum_runtime.rs : later_stage_cannot_approve_before_earlier_stage`; `n23_operator_authz_runtime.rs : self_claimed_higher_role_is_rejected` |
| 7 | Quorum = 2 requires two distinct operators | `n26_multi_stage_and_quorum_runtime.rs : quorum_stage_completes_only_after_n_unique_approvals` |
| 8 | Same operator voting twice → `DuplicateOperator` | `n26_multi_stage_and_quorum_runtime.rs : duplicate_approval_from_same_operator_counts_once` |
| 9 | `ApprovalOutcome::Expired` returned with no request | `n27_wallet_policy_and_leases.rs : expired_signing_lease_rejects_late_approval` |
| 10 | Expiry audit writes `approval_lease_expired`, never `human_approved` / `human_rejected` | `p1_core_correctness.rs : expired_decide_writes_lease_expired_audit_not_human_decision` |
| 11 | Durable workflow state recovers after restart | `n25_approval_workflow_restart.rs : approval_workflow_state_survives_restart_mid_process` |
| 12 | propose-transfer failure does NOT create pending approval | `p1_core_correctness.rs : propose_transfer_failure_returns_422_with_no_pending_approval` |
| 13 | propose-transfer success + policy hit creates pending approval | `p1_core_correctness.rs : propose_transfer_success_surfaces_approval_in_pending_list` |
| 14 | Tampered signed tx rejected | `n20_external_wallet_e2e.rs : external_wallet_e2e_tampered_tx_rejected` |
| 15 | External wallet happy path verify → submit → lifecycle advance | `n20_external_wallet_e2e.rs : external_wallet_full_e2e_challenge_bind_approve_sign_complete` |

**Kind annotations** (matters for how each result should be read):
- Cases **1, 3, 4, 5, 6, 7, 8, 9** — *integration*. Real production code paths, in-memory state only.
- Case **2** — split across *integration* (policy evaluation side in `n22`) and a stitched *integration* end-to-end walk in `p1_core_correctness.rs`.
- Case **10** — *persistence-semantics + contract*. It exercises `emit_decide_audit` extracted from the daemon against a real `AuditRepository`. Proves the invariant "expiry does not emit human_* rows"; it does not spin up the full daemon.
- Case **11** — *persistence-semantics*. Opens a fresh `ApprovalWorkflowRepository` over the same SQLite file; not an OS process restart.
- Case **12** — *contract*. Proposer returns `Err` → HTTP 422 → `/pending-approvals` empty. The proposer is a stub; the contract tested is "422 never leaks state", not the simulate→policy error flow itself.
- Case **13** — *contract*. Proposer stub emulates the pipeline by registering an approval before returning success. Full simulate→policy→register path is exercised separately by **case 15** (`n20`).
- Cases **14, 15** — *full-pipeline in-memory*.

### Phase 2 — Operator / API black-box

Single file: [`crates/gateway/tests/p2_api_blackbox.rs`](../crates/gateway/tests/p2_api_blackbox.rs).

| # | Case | Test fn |
|---|---|---|
| 1 | `GET /policy/rules` preserves config order | `case1_policy_rules_preserve_config_order` |
| 2 | `GET /pending-approvals` cross-session + workflow payload | `case2_pending_approvals_are_cross_session_and_include_workflow` |
| 3 | `GET /wallets` includes daily_spend | `case3_wallets_include_daily_spend` |
| 4 | `GET /audit` pagination (limit + offset, DESC) | `case4_audit_pagination_limit_and_offset` |
| 5 | `/health` public, `/metrics` auth-gated | `case5_health_public_metrics_auth_gated` |
| 6a | `/debug/seed-demo` returns 503 when env gate off | `case6a_seed_demo_disabled_returns_503` |
| 6b | `/debug/seed-demo` is accumulative | `case6b_seed_demo_accumulates_each_call` |
| 7a | `RoleMismatch` → 403 + `role_mismatch` body | `case7a_role_mismatch_returns_403` |
| 7b | Operator claims role they don't hold → 403 + `role_not_held` | `case7b_role_not_held_returns_403` |
| 7c | `DuplicateOperator` → 409 + `duplicate_operator` | `case7c_duplicate_operator_returns_409` |
| 7d | `Expired` → 410 + `lease_expired` | `case7d_expired_returns_410_gone` |
| 8 | Dashboard composite: pending / audit / wallets align | `case8_dashboard_composite_matches_across_endpoints` |
| 10 | Audit rows survive "restart" (fresh repository over same pool) | `case10_audit_rows_survive_restart_via_fresh_repository` |

Case 9 (propose-transfer success/failure) is already covered in Phase 1
(`p1_core_correctness.rs`) and is not re-asserted at the HTTP layer.

**Kind annotations**:
- Cases **1–8, 6a, 6b, 7a–7d** — *contract*. Drive the real `axum::Router`
  with stubbed business-logic adapters; verify status code + body shape +
  store side-effects.
- Case **10** — *persistence-semantics*, not a process restart. The test
  drops the original `AuditRepository` and builds a fresh one sharing the
  same SQLite pool. This is sufficient to prove "the repository reads
  back what was written"; it is not a replacement for a real restart
  test, which is out of scope for Phase 2 by design.
- Case **6b** asserts the *accumulative* contract of `/debug/seed-demo` —
  this is a **contract** between the HTTP endpoint and any client that
  re-calls it. The local `AccumulativeSeeder` mirrors the production
  `GatewayDemoSeeder` semantics on purpose; they are not shared code.

### Phase 3 — Frontend E2E (Playwright)

Two projects against two Next.js instances (production `next start`):

| File | Project | Tests |
|---|---|---|
| [`frontend/tests/e2e/dashboard.showcase.spec.ts`](../frontend/tests/e2e/dashboard.showcase.spec.ts) | `showcase` on `:3001` | 3 (dashboard / policy / audit renders from fixtures) |
| [`frontend/tests/e2e/dashboard.live.spec.ts`](../frontend/tests/e2e/dashboard.live.spec.ts) | `live` on `:3000` | 7 (cases 2–8: LIVE badge, dashboard numbers, graceful degradation, chain stages, countdown decrement, audit event types, policy rule order + wallet tab) |

**Kind annotations**:
- All tests in both projects are *UI E2E* — real Chromium driving either
  the real gateway on `:7070` (live) or the fixture-backed Next build
  (showcase).
- Case 6 (lease countdown) re-calls `/debug/seed-demo` inside the test to
  guarantee a fresh lease. This is a deliberate *stability* choice: the
  cached Next build + long-lived daemon means the original seed's 5-minute
  lease may already be expired by the time the test runs. The extra
  append does not interfere with later tests (case 7 uses `toContainText`
  which is additive-safe).

Supporting infrastructure (non-test but required):

- [`frontend/playwright.config.ts`](../frontend/playwright.config.ts) — two-project split
- [`frontend/tests/e2e/global-setup.ts`](../frontend/tests/e2e/global-setup.ts) — starts daemon + seeds + builds + serves two Next instances, with a `.next-e2e-<port>/BUILD_ID` cache skip (`CLAW_E2E_FORCE_BUILD=1` to force rebuild)
- [`frontend/tests/e2e/global-teardown.ts`](../frontend/tests/e2e/global-teardown.ts) — kills PIDs recorded progressively by `recordSpawned`, plus `taskkill /IM clawd.exe` backstop on Windows

---

## Deterministic vs environment-dependent

| Category | Runs cleanly offline? | Notes |
|---|---|---|
| Unit / inline | ✅ yes | Pure in-memory |
| Rust integration (`n*`, `c1`, `p1`, `p2`) | ✅ yes | SQLite in-memory; no RPC / network |
| `crates/solana-core/tests/devnet_orca_check.rs` | ⚠️ needs devnet RPC | Probes account existence only — no signing |
| `crates/solana-core/tests/devnet_orca_swap_sim.rs` | ⚠️ needs devnet RPC | Builds + simulates — no funds needed |
| `crates/solana-core/tests/devnet_orca_swap_e2e.rs` | ⚠️ needs devnet RPC **and** funded keypair at `data/devnet.json` (or `CLAW_DEVNET_KEYPAIR=<path>`) | Real on-chain swap |
| Playwright E2E | ✅ yes (daemon spawned locally, no external RPC reached during tests) | Requires `cargo build --bin clawd` first; `npx playwright install chromium` once |

---

## How to run

### Full Rust workspace tests

```bash
# Clean up any stray daemon from previous runs — Windows linker can hit
# LNK1104 file-lock if clawd.exe is still running against target/debug.
taskkill /F /IM clawd.exe   # Windows; no-op if not running

cargo test -p claw-types -p claw-risk-engine -p claw-state-store \
           -p claw-api -p claw-gateway
```

### Capture a full run as a durable artifact

```bash
# One-shot: runs cargo test (or nextest if installed), runs Playwright,
# and drops everything under docs/artifacts/<timestamp>/ ready for review
# or CI upload.
bash scripts/capture-run.sh

# Flags: --skip-playwright / --skip-rust / --force-build
```

### Rust via cargo-nextest (recommended for reviewers)

Install once:

```bash
cargo install cargo-nextest --locked
```

Run the same gate that CI runs:

```bash
cargo nextest run --profile ci \
  -p claw-types -p claw-risk-engine -p claw-state-store \
  -p claw-api -p claw-gateway
# → target/nextest/ci/junit.xml
```

The profile is defined in [`.config/nextest.toml`](../.config/nextest.toml).

### Just the Phase 1 / Phase 2 files

```bash
cargo test -p claw-gateway --test p1_core_correctness
cargo test -p claw-gateway --test p2_api_blackbox
```

### Devnet probes (no funds needed)

```bash
cargo test -p claw-solana-core --test devnet_orca_check     -- --nocapture
cargo test -p claw-solana-core --test devnet_orca_swap_sim  -- --nocapture
```

### Devnet E2E (needs funded keypair)

```bash
# Fund the keypair at data/devnet.json first, then:
CLAW_DEVNET_KEYPAIR=C:/Users/jeffr/SolFrontier/data/devnet.json \
  cargo test -p claw-solana-core --test devnet_orca_swap_e2e -- --nocapture
```

### Playwright E2E

```bash
cd frontend

# One-time install
npm install --save-dev @playwright/test
npx playwright install chromium

# Before first run: make sure the daemon binary exists
cargo build --bin clawd       # from repo root

# Run
npx playwright test                                  # full suite (10 tests)
npx playwright test --project=live                   # live only (7 tests)
npx playwright test --project=showcase               # showcase only (3 tests)
npx playwright test -g "case[456]" --project=live    # fast debug loop
CLAW_E2E_FORCE_BUILD=1 npx playwright test           # force Next rebuild
```

---

## Known limitations

Items covered by the three operational rules at the top of this file
(LNK1104 mitigation, devnet keypair requirement, `CLAW_E2E_FORCE_BUILD`)
are **not** repeated here. The items below are known trade-offs that do
not have a simple process workaround.

1. **Playwright uses `next build` + `next start`** (production mode), not
   `next dev`, because two Turbopack dev servers racing OOM on this
   machine. Trade-off: UI code changes require a rebuild to show up in
   E2E, unlike manual `npm run dev`. This amplifies rule 3 above.
2. **No cross-browser coverage.** Only Chromium is installed. Firefox /
   WebKit would be opt-in via `npx playwright install firefox webkit` and
   an extra Playwright project — fine to add at release-gate / nightly
   cadence, not per-PR.
3. **No CI.** All numbers in `TEST_RESULTS.md` are from local runs. See
   `TEST_RESULTS.md § Recommendation` for the suggested artifact capture
   sequence to bridge this gap without adding CI code to the repo yet.
4. **Inline `#[cfg(test)]` unit tests per module are not inventoried.**
   The 366 workspace pass figure is aggregate. If a module gains or loses
   inline tests, this document won't reflect it until the next aggregate
   run is re-observed.

---

## Test file inventory (authoritative count)

Rust integration files (`cargo test`-style):

| File | `#[test]` / `#[tokio::test]` count |
|---|---:|
| `crates/gateway/tests/c1_rate_limit_runtime.rs` | 4 |
| `crates/gateway/tests/n6_signing_smoke.rs` | 7 |
| `crates/gateway/tests/n7_spend_tracking.rs` | 7 |
| `crates/gateway/tests/n10a_external_wallet.rs` | 18 |
| `crates/gateway/tests/n10b_bridge_simulation.rs` | 3 |
| `crates/gateway/tests/n10c_store_hardening.rs` | 23 |
| `crates/gateway/tests/n10d_daemon_integration.rs` | 3 |
| `crates/gateway/tests/n15_orchestrator_integration.rs` | 13 |
| `crates/gateway/tests/n16_durability.rs` | 9 |
| `crates/gateway/tests/n16b_durability_hardening.rs` | 8 |
| `crates/gateway/tests/n16c_durable_first.rs` | 6 |
| `crates/gateway/tests/n17_semantic_cleanup.rs` | 7 |
| `crates/gateway/tests/n18_foundation_audit.rs` | 38 |
| `crates/gateway/tests/n19_wallet_challenge.rs` | 9 |
| `crates/gateway/tests/n20_external_wallet_e2e.rs` | 3 |
| `crates/gateway/tests/n21_submission_invariants.rs` | 11 |
| `crates/gateway/tests/n22_config_policy_wiring.rs` | 19 |
| `crates/gateway/tests/n23_operator_authz_runtime.rs` | 4 |
| `crates/gateway/tests/n24_session_policy_lifecycle.rs` | 7 |
| `crates/gateway/tests/n25_approval_workflow_restart.rs` | 4 |
| `crates/gateway/tests/n26_multi_stage_and_quorum_runtime.rs` | 7 |
| `crates/gateway/tests/n27_wallet_policy_and_leases.rs` | 6 |
| `crates/gateway/tests/n28_policy_alerting_runtime.rs` | 6 |
| `crates/gateway/tests/p1_core_correctness.rs` | 4 |
| `crates/gateway/tests/p2_api_blackbox.rs` | 13 |
| `crates/solana-core/tests/devnet_orca_check.rs` | 1 |
| `crates/solana-core/tests/devnet_orca_swap_sim.rs` | 1 |
| `crates/solana-core/tests/devnet_orca_swap_e2e.rs` | 1 |

**Integration-file test count** (grep `^#[test]\|^#[tokio::test]` headers):
sum = **240 tests** across 28 gateway+solana-core test files.

> The ~126-count difference between this figure and the **366** reported in
> `TEST_RESULTS.md` is accounted for by inline `#[cfg(test)]` unit tests
> across `claw-types`, `claw-risk-engine`, `claw-state-store`,
> `claw-api`, and various `crates/gateway/src/**` modules. Those are not
> inventoried per-file here because the exact per-module count is only
> surfaced by running `cargo test`. The 366 figure comes from the
> aggregated `cargo test -p claw-types -p claw-risk-engine -p
> claw-state-store -p claw-api -p claw-gateway` output in the
> Phase 2 session.

Playwright E2E:

| File | Tests |
|---|---:|
| `frontend/tests/e2e/dashboard.showcase.spec.ts` | 3 |
| `frontend/tests/e2e/dashboard.live.spec.ts` | 7 |

### Artifact locations (durable outputs)

| Path | Produced by | Purpose |
|---|---|---|
| `target/nextest/ci/junit.xml` | `cargo nextest run --profile ci` | JUnit-format Rust test report; uploaded by CI |
| `frontend/playwright-report/` | `npx playwright test` | HTML + JSON Playwright report |
| `docs/artifacts/<timestamp>/` | `bash scripts/capture-run.sh` | A dated snapshot of everything above for a single reviewer-grade run |

All three paths are gitignored (see `frontend/.gitignore`) except the
`docs/artifacts/` parent, which is kept via `.gitkeep`. That means a
reviewer can commit a particular snapshot by force-adding it if they want
to pin it in history, otherwise artifacts are per-run only.

### CI

A minimal GitHub Actions lane lives at
[`.github/workflows/ci.yml`](../.github/workflows/ci.yml) and gates every
PR with:

- `cargo fmt --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo nextest run --profile ci` (Rust `-p claw-types -p claw-risk-engine -p claw-state-store -p claw-api -p claw-gateway`)
- Uploads `junit.xml` as a workflow artifact

Deliberately **not** in CI: `devnet_orca_*` (needs a funded keypair) and
Playwright E2E (needs the daemon + wallet fixture). Those run via
`capture-run.sh` locally and produce their own artifacts.
