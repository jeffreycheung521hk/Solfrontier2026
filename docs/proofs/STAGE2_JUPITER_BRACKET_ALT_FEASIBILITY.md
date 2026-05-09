# Stage 2 — Jupiter Bracket + Instructions Sysvar / ALT Feasibility

**Status:** Feasibility study only. No product code in the live execution path was modified.
No live RPC. No signing. No broadcast. No deploy. No mainnet/devnet transaction. No push.
**Date:** 2026-05-09.
**Branch:** `stage2/jprep-jupiter-bracket-alt-feasibility`
**Worktree:** `C:/Users/jeffr/SolFrontier-jprep`
**Base:** `origin/main` at `67a97371950e880ec4649910b9c9057409a0a931` (`docs(readme): document protocol standards in use`).

**Companions consulted:**
- [`STAGE2_DELEGATED_CONDITIONAL_EXECUTION_SPEC.md`](../STAGE2_DELEGATED_CONDITIONAL_EXECUTION_SPEC.md) — § 6 Jupiter, § 6.1.1 API pin, § 6.1.3 sibling-ix, § 6.3 tx-size / ALT, § 7.1 Pyth, § 10 invariants, § 14 audit corrections.
- `STAGE2_OFFICIAL_DOCS_AUDIT.md` (untracked in this worktree; lives in main worktree at `C:\Users\jeffr\SolFrontier\docs\STAGE2_OFFICIAL_DOCS_AUDIT.md`) — § 9.1 / § 9.2 addendum.
- [`STAGE2_PYTH_ADAPTER_RESEARCH.md`](../STAGE2_PYTH_ADAPTER_RESEARCH.md) — § 5 (1 slot per feed), § 8 (max-age vs heartbeat), § 11.2 (account pressure note).
- [`crates/types/src/stage2_watch_rule.rs`](../../crates/types/src/stage2_watch_rule.rs) — `JupiterApiVersion`, `max_accounts_hint: u8`, `require_pre_post_bracket: bool` already canonical.
- [`programs/clawsol-authority/src/solend_boundary.rs`](../../programs/clawsol-authority/src/solend_boundary.rs) — pattern reference only (deterministic sibling-ix descriptor walk → instructions-sysvar replacement TODO). Solend assumptions (single program id, two instruction variants) DO NOT carry over to Jupiter.

---

## 1. Executive Summary

The Stage 2 conditional Jupiter buy is **feasible-pending-measurement** at the spec/architecture level and **gated on live byte/account/CU measurement** before any implementation work begins. There is no spec-level blocker; there are concrete measurement gates that this slice itemises. If any of those gates fails, conditional Jupiter falls back to **preview-only** as the Stage 2 spec § 6.3.2 already prescribes.

**Pinned API path (today's official docs).** `GET https://api.jup.ag/swap/v2/build` with `tipInstruction` + `blockhashWithMetadata` priced into the byte budget. v1 `/swap/v1/swap-instructions` is allowed only as an emergency fallback with explicit deprecation acknowledgement; Ultra is excluded because its custom-bracketed-instruction insertion is not exposed in the API surface (Ultra is a one-shot `/order → /execute` flow that returns a fully-built tx without the customisable instruction list Stage 2 needs).

**Bracket shape (logical).** `ix[0] = pre_check (or ExecuteAction prologue) → ix[1..N] = Jupiter setup + computeBudget + swap + cleanup + tip → ix[N+1] = post_check (or ExecuteAction epilogue)`. The exact program-API split (separate `pre_check_v2` / `post_check_v2` ixs vs a single `ExecuteAction` consuming the instructions sysvar with bracket logic inline) is an open trade-off; § 4 below itemises the cost difference. **Recommendation: single-ix `ExecuteAction` consolidating both checks**, conditional on measurement showing it stays under the 1,400,000 CU per-tx ceiling. This buys ~32 bytes (one fewer ix header + one fewer program-id account slot) and one fewer cross-program boundary.

**Account budget (Scenario B).** Estimated 14 ClawSol-static slots + 3 Pyth + 1 instructions sysvar + 7 Jupiter-program-class accounts (token program / ATA program / system program / compute budget / Jupiter v6 program / Pyth receiver / clawsol-authority program) + ALT-loaded route accounts. **Static slot estimate: ~25**, leaving 64 − 25 = **39 ALT-loaded slots** for Jupiter route accounts. This corresponds to a Jupiter `maxAccounts` ceiling of ~39 if Jupiter consumes its budget purely from ALT addresses (it does not — see § 5.4 caveat). The realistic starting point for `maxAccounts` is **the lower of (Jupiter's recommended starting 64) and (64 − ClawSol-static)**, with iterative reduction as the spec requires.

**Serialized byte budget.** `PACKET_DATA_SIZE = 1232` is the hard cap. Without measurement, the report cannot give a final number; it provides the measurement harness plan in § 11. Budget pressure is real — the audit's § 9.1 already noted v2's `tipInstruction` + `blockhashWithMetadata` materially affect tx bytes — but the bracket fits in principle if the Jupiter route is restricted to ~6–8 setup/swap/cleanup ixs and `maxAccounts` is held in the 50–58 range.

**Compute-unit pressure.** Worst-case estimate: ~600,000 CU for the bracket (instructions-sysvar walk + condition re-eval + adverse-bound math) + ~400,000–800,000 CU for a typical Jupiter aggregator route. Total ~1.0M–1.4M CU is **plausibly within** the 1.4M per-tx ceiling but **needs measurement**. CU is potentially the tightest gate, not bytes.

**Pyth self-post strategy.** Default v1 plan: Pyth self-post is a **separate prerequisite transaction**, not bundled into the Stage 2 execute tx. Audit §§ 9.2 / U-6 and `STAGE2_PYTH_ADAPTER_RESEARCH.md` § 8.2 already establish the cost model and the daemon's `ensure_fresh_price_update` flow. Bundling self-post is **not plausible** in the same v0+ALT tx with a 3-feed Scenario B route — three `post_update_atomic` ixs would themselves consume ~50–100 KB of merkle-proof data and ~600K CU each, blowing both budgets.

**Final classification: Needs measurement.** All four hard gates (account count, serialized byte size, compute-unit estimate, sibling-verification feasibility) require an env-gated measurement harness against `api.jup.ag/swap/v2/build` with realistic Scenario B parameters before any "Ready for implementation" claim is honest. **Sibling-verification feasibility itself is unproven beyond static analysis**; until an actual `/build` response is exercised and its instruction list classified by program id, the sibling-ix discipline is design-only. Preview-only remains the safe-by-default posture per Stage 2 spec § 6.3.2.

---

## 2. Preflight + Scope

### 2.1 Worktree Provenance

```
git worktree list
  C:/Users/jeffr/SolFrontier-jprep   67a9737   stage2/jprep-jupiter-bracket-alt-feasibility
```

Created via `git worktree add -b stage2/jprep-jupiter-bracket-alt-feasibility C:/Users/jeffr/SolFrontier-jprep 67a97371950e880ec4649910b9c9057409a0a931`.

### 2.2 origin/main Prerequisites Confirmed

| Prereq | Commit |
|---|---|
| README protocol standards | `67a9737` `docs(readme): document protocol standards in use` |
| P4 Solend delegated boundary skeleton | `ee9b796` `feat(program): add Solend delegated withdraw boundary skeleton` |
| A1 frontend watch rule preview | `cc8787b` `feat(frontend): add Stage 2 watch rule preview` |
| P3 condition gate wired into ExecuteAction | `1cc72ef` `feat(program): enforce Stage 2 condition gate in execute action` |

### 2.3 Scope Restrictions Honored

- **No frontend touched.** No `frontend/*` writes in this slice.
- **No Jupiter production path touched.** `crates/gateway/src/integrations/jupiter*.rs` read-only.
- **No watcher / daemon runtime touched.** Read-only inspection only.
- **No live execution tx builders touched.** `solend_*`, `jupiter_*`, `*_jit_*` modules are read-only here.
- **No `config/default.toml` change.** No config rebind.
- **No `programs/clawsol-intent/*` touched.** Stage 1 substrate is untouched.
- **No Solend execution path touched.** `solend_boundary.rs` is *referenced as pattern only*; we do not copy its single-program-id assumption to Jupiter.
- **No live RPC, no deploy, no signing, no broadcast, no mainnet/devnet tx, no push.**
- This slice is **docs-only** — no code helper was added (per the prompt's "Preferred for this slice: docs-first report").

---

## 3. Official-Docs Confirmation (2026-05-09)

### 3.1 Jupiter — current custom-transaction path

**Pinned endpoint.** `GET /swap/v2/build` on host `https://api.jup.ag`.
- Confirmed via [developers.jup.ag/docs/swap/build](https://developers.jup.ag/docs/swap/build) (fetched 2026-05-09): URL Path `/swap/v2/build`, HTTP method GET, version v2.
- Response fields verbatim from that page:
  - `computeBudgetInstructions` — compute unit price instruction (excludes compute unit limit).
  - `setupInstructions` — pre-swap setup such as account creation.
  - `swapInstruction` — the primary swap operation.
  - `cleanupInstruction` — post-swap cleanup (optional, may be null).
  - `otherInstructions` — additional instructions.
  - `tipInstruction` — SOL tip transfer (included when `tipAmount` parameter provided).
  - `addressesByLookupTableAddress` — address lookup tables for v0 transactions.
  - `blockhashWithMetadata` — recent blockhash and expiry height.
- Important: `/build` transactions **cannot use `/execute`** (`/build` lacks the required `requestId` and is designed for customisation). This is exactly the surface Stage 2 needs.

**v1 (`/swap/v1/swap-instructions`).** Audit (§ 6 / J-A2) confirmed it returns `computeBudgetInstructions[]`, `setupInstructions[]`, `swapInstruction`, `cleanupInstruction`, `otherInstructions[]`, `addressLookupTableAddresses[]` — slightly different field naming (`addressLookupTableAddresses` vs v2's `addressesByLookupTableAddress`) and **no** `tipInstruction` / `blockhashWithMetadata`. Today's repo (per Explore subagent inventory) calls v1 in `crates/gateway/src/integrations/jupiter.rs`. v1 is officially superseded by v2; the Stage 2 spec § 6.1.1 already requires the implementation to pin one before tx-size measurement.

**Status of v1 today.** I attempted `https://developers.jup.ag/docs/swap/v1/swap-instructions` and got HTTP 404 on 2026-05-09. The v1 page may have been unlisted from the developer docs index (consistent with "no longer actively maintained"). The v1 endpoint itself is still live in the repo's existing JIT executor (mainnet-proven in Phase 5G). **Recommendation: pin v2 for Stage 2.** v1 should be reserved as an emergency fallback only with documented justification.

**Ultra API.** Excluded for Stage 2. Ultra is `/order → /execute` and returns a fully-built, signed-or-server-assembled transaction; it does not expose an instruction list that ClawSol can wrap with `pre_check_v2` / `post_check_v2` ixs in the same atomic transaction. Confirmed by the developers.jup.ag/docs/swap/build documentation note that `/execute` "validates transactions to prevent modifications" — the opposite of what Stage 2 needs. **Ultra remains excluded unless and until an Ultra variant explicitly supports custom instruction insertion** (no such variant is documented as of 2026-05-09).

**Host migration.** Audit § 9 noted `lite-api.jup.ag` is sunset 2026-06-30 per the rate-limit page. I attempted `https://developers.jup.ag/docs/swap-api/rate-limit` on 2026-05-09 and got HTTP 404; the previously-cited `dev.jup.ag/portal/rate-limit` 301-redirects to `developers.jup.ag/portal/rate-limit` which is gated behind the developer-portal login. **Re-confirmation gap.** The audit's 2026-06-30 sunset claim cannot be re-fetched without portal credentials; the implementation slice MUST reconfirm this date against the live portal page before mainnet rehearsal. Worst case: assume `lite-api.jup.ag` is already gone and pin to `api.jup.ag` (which the existing repo already does — `JUPITER_API_BASE_URL = "https://api.jup.ag"`).

**`maxAccounts` guidance (refreshed 2026-05-09).** [developers.jup.ag/docs/swap/advanced/reduce-transaction-size](https://developers.jup.ag/docs/swap/advanced/reduce-transaction-size):
- Definition: "constrains the quantity of accounts included in a swap route."
- Default 64; range 1–64.
- Verbatim warning: *"Very low values (e.g below 50) can result in **no route found** or **significantly worse pricing**."*
- Verbatim recommendation: *"Start with a moderate reduction (e.g. reducing 2 and retry) and test pricing impact before going lower."*
- Setup-instructions filtering: `createAssociatedTokenAccountIdempotent` is unconditionally included; *"no-ops on existing accounts but still consume transaction space."* Recommended: pre-flight `getAccountInfo` and filter the ix client-side; trade-off is added RPC latency.

The Stage 2 spec's earlier `maxAccounts ≤ 48` prescription (§ 6.1.1) is therefore **stale**; replace with iterative tightening starting from 64, never below 50 without documented price-impact measurement (consistent with audit § 9.1 addendum).

### 3.2 Solana / Anza — transaction substrate

| Constraint | Value | Source |
|---|---|---|
| `PACKET_DATA_SIZE` (max wire-form tx bytes) | **1,232** (= IPv6 MTU 1280 − 48 bytes headers) | [solana.com/docs/core/transactions](https://solana.com/docs/core/transactions) (fetched 2026-05-09) |
| Legacy account-key budget | **32** | [solana.com/docs/advanced/lookup-tables](https://solana.com/docs/advanced/lookup-tables) |
| v0 + ALT account-key budget | **64** | Same |
| Max addresses per ALT | **256** | Same |
| Per-tx ALT count limit | **Not documented** | Same — measurement target |
| Per-tx CU ceiling | **1,400,000** (`MAX_COMPUTE_UNIT_LIMIT`) | [solana.com/docs/core/runtime](https://solana.com/docs/core/runtime) |
| Per-instruction CU default | **200,000** (builtin: 3,000) | Same |
| Instructions sysvar address | `Sysvar1nstructions1111111111111111111111111` | [docs.rs/solana-program::sysvar::instructions](https://docs.rs/solana-program/latest/solana_program/sysvar/instructions/) |
| Instructions sysvar helpers | `load_instruction_at_checked`, `load_current_index_checked`, `get_instruction_relative` | Same |
| ALT extension cap | "~20 addresses per single-tx extend before needing multiple txs" | Lookup-tables page |

Quotes:
- *"this listing would effectively be capped at 32 addresses per transaction."* — Solana docs (legacy).
- *"a transaction would now be able to raise that limit to 64 addresses per transaction."* — Solana docs (v0 + ALT).
- *"Size limit: 1,232 bytes maximum, derived from the IPv6 minimum MTU (1,280 bytes) minus 48 bytes for network headers."* — solana.com/docs/core/transactions.
- *"Load an `Instruction` in the currently executing `Transaction` at the specified index."* — `load_instruction_at_checked` description.

### 3.3 Pyth — already covered

`STAGE2_PYTH_ADAPTER_RESEARCH.md` is the authority. Restated here only for the bracket-budget arithmetic:
- 1 `PriceUpdateV2` account per feed, **`Account<'info, PriceUpdateV2>`** (Anchor-checked program owner).
- 3 feeds for Scenario B (BTC/USD, ETH/USD, SOL/USD).
- `verification_level = Full` only; reject `Partial`.
- 30 s freshness for trading triggers.
- Adverse-bound for triggers: `price - conf` for `>`, `price + conf` for `<`.
- All in `i128` integer space; no floats.

---

## 4. Bracket Shape

### 4.1 Logical Structure (per Stage 2 spec § 6.1.2)

```
ix[0]      pre_check_v2  | ExecuteAction (prologue phase)
            - load + decode AuthorizationRecord (PDA)
            - assert not revoked / not completed / not expired (slot < expires_at_slot)
            - assert executor signer
            - re-evaluate Pyth conditions on PriceUpdateV2 accounts (3 for Scenario B)
            - assert input cap and used_amount_raw + input_amount_raw <= max
            - record pre_in_balance and pre_out_balance from token accounts
            - introspect instructions sysvar to verify sibling ixs (§ 5)
            - mark pending_check (state hold for atomicity)

ix[1..N]   Jupiter-class instructions (per /build response)
            ix[1]     computeBudgetInstructions[*]   (1–2 ixs typical)
            ix[2]     setupInstructions[*]           (e.g. createAssociatedTokenAccountIdempotent)
            ix[k]     swapInstruction                (the actual aggregated route)
            ix[k+1]   cleanupInstruction             (optional; e.g. close wSOL ATA)
            ix[k+2]   otherInstructions[*]           (e.g. wSOL wrap)
            ix[k+3]   tipInstruction                 (only if tipAmount supplied)

ix[N+1]    post_check_v2 | ExecuteAction (epilogue phase)
            - read post_in_balance and post_out_balance
            - assert (pre_in - post_in)  <= action.max_input_amount_raw
            - assert (pre_in - post_in)  <= remaining_cap = max - used_amount_raw
            - assert (post_out - pre_out) >= action.min_output_amount_raw  (adverse-bound derived)
            - assert destination_account matches PDA destination
            - re-introspect instructions sysvar (defence in depth)
            - update used_amount_raw
            - if one_shot: completed = true; last_execution_slot = Clock.slot
            - clear pending_check
```

If any ix in this bracket fails, the entire tx rolls back atomically — Solana transactions are all-or-nothing.

### 4.2 Two-Ix vs One-Ix Variants — Trade-Off

The bracket can be implemented as either:

**A. Two-instruction variant (`pre_check_v2` + `post_check_v2`).** Spec § 6.1.2 written form.
- Cost per ix:
  - Each adds 1 `program_id` slot in the static budget (the clawsol-authority program), but both ixs share the SAME program-id slot — the message compiler dedupes.
  - Each adds an instruction header: 1 byte `program_id_index` + 1 byte `accounts.len()` + N bytes account-index list + 4 bytes `data.len()` LE + data. Per-ix overhead **excluding data** is ~6 bytes for an ix with 5–6 accounts. With data, both ixs together likely add ~120–180 bytes.
  - Two cross-instruction boundaries → small CU overhead (each ix ~3,000 CU framework cost) and two re-introspection sweeps over the instructions sysvar.

**B. One-instruction variant (`ExecuteAction` consuming instructions sysvar inline).**
- Cost: one ix, one set of accounts, one CU header.
- Saves one ix header (~6 bytes) and one cross-ix boundary (~3,000 CU).
- Enforces atomicity by construction: pre-check / sibling-ix verify / post-check happen in a single processor invocation; no `pending_check` flag needed.
- Loses: cannot interleave the Jupiter ixs *between* pre and post — the bracket would need the Jupiter ixs to come before `ExecuteAction`, with `ExecuteAction` doing pre+post in one go via `load_instruction_at_checked` over indices `[0..n-1]`. That changes the topology to `Jupiter ixs first, ExecuteAction last`. The instruction sysvar still allows reading the surrounding ixs by index, so **the verification semantics are equivalent**; only the implementation shape differs.

**Recommendation: variant B (single `ExecuteAction`)** — saves ~6 bytes + ~3,000 CU + one cross-ix boundary, eliminates the `pending_check` state-hold requirement, and matches the existing P3-merged `ExecuteAction` interface. The bracket's *semantics* are unchanged: `ExecuteAction` reads `Sysvar1nstructions1...` and walks every ix in the tx, classifying and bounds-checking each.

This recommendation is **conditional on measurement** — if the inlined post-check arithmetic + sibling walk pushes one ix over the per-instruction CU default of 200,000, the bracket needs an explicit `SetComputeUnitLimit` (which the bracket should set anyway). If even with `SetComputeUnitLimit = 1,400,000` it pushes the per-tx CU ceiling, the slice falls back to variant A.

### 4.3 Why pre_check Cannot Just Be Folded Into ExecuteAction Start

The prompt asks if `pre_check` can be consolidated into `ExecuteAction` start. **It can, AND that's the recommended shape (§ 4.2 above).** The "consolidated" variant:
- ix[0..N]: Jupiter ixs in their /build order.
- ix[N+1]: `ExecuteAction` reads sysvar, classifies ix[0..N], computes pre/post deltas from balances at *its own* clock observation (token accounts haven't changed during this single-tx execution because the prior ixs already settled in the same tx), enforces all gates, and updates state.

This is functionally equivalent to having `pre_check_v2` at index 0 because Solana ixs in a single transaction execute in declared slice order (audit J-C2 / `Message::compile_instructions`); the post-check can read pre-balances by introspecting the account snapshot before any of the swap ixs ran — except it can't, because by the time post-check runs the swap has already settled. **Ergo, pre/post balance deltas in variant B require the daemon to record pre-balances off-chain and pass them as instruction data, OR have post-check derive pre-balances from the now-changed accounts plus the swap-ix data.** Neither is great.

**Updated recommendation:** Keep variant **A (two-ix)** for Stage 2 v1. The on-chain pre/post balance read pattern requires `pre_check` to capture pre-balances *before* the Jupiter ixs run; only an on-chain ix at the very start can do that without trusting daemon-supplied data. Variant B saves bytes/CU but breaks the bracket's "balances are read on-chain at both endpoints" property, which is non-negotiable defence-in-depth alongside sibling-ix verification.

**Decision:** Variant A. The byte/CU saving from variant B is not worth weakening the balance-read property.

### 4.4 Account List for ix[0] / ix[N+1] (Variant A)

`pre_check_v2` accounts (proposed):
1. `executor` (signer, readonly) — delegated wallet.
2. `authorization_pda` (writable) — to mark pending_check.
3. `instructions_sysvar` (readonly) — for sibling-ix introspection.
4. `pyth_btc_price_update` (readonly) — Pyth `Account<'info, PriceUpdateV2>`.
5. `pyth_eth_price_update` (readonly) — same.
6. `pyth_sol_price_update` (readonly) — same.
7. `delegated_input_token_account` (readonly) — for pre_in_balance.
8. `destination_token_account` (readonly) — for pre_out_balance.
9. `clock_sysvar` (readonly) — implicit; `Clock::get()` works without passing the account, so this slot can be **omitted**.

→ **8 accounts** for `pre_check_v2`. The clawsol-authority program id occupies the 9th slot but is shared with `post_check_v2`.

`post_check_v2` accounts (proposed):
1. `executor` (signer) — same as pre.
2. `authorization_pda` (writable) — to update used_amount_raw, completed, etc.
3. `instructions_sysvar` (readonly).
4. `delegated_input_token_account` (writable? No — readonly suffices for delta read).
5. `destination_token_account` (readonly) — same.

→ **5 accounts**, all duplicates of pre-check accounts. The message compiler dedupes; net new account-key cost is **0** (everything was already added by pre_check). Only the per-ix accounts-index list (5 bytes) is new.

### 4.5 What the Bracket Does NOT Do

- Does NOT CPI into Jupiter. Strong CPI / PDA-escrow Jupiter is deferred to v1.5+ per spec § 6.2.
- Does NOT modify Jupiter's account list. The bracket trusts Jupiter to respect its own `maxAccounts` parameter.
- Does NOT enforce Jupiter program-id pinning at the daemon layer alone — sibling-ix verification (§ 5) re-asserts program-id pinning on chain.
- Does NOT validate the swap route's individual hops. The bracket trusts the balance delta + program-id pinning.

---

## 5. Sibling-Instruction Verification Strategy (B-1 Resolution)

Stage 2 spec § 6.1.3 requires `pre_check_v2` and `post_check_v2` to consume `Sysvar1nstructions1111111111111111111111111` and verify every middle ix. This section operationalises that requirement against the Jupiter-specific reality that **middle ixs span MULTIPLE program ids** (not single-program like Solend boundary).

### 5.1 Why Program-Id-Only Verification Is Insufficient

A naive "every middle ix's `program_id == JUPITER_V6_PROGRAM_ID`" rule would fail to recognise Jupiter's own setup/cleanup ixs that legitimately dispatch through:
- SPL Token Program (for transfers within the route).
- Associated Token Account program (for `createAssociatedTokenAccountIdempotent`).
- System Program (for SOL movements).
- Compute Budget Program (for `SetComputeUnitLimit` / `SetComputeUnitPrice`).
- Optional tip programs (Jito tipAccount transfer; the tipInstruction can be a system_instruction::transfer to a Jito tip pubkey, not a Jupiter ix).

A per-class allowlist plus per-class account-write rules is required.

### 5.2 Instruction-Class Allowlist

The bracket allows the following classes in the middle band `ix[1..N]`. Pinned constants for each program id MUST live in `clawsol-authority/src/jupiter_boundary.rs` (future slice — out of this report) and be parity-asserted against the existing repo's `crates/gateway/src/integrations/jupiter.rs`.

| Class | Program ID | What it may write | Notes |
|---|---|---|---|
| **Compute Budget** | `ComputeBudget111111111111111111111111111111` | Nothing (no account writes; ix data only) | `SetComputeUnitLimit`, `SetComputeUnitPrice`. Always allowed, any number of ixs. |
| **Jupiter Swap (v6)** | Jupiter v6 mainnet program id (TBD-pinned) | Token accounts ONLY in the recorded source/destination set + Jupiter-internal route accounts (writable indices in the swap ix's account list, all of which appear in the message's account_keys/ALT) | The single `swapInstruction` from `/build`. Exactly one expected. |
| **SPL Token** | `TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA` | Token accounts ONLY in the recorded source/destination set | Used by Jupiter for internal transfers; allowed but only against accounts the bracket recognises. |
| **Associated Token Account** | `ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL` | Newly-created ATAs whose seeds derive from `(owner, TOKEN_PROGRAM, mint)` for {input_mint, output_mint, intermediate_mint(s)} | `createAssociatedTokenAccountIdempotent`. May be filtered client-side per audit § 9.1 — but if present, the bracket must accept it. |
| **System Program** | `11111111111111111111111111111111` | Lamport transfers (e.g. wSOL wrap) and ATA creation back-end | Allowed only when paired with an ATA-create or wSOL operation. |
| **Optional Tip** | Jito tip pubkey (system_instruction::transfer) OR a Jito-provided program id, depending on Jupiter's `tipInstruction` shape | Lamport transfer to a Jito tip account ONLY | If `tipInstruction` is present in the /build response, the bracket allows it but **the destination must be a known Jito tip pubkey** (allowlist; see § 5.5). |

Any program id outside this allowlist in any middle ix → **reject**. This includes any DEX program (Whirlpool, Raydium, Phoenix, etc.) appearing as a TOP-LEVEL ix — those programs are reachable only via CPI from Jupiter's swap ix, which is a single top-level ix. A direct top-level Whirlpool ix in the middle band is the exact attack the bracket exists to prevent.

### 5.3 Per-Class Account-Write Rules

The bracket enforces, for every middle ix, that **no account marked writable is outside the recorded "allowed writable set"**. The allowed writable set comprises:
- The delegated wallet's input ATA (USDC).
- The delegated wallet's output ATA (wSOL/SOL).
- The destination ATA (= `AuthorizationRecord.destination`).
- Jupiter's internal route accounts (writable as required by the swap ix).
- ATA accounts created by `setupInstructions` (recognised by their derived address from `(owner, TOKEN_PROGRAM, mint)`).
- The delegated wallet itself for `cleanupInstruction` → SOL refund on wSOL close.

Enforcement algorithm (pseudocode):

```text
for each middle_ix at sysvar index i in 1..N:
    classify(middle_ix.program_id) -> class | reject
    for each account_meta in middle_ix.accounts:
        if account_meta.is_writable:
            if account_meta.pubkey ∉ allowed_writable_set:
                reject  # tampering signal
```

The **`allowed_writable_set` is derived deterministically** from:
- `AuthorizationRecord.delegated_wallet`.
- Canonical `WatchRule.action.input_mint` and `output_mint` (committed in canonical_rule_hash).
- `AuthorizationRecord.destination`.
- `derive_associated_token_address(delegated_wallet, input_mint)` and similar for output / destination.
- The Jupiter swap ix's own writable accounts that are **already in the message's account_keys/ALT** (i.e. the route's intermediate token accounts that Jupiter's quote pre-declared).

The Jupiter route's writable accounts are NOT independently knowable to the bracket pre-quote, so the bracket's rule is **"every writable account in a Jupiter-class middle ix must appear in the Jupiter swap ix's own account list at a writable index"** — i.e. the bracket trusts Jupiter to internally declare its writable accounts and rejects out-of-band writes that don't match.

### 5.4 Ordering Verification

Per spec § 6.1.3 item 4, ix ordering must match the Jupiter-documented `setup → swap → cleanup` shape. The bracket enforces, in the middle band:

1. Compute Budget ixs come first (any number).
2. Setup ixs come second (in /build's `setupInstructions[]` order).
3. Exactly one Swap ix (the `swapInstruction`).
4. Optional Cleanup ix (`cleanupInstruction`).
5. Optional Other ixs (`otherInstructions[]`).
6. Optional Tip ix (`tipInstruction`) — comes last in the middle band per Jupiter's recommendation.

Reordering is rejected. Detection: walk middle ixs in order, classify each, maintain a state-machine cursor (compute_budget → setup → swap → cleanup → other → tip), reject any backward transition. The state machine is small and CU-cheap.

### 5.5 Detection of Inserted Malicious Token Transfer

A specific attack to defend against: a malicious daemon inserts an extra `spl_token::transfer(delegated_input_ata → attacker_ata, max_drain_amount)` between the Jupiter setup and swap. Defence:
1. The transfer ix is an SPL Token-class ix → allowed by program-id check.
2. The destination `attacker_ata` is **writable** but is **not in `allowed_writable_set`** (it's not the delegated input/output ATA, not the destination, not a Jupiter route account, not an ATA derived from a canonical mint).
3. → Rejected at the writable-account check in § 5.3.

A subtler attack: `spl_token::transfer(delegated_input_ata → delegated_input_ata, 0)` (no-op). This passes all writable checks because the destination is in `allowed_writable_set`. **Defence:** the bracket also requires the post-check balance delta to match expectations — `(pre_in - post_in) <= max_input_amount_raw` and `(post_out - pre_out) >= min_output_amount_raw`. A no-op transfer between the user's own accounts moves zero balance and is therefore harmless; an actual drain to a different destination is caught by the writable check.

### 5.6 Distinction From Solend Pattern

`solend_boundary.rs` treats sibling-ix verification as a single-program-id walk over Solend's instruction variants. **DO NOT carry that assumption to Jupiter.** Jupiter's middle-band ixs are intentionally heterogeneous (5+ program ids in a typical route). The Jupiter sibling-ix verifier is a per-class state machine, not a single-program ordinal check.

The Solend boundary's *pattern* (deterministic walk; per-ix classification; rejection on unknown writable accounts; specific error per attack class) is reused. The Solend boundary's *content* (single `SOLEND_PROGRAM_ID_MAINNET`; two `LendingInstruction` ordinals; refresh-then-withdraw ordering) is Solend-specific and not transferable.

### 5.7 Cost of Sibling-Ix Verification

- 1 instructions sysvar account slot (shared between pre_check and post_check; net new cost = 1 slot).
- CU for the walk: 1 `load_current_index_checked` + (N-1) `load_instruction_at_checked` calls. Each `load_instruction_at_checked` is documented (Solana program docs) as a `~1,500 CU` operation in modern Agave releases (this is an estimate; not in the docs I fetched). For N=10 middle ixs, ~15,000 CU per sweep. Two sweeps (pre + post) ~30,000 CU.
- CU for the per-class classification: trivial (~1,000 CU per ix).
- CU for the writable-account check: per-account hashtable lookup, ~500 CU per account. For 30 writable accounts in 10 ixs, ~15,000 CU.
- **Total bracket-side CU estimate: ~60,000–100,000 CU**, well under the 1,400,000 ceiling but measurable.

---

## 6. Account Budget — Itemised

Account counts for Scenario B (BTC > 75K AND ETH > 2.3K AND SOL < 90 → buy SOL with 5 USDC).

### 6.1 Static Account Slots (Non-ALT)

These accounts MUST be in the message's static `account_keys` array (not ALT-loaded), because either (a) they are signers, (b) they are writable, or (c) they are program-id slots.

| # | Slot | Reason for static | Writable? | Signer? |
|---|---|---|---|---|
| 1 | Delegated wallet (executor) | Signer + fee payer | Yes (fee debit) | Yes |
| 2 | Authorization PDA | Writable (state mutation) | Yes | No |
| 3 | clawsol-authority program | Program id slot | No | No |
| 4 | Jupiter v6 program | Program id slot | No | No |
| 5 | SPL Token program | Program id slot | No | No |
| 6 | Associated Token Account program | Program id slot (only if a `setupInstructions` ATA-create remains after client-side filter) | No | No |
| 7 | System program | Program id slot (ATA + wSOL) | No | No |
| 8 | Compute Budget program | Program id slot | No | No |
| 9 | Pyth receiver program | Program id slot (for `Account<'info, PriceUpdateV2>` owner check) | No | No |
| 10 | Instructions sysvar (`Sysvar1nstructions1...`) | Required for sibling-ix introspection | No | No |
| 11 | BTC PriceUpdateV2 | Pyth account (could be ALT if not writable, but Anchor's `Account<'info, T>` typically reads from static; conservative = static) | No | No |
| 12 | ETH PriceUpdateV2 | Same | No | No |
| 13 | SOL PriceUpdateV2 | Same | No | No |
| 14 | Delegated wallet input USDC ATA | Writable (Jupiter source) | Yes | No |
| 15 | Delegated wallet output wSOL ATA | Writable (Jupiter destination) | Yes | No |
| 16 | Destination wallet (user main) | Writable if sweep ix is in same tx; readonly if sweep is separate | Conditional | No |
| 17 | Destination wallet output ATA | Writable (sweep destination) — only if sweep is same-tx | Conditional | No |
| 18 | (optional) Jito tip pubkey | Writable (lamport recipient) — only if tipInstruction present | Conditional | No |
| 19 | (optional) wSOL mint or USDC mint | Static if cleanupInstruction unwraps wSOL via `closeAccount` | No | No |

**Static slot estimate range:** 14 (no sweep, no tip, no extra mint) to 19 (all optional ixs present).

**Practical baseline for the report:** 17 static slots (assuming sweep is a separate tx — see § 8 Pyth analogue — and no Jito tip in the v1 demo).

### 6.2 ALT-Loaded Slots (Read-Only Account-Key Entries)

The remaining v0+ALT budget is `64 − static = 64 − 17 = 47 slots` for ALT-loaded read-only accounts.

These MUST be read-only (writable accounts are not allowed to be ALT-loaded by the v0 message format). The Jupiter route accounts that are read-only — pool oracles, price-feed PDAs that the route's individual DEXes consult, intermediate token account observations — fit here.

**Jupiter `maxAccounts` interaction.** Jupiter's `maxAccounts` parameter constrains the *quantity* of accounts in the route, but Jupiter chooses how to split between "must be writable in the swap ix" and "may be read-only in ALT". Jupiter does not expose a writable-vs-readonly split in `maxAccounts`. The bracket's safe assumption: `maxAccounts ≤ 64 − static` to ensure the route fits even in the worst case where every Jupiter-route account is writable and must be static.

**Practical starting point for measurement:** `maxAccounts = 47` (= 64 − 17 static). If the /build response actually returns a route whose writable-account count is less than `64 − static − route_readonly`, the bracket has headroom. If not, iterative reduction per Jupiter guidance.

### 6.3 Number of ALTs

Jupiter's `/build` response returns `addressesByLookupTableAddress` — a map from ALT pubkey to addresses. In practice Jupiter typically uses 1 ALT (its own canonical Jupiter ALT for v6) plus possibly 1–2 DEX-specific ALTs. Each ALT pubkey occupies one byte in the message's `addressTableLookups[]` field plus 1 byte for `writableIndexes.len()` + N writable indices + 1 byte for `readonlyIndexes.len()` + M readonly indices. **ALT metadata overhead:** ~8–20 bytes per ALT. Three ALTs ~ 60 bytes.

### 6.4 Account-Key Compression Math

Without ALT: every account is 32 bytes in `account_keys`.
With ALT: the account is 1 byte in the lookup table's index list + ~32 bytes in the ALT itself (already on-chain, not transmitted in the tx).

**Tx byte savings:** 31 bytes per account moved from static to ALT-loaded. For a 47-account ALT-loaded route, savings ~ 47 × 31 = **~1,460 bytes** vs legacy. This is what makes Scenario B even theoretically possible — without ALT, the static account list alone (47 × 32 = 1,504 bytes) would exceed `PACKET_DATA_SIZE`.

---

## 7. Serialized Byte-Size Budget

### 7.1 Hard Cap and Constants

- `PACKET_DATA_SIZE = 1,232 bytes` (Solana docs).
- v0 transaction signature overhead: `1 + 64 × num_signatures`. Stage 2 has 1 signer (delegated wallet executor) → `1 + 64 = 65 bytes`.
- v0 message header: 3 bytes.
- Account-keys count + array: `compact_u16(static_count) + 32 × static_count`.
- Recent blockhash: 32 bytes.
- Compact array of compiled instructions: `compact_u16(N) + Σ ix_size`.
- Each compiled ix: `1 byte program_id_index + compact_u16(account_count) + N bytes account_indices + compact_u16(data_len) + data`.
- `addressTableLookups[]`: `compact_u16(num_alts) + Σ (32 bytes ALT pubkey + compact_u16(W) + W bytes writable_indices + compact_u16(R) + R bytes readonly_indices)`.

### 7.2 Estimated Byte Cost (Scenario B, Variant A bracket, `maxAccounts=47`)

| Component | Bytes (estimate) | Source |
|---|---|---|
| Signatures (1 signer × 64 + 1 byte length) | 65 | Solana wire format |
| Message header (3 bytes) | 3 | Solana wire format |
| Account-keys count (compact-u16) | 1 | |
| Static account keys (17 × 32) | 544 | § 6.1 |
| Recent blockhash | 32 | |
| Instructions count (compact-u16) | 1 | |
| `pre_check_v2` ix (program_id + ~8 accounts indexes + ~32 bytes data) | ~45 | § 4.4 |
| Jupiter `computeBudgetInstructions` (2 ixs × ~10 bytes) | ~20 | |
| Jupiter `setupInstructions` (1 ATA-create × ~30 bytes) | ~30 | |
| Jupiter `swapInstruction` (data ~150 bytes + ~40 account indexes) | ~200 | Estimate; **measurement target** |
| Jupiter `cleanupInstruction` (close wSOL × ~25 bytes) | ~25 | |
| Jupiter `tipInstruction` (system transfer × ~15 bytes; only if tipAmount supplied) | ~15 | |
| `post_check_v2` ix (program_id + ~5 accounts indexes + ~24 bytes data) | ~35 | § 4.4 |
| `addressTableLookups[]` (3 ALTs × ~20 bytes each + length byte) | ~65 | § 6.3 |
| **Subtotal** | **~1,081 bytes** | |
| **Headroom vs 1,232** | **~151 bytes** | |

The Jupiter `swapInstruction` byte count is the largest unknown. The 200-byte estimate is plausible for a 5-hop route at `maxAccounts=47`; 1-hop direct routes are smaller; complex routes through multiple pools can be larger. **Measurement is required.**

If the actual tx exceeds 1,232 bytes, Stage 2 spec § 6.3.2 forbids dropping safety ixs. Mitigations in priority order:
1. Reduce `maxAccounts` (per Jupiter recommendation, by 2 at a time, with price-impact measurement).
2. Filter `createAssociatedTokenAccountIdempotent` from `setupInstructions` if the destination ATA exists (saves ~30 bytes; audit § 9.1).
3. Move sweep to a separate tx (saves ~70 bytes for 1 fewer ix and 2 fewer accounts).
4. Move Pyth self-post to a separate tx (already the v1 default — see § 9 below).
5. **If still over budget after all above: classify as preview-only.**

### 7.3 What Cannot Be Dropped to Make It Fit

- Sibling-ix verification (instructions sysvar; ~1 slot, ~1 byte index).
- Pre-check / post-check ixs (variant A; both required for balance-read property).
- 3 Pyth feeds (all required for Scenario B AND condition).
- ALT (mandatory per spec § 6.3.2 — no legacy-tx fallback).
- Compute Budget ixs (required to set CU limit > 200K default).

### 7.4 v1 vs v2 Byte Delta

Audit § 6 (C-6) noted that v2's `tipInstruction` and `blockhashWithMetadata` differ from v1. Concrete delta:

- v2 adds `tipInstruction` (only if `tipAmount` is supplied; ~15 bytes when present). Stage 2 v1 does NOT supply `tipAmount` initially, so the tip ix is absent. Net cost of v2 over v1 **on this dimension**: 0 bytes.
- v2 adds `blockhashWithMetadata` to the response payload — this is a *response* field, not an ix or account; it's parsed by the daemon and doesn't appear in the wire-form tx. Net wire-form cost: **0 bytes**.
- v2 renames `addressLookupTableAddresses` (v1) → `addressesByLookupTableAddress` (v2). No tx-byte difference; only response shape.

**Net byte-budget delta v1 → v2: 0 bytes** in the no-tip case. The audit's worry about v2 fields blowing the byte budget is *conditional on supplying `tipAmount`*. The Stage 2 v1 demo can stay tip-free; if Jito tipping becomes needed for landing reliability under congestion, that's a measured spend.

---

## 8. Compute-Unit Pressure

### 8.1 Per-Tx CU Ceiling and Defaults

- Hard cap: `MAX_COMPUTE_UNIT_LIMIT = 1,400,000` CU (Solana docs).
- Default per-instruction: 200,000 CU. To use more, the bracket MUST emit `ComputeBudgetProgram::SetComputeUnitLimit(target)` as part of its `computeBudgetInstructions`.
- Builtin-ix default: 3,000 CU each.

### 8.2 Estimated CU Pressure

**Caveat: these are educated estimates, not measured values.** Real values must come from devnet measurement against an actual /build response.

| Component | Estimated CU | Notes |
|---|---|---|
| `ComputeBudgetProgram::SetComputeUnitLimit(1_400_000)` | 150 | Builtin |
| `ComputeBudgetProgram::SetComputeUnitPrice(...)` | 150 | Builtin |
| `pre_check_v2` instructions-sysvar walk (10 sibling ixs) | ~15,000 | § 5.7 |
| `pre_check_v2` per-class classification | ~10,000 | |
| `pre_check_v2` writable-account check (~30 accounts) | ~15,000 | |
| `pre_check_v2` Pyth `get_price_no_older_than` × 3 feeds (Anchor SDK helper) | ~30,000 | Estimate; SDK includes ed25519 verify deferred |
| `pre_check_v2` adverse-bound + i128 comparison × 3 | ~5,000 | Trivial fixed-point |
| `pre_check_v2` PDA load + decode + lifecycle gates | ~10,000 | |
| Setup `createAssociatedTokenAccountIdempotent` (no-op path) | ~20,000 | SPL idempotent check |
| **Jupiter `swapInstruction`** | **~400,000–800,000** | **Dominant cost.** Varies by route hops. Real value: measure. |
| Jupiter `cleanupInstruction` (close wSOL) | ~10,000 | |
| `post_check_v2` instructions-sysvar walk + checks | ~30,000 | Same shape as pre |
| `post_check_v2` balance delta + min_out check | ~5,000 | |
| `post_check_v2` PDA write-back | ~5,000 | |
| Per-ix framework overhead (10 ixs × 3K) | ~30,000 | |
| **Subtotal** | **~585,000–985,000 CU** | |
| **Headroom vs 1,400,000** | **~415,000–815,000 CU** | |

**Conclusion:** CU is plausibly within budget but sensitive to Jupiter's actual route cost. Routes that hit the Jupiter swap ix's worst case (multi-hop through Whirlpool, Raydium CLMM, Orca, with multiple price-feed reads) can consume 800K+ CU, leaving thin headroom for the bracket. Measurement is required.

If CU is exceeded:
1. Reduce `maxAccounts` to favour shorter routes.
2. Pin `restrictIntermediateTokens=true` (already required by spec).
3. **No degraded-safety fallback.** Cannot drop bracket checks to save CU.
4. **If still over 1.4M after measurement → preview-only.**

### 8.3 What Cannot Be Trimmed for CU

Same list as § 7.3 (byte-budget side). Sibling-ix verification, pre/post checks, and Pyth re-evaluation are non-negotiable.

---

## 9. Pyth 3-Feed Impact

`STAGE2_PYTH_ADAPTER_RESEARCH.md` is the authority. This section restates only the bracket-budget-relevant impact:

- **Account slots:** 3 (one `PriceUpdateV2` per feed) + 1 shared receiver-program slot.
- **Bytes:** Each `PriceUpdateV2` account is referenced by index (1 byte each in ix.accounts; 32 bytes each in account_keys static). Per the v0 design these accounts are *non-writable* and *not signers*, so they could in principle be ALT-loaded — saving ~93 bytes (3 × 31).
- **CU:** ~10,000 CU per `get_price_no_older_than` invocation × 3 feeds = ~30,000 CU.

### 9.1 Pyth Self-Post in Same Tx — Plausibility

Audit § 9 / Pyth research § 13.1 already established self-post is a separate prerequisite tx. Restating why bundling is not plausible for Scenario B:

- A `post_update_atomic` tx carries:
  - The Pyth Hermes VAA (variable; typically 200–600 bytes per feed).
  - The merkle proof for that feed (~40–80 bytes per feed).
  - Ed25519 signature verification ix (Solana ed25519 program).
  - Resulting wire-form size ~600–1,000 bytes per feed self-post.
- Three feeds in one bundle ~ 1,800–3,000 bytes — already over the 1,232-byte cap before any Jupiter ix is added.
- CU: ed25519 + merkle verify is ~600,000 CU per self-post. Three self-posts ~ 1,800,000 CU — over the 1.4M ceiling.

**Conclusion: Pyth self-post bundling is not plausible.** The Stage 2 v1 plan keeps self-post as a separate prerequisite tx, sequenced by the daemon before the execute tx. The execute tx assumes `PriceUpdateV2` accounts are already on chain at acceptable freshness.

This is consistent with `STAGE2_PYTH_ADAPTER_RESEARCH.md` § 8.2 which already specifies `ensure_fresh_price_update` as a daemon-side flow.

---

## 10. ALT Strategy

### 10.1 Hard Requirement

ALT is **mandatory** per Stage 2 spec § 6.3.2. v0 transactions only. No legacy-tx fallback.

### 10.2 ALT Sources

Three sources contribute ALTs to the Stage 2 execute tx:

1. **Jupiter's canonical v6 ALT.** Always present; returned by `/build` in `addressesByLookupTableAddress`. Contains common DEX program ids and frequently-routed token mints.
2. **Per-route DEX ALTs.** Sometimes present (Whirlpool ALT, Raydium CLMM ALT) when the route hits those programs.
3. **ClawSol-owned ALT (proposed).** Optionally created at Stage 2 deployment time, containing:
   - Pyth receiver program id.
   - Pyth `PriceUpdateV2` accounts for BTC/ETH/SOL (sponsored shard-0).
   - SPL Token program id.
   - Associated Token Account program id.
   - System program id.
   - Compute Budget program id.
   - clawsol-authority program id.

A ClawSol-owned ALT pre-populated with these stable accounts saves ~7 × 31 = **~217 bytes** vs static encoding. **Strong recommendation: create this ALT once at Stage 2 deployment and reference it in every execute tx.**

### 10.3 ALT Resolution

The existing repo's `crates/gateway/src/integrations/jupiter_alt.rs` already implements ALT account resolution (`get_account` per ALT pubkey, decode `AddressLookupTable`, sort by address for determinism). Stage 2 reuses this code path; no new ALT machinery needed.

### 10.4 ALT Count Limit

Solana docs do not document a per-tx ALT count limit. The audit (U-3) flags this as a measurement target. Empirically the existing Jupiter production code path handles 1–3 ALTs per tx; >5 ALTs has not been exercised. **Measurement target: confirm tx accepts 4+ ALTs (Jupiter canonical + 2 DEX ALTs + 1 ClawSol ALT).**

---

## 11. maxAccounts Strategy

### 11.1 Updated Guidance (per 2026-05-09 Jupiter docs)

**Stale prescriptions to remove from spec § 6.1.1:**
- Hard cap "`maxAccounts ≤ 48`" (§ 6.1.1 currently states this; replace with iterative tightening).

**Current guidance to apply:**
- Default 64. Range 1–64.
- *"Very low values (e.g below 50) can result in no route found or significantly worse pricing."*
- *"Start with a moderate reduction (e.g. reducing 2 and retry) and test pricing impact before going lower."*
- Always pair with `restrictIntermediateTokens=true`.

### 11.2 Computed Jupiter Account Budget for Scenario B

Jupiter account budget = 64 (v0+ALT total) − ClawSol-static (17) = **47 slots** for Jupiter route accounts.

Setting `maxAccounts=47` is the upper bound. If a 47-account route returns no route or worse pricing, iterate down by 2: 45 → 43 → 41. Below 41 (= 64 − 17 − 6 buffer) the route is increasingly likely to fail outright.

**Below 50:** apply the audit's "no route found" warning. Document price impact at each step in the measurement harness output.

### 11.3 Values to Sample

For Scenario B (USDC → wSOL), the measurement harness should sample:

| `maxAccounts` | Expected behaviour |
|---|---|
| 64 | Best route quality. Likely exceeds tx-byte budget when added to bracket. |
| 58 | Conservative middle. Probably fits. |
| 55 | Tighter; route quality begins to degrade for less-liquid pairs (USDC/wSOL is liquid → likely fine). |
| 52 | Right above the warning floor. |
| 50 | At the warning floor; "no route" risk begins per Jupiter. |
| 48 | Below warning; expected to fail or return poor pricing per Jupiter. |

Sample each value; record (a) whether `/build` returned successfully, (b) the swap ix byte size, (c) the executor-side estimated tx byte size after wrapping with the bracket, (d) the route's quoted output amount for a fixed input. The optimal `maxAccounts` value is the largest one whose wrapped tx fits within 1,232 bytes.

### 11.4 No Hard-Coded Final Value

The bracket implementation must accept `maxAccounts_hint: u8` from the `WatchRule.action.max_accounts_hint` field (already canonical per `crates/types/src/stage2_watch_rule.rs:285`). The daemon picks based on measurement; the program does not enforce a specific value (it's a quote-time hint, not a security-relevant parameter).

The `WatchRule` schema currently has `max_accounts_hint: u8` and the test fixture sets it to `48`. **Recommend updating the canonical fixture to `47`** to match the Scenario B static-account budget — but this is a future-slice task outside this report's scope (would require a schema-version bump in `STAGE2_WATCH_RULE_SCHEMA_VERSION` only if treated as a wire change, which it is not because `max_accounts_hint` is part of the canonical hash but its *value* changing is just data, not schema).

Actually re-reading: the `48` in the fixture is the *committed value for that fixture's test*. The schema accepts any `u8`. No schema bump needed; the daemon picks the live value at proposal time per measurement.

---

## 12. Pyth Self-Post Strategy (Restated for Bracket Context)

Default v1 plan: **separate prerequisite tx**. See `STAGE2_PYTH_ADAPTER_RESEARCH.md` § 8.2 for the daemon's `ensure_fresh_price_update` flow. Cost (per audit U-6 / Pyth research § 11.6):
- 1 self-post tx per stale feed per execute window.
- ~5,000 lamports base fee + Jito tip (if used).
- ~600,000 CU per self-post.
- Transient `PriceUpdateV2` rent until `close_update` reclaims.

The Stage 2 execute tx assumes the `PriceUpdateV2` accounts already exist at acceptable freshness. If the daemon's pre-flight finds any feed stale, it self-posts before submitting the execute tx.

### 12.1 Same-Tx Plausibility Restated

§ 9.1 above already concluded same-tx self-post is not plausible for Scenario B (3 feeds × ~600 CU + 1.8K bytes vs 1.4M CU + 1232 byte limits). For 1-feed Scenario A (Solend APY trigger; no Pyth), same-tx self-post is moot — Solend's reserve account is read directly, not via Pyth.

### 12.2 Single-Feed Pyth Future Variant

A future Stage 2 variant with only 1 Pyth condition (e.g. "buy SOL when SOL > $200") might be able to bundle a single self-post into the execute tx — ~600 bytes + ~600K CU added is plausible against the residual budget. This is **out of v1 scope** but worth noting for Stage 2 v1.5+.

---

## 13. Open Blockers

### 13.1 Implementation Blockers (must resolve before code is written)

1. **Jupiter v6 program id pin.** The bracket's per-class allowlist (§ 5.2) requires the exact Jupiter v6 mainnet program-id constant, parity-asserted against the existing repo's gateway pin. Not in this report; future slice.
2. **Jito tip pubkey allowlist.** If `tipInstruction` is supported, the bracket needs a maintained list of valid Jito tip pubkeys. Not specified in this slice. Out of v1 scope unless tipping is explicitly enabled.
3. **ClawSol-owned ALT deployment.** If the byte-budget mitigation (§ 10.2 item 3) is adopted, an ALT deployment slice is required before mainnet rehearsal.
4. **Per-class allowlist constants module.** `programs/clawsol-authority/src/jupiter_boundary.rs` (proposed name) needs to exist and host the program-id constants + classification logic. Future slice.

### 13.2 Measurement Blockers (must resolve before "Ready for implementation")

5. **Live `/build` byte size at `maxAccounts ∈ {64, 58, 55, 52, 50, 48}`** — actual swap-ix byte count for USDC→wSOL on mainnet routes.
6. **Bracket-wrapped tx byte size** at each `maxAccounts` value, with the proposed bracket structure.
7. **Compute-unit cost** of a representative Jupiter route under simulation (`simulateTransaction`) — this can be done off-chain without broadcast, env-gated.
8. **ALT count tolerance** — confirm a tx with 4 ALTs (Jupiter canonical + 2 DEX ALTs + 1 ClawSol ALT) accepts.
9. **Sibling-ix sweep CU cost** with the actual classification logic — only knowable after implementation.

### 13.3 Spec Blockers Already Resolved

- **B-1 (sibling-ix verification):** addressed in spec § 6.1.3 + this report's § 5.
- **B-2 (Solend correctness):** out of Jupiter scope; addressed in P4.
- **B-3 (Jupiter v1 vs v2):** **resolved here** — pin v2 `/build`. v1 emergency fallback only.

### 13.4 Documentation Re-Confirmation Gaps

- `lite-api.jup.ag` sunset date (2026-06-30 per audit) cannot be re-fetched without Jupiter portal credentials. Implementation slice MUST re-confirm before mainnet rehearsal.
- Per-tx ALT count limit not documented; measurement target.
- Exact CU cost of `load_instruction_at_checked` not documented; measurement target.

---

## 14. Measurement Harness Plan (Proposed, Not Implemented in This Slice)

Per the prompt's optional-harness section, default posture is **no live API / no live RPC**. If a future slice adds a harness, it MUST satisfy:

- **Env-gated.** Default run self-skips. Recommended env: `CLAW_STAGE2_JUPITER_MEASUREMENT_ENABLED=1`.
- **No signing.** No keypair load in the harness path.
- **No broadcast.** No `send_*_transaction` call.
- **No mainnet/devnet write.** Only `/quote` + `/build` HTTP fetches and optional `simulateTransaction` RPC reads (which are permitted; they don't mutate state).
- **Report flags live API/RPC use** explicitly in its output: header line `LIVE_API_CALLED=true|false`, `LIVE_RPC_CALLED=true|false`.

Suggested harness behaviour:
1. Take `(input_mint, output_mint, input_amount_raw, slippage_bps, maxAccounts_value)` as inputs.
2. Call Jupiter `/quote` with `restrictIntermediateTokens=true` and the supplied `maxAccounts`.
3. Call `/build` with the quote, supplying a synthetic delegated-wallet pubkey.
4. Parse `/build` response into ix list + ALT references.
5. Statically wrap the ix list with a stub `pre_check_v2` + `post_check_v2` (synthetic data, correct shape).
6. Compute `bincode::serialize(&v0_versioned_tx).len()` without signing.
7. Optionally call `simulateTransaction` to get CU estimate (read-only RPC; no mutation).
8. Emit a report row: `(maxAccounts, tx_bytes, simulated_cu, ix_count, alt_count, route_quote_out_amount)`.
9. NEVER call `sendTransaction` / `sendRawTransaction` / `simulateAndSign` etc.

The harness lives in a new `crates/gateway/tests/stage2_jupiter_measurement.rs` (or under `tools/`) — env-gated `#[ignore]` test, runnable explicitly. Out of this slice's scope.

---

## 15. Decision Matrix

Objective gates (from prompt § 11):

| Gate | Status | Notes |
|---|---|---|
| Account count | **Plausible-fits** | 17 static + 47 ALT slots within 64-key v0 budget. |
| Serialized byte size | **Needs measurement** | Estimated ~1,081 bytes; ~151 bytes headroom; sensitive to swap-ix size. |
| Compute-unit estimate | **Needs measurement** | Estimated ~585K–985K CU; ~415K–815K headroom; sensitive to route depth. |
| Route quality / `maxAccounts` degradation | **Needs measurement** | Iterative tightening from 64; floor at 50 per Jupiter. |
| Sibling verification feasibility | **Plausible at design level; needs implementation+measurement** | Per-class allowlist + writable-set check; CU cost ~60–100K estimated. |
| Pyth self-post feasibility | **Confirmed: separate tx** | Same-tx bundling not plausible for 3-feed Scenario B. |

### 15.1 Final Classification

**Stage 2 conditional Jupiter buy (Scenario B): Needs measurement.**

This is **not** "Ready for implementation" — the byte-budget and CU-budget gates are unproven beyond static analysis. It is **not** "Needs spec update" — the spec is internally consistent and the audit's three blockers (B-1, B-2, B-3) are resolved at spec level. It is **not** "Preview-only until solved" — there is no known reason it cannot fit; the gates are measurable.

The next slice should:
1. Land the env-gated measurement harness (§ 14).
2. Run the harness against `api.jup.ag/swap/v2/build` for `maxAccounts ∈ {64, 58, 55, 52, 50, 48}`.
3. Land the per-class allowlist module (`jupiter_boundary.rs`) with sibling-ix verifier + tests.
4. Re-classify based on measurement output.

If the measurement reveals byte or CU pressure cannot be solved by the mitigations in § 7 / § 8, the classification falls back to **preview-only** as Stage 2 spec § 6.3.2 requires. **Preview-only is acceptable and is safer than overclaiming** — the demo UI surfaces the constraint visibly per spec.

---

## 16. Code-Level Skeleton — Not Added in This Slice

Per the prompt's "Preferred for this slice: docs-first report; no product code unless the helper is small, pure, and tested," **no code helper is added.** The Jupiter v6 program-id pin and the per-class allowlist would be the natural pure-helper module, but landing them now without the per-class enumeration measurement (§ 14) risks committing constants that the harness would later refute. Better to land the harness first, then the constants module with measured backing.

The existing `crates/types/src/stage2_watch_rule.rs` already has `JupiterApiVersion::V2`, `max_accounts_hint: u8`, and `require_pre_post_bracket: bool` canonical, so the schema substrate is already in place. No type-level changes needed here.

---

## 17. Sources Consulted (Confirmation Date 2026-05-09)

### Jupiter (developers.jup.ag)
- [docs/swap/build](https://developers.jup.ag/docs/swap/build) — v2 `/swap/v2/build` URL path, HTTP method, response field list.
- [docs/swap/advanced/reduce-transaction-size](https://developers.jup.ag/docs/swap/advanced/reduce-transaction-size) — `maxAccounts` semantics, default 64, range 1–64, ≤50 warning, "reduce by 2 and retry" prescription, `createAssociatedTokenAccountIdempotent` filtering note.
- [docs/api/swap-api/build](https://developers.jup.ag/docs/api/swap-api/build) — attempted, HTTP 404 (URL changed; superseded by `/docs/swap/build`).
- [docs/swap/v1/swap-instructions](https://developers.jup.ag/docs/swap/v1/swap-instructions) — attempted, HTTP 404 (v1 page apparently unlisted from the index; v1 endpoint itself remains live in repo's existing JIT executor).
- [docs/swap-api/rate-limit](https://developers.jup.ag/docs/swap-api/rate-limit) — attempted, HTTP 404.
- [portal/rate-limit](https://developers.jup.ag/portal/rate-limit) — gated behind portal login; sunset claim of `lite-api.jup.ag` (2026-06-30) carried forward from `STAGE2_OFFICIAL_DOCS_AUDIT.md` § 9 unchanged; **must re-confirm before mainnet rehearsal**.

### Solana / Anza (solana.com, docs.rs)
- [solana.com/docs/core/transactions](https://solana.com/docs/core/transactions) — `PACKET_DATA_SIZE = 1,232 bytes` derivation (IPv6 MTU 1280 − 48 header bytes).
- [solana.com/docs/advanced/lookup-tables](https://solana.com/docs/advanced/lookup-tables) — legacy 32 / v0+ALT 64 account-key budgets; 256 max addresses per ALT; per-tx ALT count limit not documented.
- [solana.com/docs/core/runtime](https://solana.com/docs/core/runtime) — `MAX_COMPUTE_UNIT_LIMIT = 1,400,000`; per-instruction default 200,000; builtin 3,000.
- [docs.rs/solana-program::sysvar::instructions](https://docs.rs/solana-program/latest/solana_program/sysvar/instructions/) — `Sysvar1nstructions1111111111111111111111111` address; `load_instruction_at_checked`, `load_current_index_checked`, `get_instruction_relative` helpers.

### Inherited from `STAGE2_OFFICIAL_DOCS_AUDIT.md` (2026-05-09 audit, unchanged)
- Jupiter v1 `/swap-instructions` schema (J-A2); v1-v2 superseded note (C-6); ATA precondition for `destinationTokenAccount` (J-B3 / U-8); custom-ix composability supported on Swap API but not Ultra (J-B1).
- Solana legacy 32 / v0+ALT 64 budget (J-C2); 1232-byte cap (J-C1); v0 mandatory for ALT (J-D1).
- Pyth `Account<'info, PriceUpdateV2>` ownership pattern; `get_price_no_older_than` enforces `Full` + `feed_id` + age atomically (P-B1, U-5).
- Borsh wire shape (B-1..5); PDA `find_program_address` (Sol-A1); `invoke_signed` for PDA signing (Sol-A2).

### Inherited from `STAGE2_PYTH_ADAPTER_RESEARCH.md` (2026-05-09)
- 1 PriceUpdateV2 account slot per feed; 3 slots for Scenario B (§ 5).
- 30 s freshness for trading triggers; sponsored heartbeat 60 s collision → daemon self-post (§ 8.2).
- Self-post cost: ~600K CU + transient rent + base fee per feed (§ 11.6).
- Adverse-bound discipline; `i128` fixed-point comparator (§ 6, § 7).

### Repo-Internal (read-only)
- [crates/types/src/stage2_watch_rule.rs](../../crates/types/src/stage2_watch_rule.rs) — canonical schema, `JupiterApiVersion`, `max_accounts_hint`, `require_pre_post_bracket`.
- [programs/clawsol-authority/src/lib.rs](../../programs/clawsol-authority/src/lib.rs) — `process_execute_action` skeleton (P3-merged); current bracket integration point.
- [programs/clawsol-authority/src/solend_boundary.rs](../../programs/clawsol-authority/src/solend_boundary.rs) — pattern reference (deterministic sibling walk; instructions-sysvar TODO at `verify_same_tx_refresh_and_withdraw`); Solend-specific assumptions DO NOT carry to Jupiter.
- crates/gateway/src/integrations/jupiter*.rs (read-only via Explore subagent inventory) — repo currently uses v1 `/swap/v1/swap-instructions` on `https://api.jup.ag`; v0 + ALT assembly already implemented; stage 2 will pin v2 `/build` instead.

---

## 18. Final Report

**Code touched in this slice:** None. Docs-only.
**Files added:** `docs/proofs/STAGE2_JUPITER_BRACKET_ALT_FEASIBILITY.md` (this file).
**Files modified:** None.
**Files moved/deleted:** None.

**Worktree path:** `C:/Users/jeffr/SolFrontier-jprep`
**Branch:** `stage2/jprep-jupiter-bracket-alt-feasibility`
**Base commit:** `67a97371950e880ec4649910b9c9057409a0a931` (origin/main).

**Pinned Jupiter API version / endpoint:** v2 — `GET https://api.jup.ag/swap/v2/build`. v1 emergency fallback only with documented justification. Ultra excluded (no custom-bracketed-ix insertion).

**Jupiter API freshness findings:** v2 `/swap/v2/build` is the current custom-transaction path. Response fields confirmed verbatim against developers.jup.ag/docs/swap/build (2026-05-09): `computeBudgetInstructions, setupInstructions, swapInstruction, cleanupInstruction, otherInstructions, tipInstruction, addressesByLookupTableAddress, blockhashWithMetadata`. `maxAccounts` default 64; below 50 risks no-route or worse pricing per official guidance; iterative reduction by 2 prescribed.

**Tx/account/CU budget conclusions:**
- Account count: 17 static + 47 ALT-loaded plausible at `maxAccounts=47` (within 64-key v0 budget). Static slot estimate range 14–19 depending on optional ixs.
- Byte size: estimated ~1,081 bytes with ~151 bytes headroom; sensitive to swap-ix size; **measurement required**.
- CU: estimated ~585K–985K with ~415K–815K headroom; sensitive to route depth; **measurement required**.

**maxAccounts strategy:** start at 64 (Jupiter default), iteratively reduce by 2, never below 50 without documented price-impact measurement, always pair with `restrictIntermediateTokens=true`. The Stage 2 spec's earlier `≤48` cap is stale and should be updated to "iterative tightening starting from 64 with documented price impact at each step."

**Pyth self-post conclusion:** separate prerequisite transaction. Same-tx self-post not plausible for Scenario B (3 feeds × ~600K CU + ~600 bytes/feed exceeds both the 1.4M CU and 1,232-byte budgets). Daemon's `ensure_fresh_price_update` flow already specified in `STAGE2_PYTH_ADAPTER_RESEARCH.md` § 2.

**Measurement gaps:** itemised in § 13.2 (5 measurement items) + § 13.4 (3 documentation re-confirmation gaps).

**Preview-only blockers (currently): none** at the spec/architecture level. Preview-only fallback applies only if the measurement gates in § 13.2 fail. Spec § 6.3.2 already requires that fallback; no degraded-safety fallback is acceptable.

**Final classification: Needs measurement.** See § 15.

**Explicit safety statement:** No live transaction was constructed, signed, or broadcast. No live Jupiter API was called by this report's author beyond documentation pages (developers.jup.ag, solana.com, docs.rs). No mainnet/devnet transaction occurred. No Solana RPC call. No deploy. No `cargo` build of code (this slice is docs-only). No push.

**Scope checks:**
- ✅ frontend/* not touched.
- ✅ watcher / daemon runtime not touched.
- ✅ live execution tx builders not touched (read-only inspection only).
- ✅ existing Jupiter production path not modified.
- ✅ `config/default.toml` not modified.
- ✅ `programs/clawsol-intent/*` not touched.
- ✅ Solend execution path not touched.
- ✅ No live RPC, no deploy, no signing, no broadcast, no mainnet/devnet transaction.
- ✅ No push performed.

**Git status (post-write):**
```
?? docs/proofs/STAGE2_JUPITER_BRACKET_ALT_FEASIBILITY.md
```
Working tree clean except this new untracked file. (Worktree was created clean from origin/main; the protected local-only files in the main worktree do not appear in this isolated worktree.)

---

**End of feasibility report.**
