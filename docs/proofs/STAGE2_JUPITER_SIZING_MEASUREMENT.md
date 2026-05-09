# Stage 2 — Jupiter V2 Sizing Measurement Harness

**Status:** Harness landed. Default-skip. Live measurement runs only when
`CLAW_STAGE2_JUPITER_SIZING=1` is set. **No measurement was run as part of
this slice** — the `docs/proofs/stage2_jupiter_sizing_fixtures.json` file
is generated only by an opt-in run.

**Date:** 2026-05-09.
**Branch:** `stage2/i3-jupiter-v2-sizing-harness`
**Predecessor:** `stage2/jprep-jupiter-bracket-alt-feasibility` (J-prep
feasibility report at `docs/proofs/STAGE2_JUPITER_BRACKET_ALT_FEASIBILITY.md`).
**Companion test file:**
[`crates/gateway/tests/live_jupiter_stage2_sizing.rs`](../../crates/gateway/tests/live_jupiter_stage2_sizing.rs).

This slice is **measurement infrastructure only**. It does not change the
production Jupiter execution path, does not implement Stage 2 conditional
Jupiter execution, does not touch the watcher, the daemon runtime, the
`programs/clawsol-authority` boundary, the frontend, or
`config/default.toml`.

---

## 1. Purpose

The J-prep feasibility report
([`STAGE2_JUPITER_BRACKET_ALT_FEASIBILITY.md`](STAGE2_JUPITER_BRACKET_ALT_FEASIBILITY.md))
classified the Stage 2 conditional Jupiter buy as **Needs Measurement**, with
four open measurement gates (§ 13.2):

5. Live `/build` byte size at `maxAccounts ∈ {64, 58, 55, 52, 50, 48}`.
6. Bracket-wrapped tx byte size at each `maxAccounts` value.
7. Compute-unit cost of a representative Jupiter route.
8. ALT count tolerance.

This harness directly answers gates 5, 6, and 8 (byte size and ALT shape).
Gate 7 (compute-unit cost) is intentionally deferred — it requires a live
`simulateTransaction` call which this slice excludes per the
`FETCH_BUILD_ONLY` invariant.

---

## 2. Safety Contract

Hard-coded into the harness header and echoed at every run start / row /
end:

| Marker | Meaning |
|---|---|
| `NO_SIGNING` | No keypair is loaded by the test. No `Keypair::new`, no `signing.rs`. |
| `NO_BROADCAST` | No `sendTransaction` / `sendRawTransaction` is called. |
| `NO_TRANSACTION_SUBMISSION` | No mainnet/devnet write of any kind. |
| `FETCH_BUILD_ONLY` | The only live HTTP call is `GET /swap/v2/build`. No simulate, no RPC, no Solana cluster traffic. |

The harness uses `Pubkey::new_unique()` for the synthetic delegated wallet
that becomes the Jupiter `taker` query parameter. That key has no funds, no
signing material, and never leaves the test process. Jupiter's `/build`
endpoint accepts unfunded `taker`s — it constructs the route assuming the
wallet exists; it does not validate balance.

---

## 3. Default Behaviour (no env)

```bash
cargo test -p claw-gateway --test live_jupiter_stage2_sizing -- --nocapture
```

The test self-skips and prints:

```
SKIP: set CLAW_STAGE2_JUPITER_SIZING=1 to run the Stage 2 Jupiter V2 sizing
harness. Default behaviour: self-skip and pass. The harness only calls
GET /swap/v2/build (no signing, no broadcast, no transaction submission).
```

Three pure-function helper unit tests inside the same file run regardless
(they exercise bracket synthesis on hand-built `V2BuildResponse` fixtures
and the byte-classification thresholds). They make no network calls.

CI does NOT set `CLAW_STAGE2_JUPITER_SIZING`; deterministic pipelines are
unaffected.

---

## 4. To Run the Live Measurement

```bash
CLAW_STAGE2_JUPITER_SIZING=1 \
  CLAW_JUPITER_BASE_URL=https://api.jup.ag \
  cargo test -p claw-gateway --test live_jupiter_stage2_sizing -- --nocapture
```

Optional overrides (all default to the prompt's baseline scenario, USDC →
wSOL, 5 USDC, 50 bps slippage, `restrictIntermediateTokens=true`):

| Env var | Default | Notes |
|---|---|---|
| `CLAW_JUPITER_BASE_URL` | `https://api.jup.ag` | The host receiving the `GET /swap/v2/build` call. |
| `CLAW_JUPITER_API_KEY` | _(unset)_ | Forwarded as `x-api-key` if set. Some Jupiter API tiers require this; the public tier does not. |
| `CLAW_STAGE2_JUPITER_INPUT_MINT` | USDC mint | base58 mint address. |
| `CLAW_STAGE2_JUPITER_OUTPUT_MINT` | wSOL mint | base58 mint address. |
| `CLAW_STAGE2_JUPITER_AMOUNT_RAW` | `5000000` | Smallest-unit input amount (5 USDC at 1e6). |
| `CLAW_STAGE2_JUPITER_SLIPPAGE_BPS` | `50` | 0.5 % slippage tolerance. |

The harness is hard-coded against the matrix `[64, 58, 55, 52, 50, 48]` per
the J-prep report's § 11.3 sample list. Each value is probed independently;
if Jupiter returns "no route" or HTTP 4xx for one value, the others are
still attempted and the row is recorded with `classification = NoRoute` or
`Error`.

---

## 5. Bracket Model (Variant A, two-instruction)

The synthetic Stage 2 bracket wraps Jupiter's instruction list as:

```
ix[0]      pre_check_v2          (synthetic clawsol-authority program id)
ix[1..N]   Jupiter ixs           (compute_budget → setup → swap → cleanup → other → tip)
ix[N+1]    post_check_v2         (synthetic clawsol-authority program id)
```

The bracket's account list mirrors J-prep § 4.4:

**`pre_check_v2` (9 accounts, 32-byte placeholder data):**
1. delegated_wallet *(signer, readonly — fee payer is the same key)*
2. authorization_pda *(writable)*
3. instructions_sysvar *(readonly — `Sysvar1nstructions1...`)*
4. pyth_btc_price_update *(readonly)*
5. pyth_eth_price_update *(readonly)*
6. pyth_sol_price_update *(readonly)*
7. delegated_input_token_account *(readonly)*
8. delegated_output_token_account *(readonly)*
9. pyth_receiver_program *(readonly — Pyth Pull oracle program id)*

**`post_check_v2` (5 accounts, 24-byte placeholder data):**
1. delegated_wallet *(signer, readonly)*
2. authorization_pda *(writable)*
3. instructions_sysvar *(readonly)*
4. delegated_input_token_account *(readonly)*
5. destination_token_account *(writable)*

**Variant choice — A, not B.** J-prep § 4.3 explicitly settled on
Variant A because the on-chain pre/post balance read property requires the
pre-check to capture pre-balances *before* the Jupiter ixs run; Variant B
saves bytes/CU but breaks that defence-in-depth property. The harness
follows § 4.3's decision.

**Why placeholder data sizes are conservative.** The on-chain program is
not yet implemented. The 32 / 24 byte sizes derive from J-prep § 7.2's
budget table (rule_id seed, max_input cap, slot bound — typical Anchor
ix data with `#[derive(BorshDeserialize)]`). Once the real bracket lands
in `programs/clawsol-authority/src/jupiter_boundary.rs`, this harness
should be re-run and the report updated.

---

## 6. ALT Modeling

The live Jupiter v2 `/build` response returns
`addressesByLookupTableAddress: null` — the addresses inside each ALT must
be resolved via Solana RPC (per `crates/gateway/src/integrations/jupiter.rs:13`
docstring). This harness intentionally skips that RPC step
(`FETCH_BUILD_ONLY`) and instead synthesizes ALT contents:

For each ALT pubkey returned in
`addressLookupTableAddresses`, the harness creates an
`AddressLookupTableAccount` whose `addresses` vector contains every account
referenced anywhere in the Jupiter ix list. The
`solana_sdk::message::v0::Message::try_compile` compiler then independently
chooses which accounts go in the static `account_keys[]` (signers and all
writable accounts) and which are ALT-loaded (read-only non-signers found
in any of the supplied ALTs).

**Why this is a faithful byte estimate.** The wire-form bytes for an ALT
lookup are:

```
+ compact_u16 lookup_count
+ Σ (32 bytes ALT pubkey + compact_u16 W + W bytes writable_indices
     + compact_u16 R + R bytes readonly_indices)
```

The `addresses` array of each ALT is **not** transmitted — it lives on
chain. The wire-form bytes only depend on (a) how many ALTs the message
references and (b) how many writable / readonly indices are loaded from
each. By over-populating each synthetic ALT with the full account set,
the compiler is free to pick optimally; the resulting per-ALT
`writable_indexes.len() + readonly_indexes.len()` reflects what an
ideally-resolved live ALT would offer.

**Caveat.** If Jupiter's actual on-chain ALTs do *not* contain a particular
read-only account that the harness's synthetic ALT does, the live tx would
have to put that account in the static slot list, costing 31 bytes per
account. The harness's byte estimate is therefore a **lower bound**.
A future env-gated slice (out of this slice's scope) can resolve real ALT
contents via RPC and tighten the estimate.

---

## 7. Classification Thresholds

| Bucket | Bytes | Meaning |
|---|---|---|
| `Green` | ≤ 1100 | Comfortable headroom (≥ 132 bytes / ~10 % under the 1232 cap). |
| `Yellow` | 1101–1232 | Within the cap but tight headroom. Implementation is feasible but should keep `maxAccounts` near the lower bound proven here. |
| `Red` | > 1232 | Over the wire cap. Stage 2 spec § 6.3.2 forbids dropping safety ixs; this `maxAccounts` value is not implementable for Scenario B and falls back to preview-only. |
| `NoRoute` | n/a | Jupiter declined to build a route at this `maxAccounts` value (per J-prep § 11.1, expected at very low values). |
| `Error` | n/a | HTTP transport failure, parse failure, or non-route 4xx. The row records the message verbatim. |
| `Unknown` | n/a | Reserved for compute-unit gates that will arrive in a future slice (currently no row uses this state). |

---

## 8. Output Artifact

When the live measurement runs, the harness writes
[`stage2_jupiter_sizing_fixtures.json`](stage2_jupiter_sizing_fixtures.json)
relative to this directory. Schema:

```jsonc
{
  "generated_at": "RFC3339 timestamp at run time",
  "official_docs_checked_at": "2026-05-09",
  "jupiter_base_url": "https://api.jup.ag",
  "scenario": {
    "input_mint": "...",
    "output_mint": "...",
    "amount_raw": 5000000,
    "slippage_bps": 50,
    "restrict_intermediate_tokens": true
  },
  "env_used": {
    "sizing_enabled": true,
    "api_key_supplied": false,
    "base_url": "https://api.jup.ag",
    "input_mint_overridden": false,
    "output_mint_overridden": false,
    "amount_overridden": false,
    "slippage_overridden": false
  },
  "rows": [
    {
      "max_accounts": 64,
      "restrict_intermediate_tokens": true,
      "route_label": null,
      "setup_instructions_count": 0,
      "compute_budget_instructions_count": 0,
      "swap_instruction_present": false,
      "cleanup_instruction_present": false,
      "other_instructions_count": 0,
      "tip_instruction_present": false,
      "blockhash_with_metadata_present": false,
      "address_lookup_tables_count": 0,
      "alt_loaded_addresses_count": null,
      "static_account_count_before_bracket": null,
      "static_account_count_after_bracket": null,
      "resolved_account_count_after_bracket": null,
      "serialized_v0_tx_bytes_after_bracket": null,
      "cu_estimate": null,
      "classification": "Green",
      "error": null
    }
    // … one row per maxAccounts in {64, 58, 55, 52, 50, 48}
  ],
  "safety_markers": [
    "NO_SIGNING",
    "NO_BROADCAST",
    "NO_TRANSACTION_SUBMISSION",
    "FETCH_BUILD_ONLY"
  ],
  "packet_data_size": 1232,
  "notes": [
    "Bracket model: Variant A two-ix...",
    "ALT modeling: each Jupiter ALT pubkey gets a synthetic...",
    "CU estimate is intentionally None in this slice..."
  ]
}
```

The harness does **not** write a JSON fixture when the env gate is unset
(no measurement was run, so there is nothing to record).

---

## 9. Sources Re-Confirmed (2026-05-09)

| Source | Confirmation |
|---|---|
| Jupiter `/swap/v2/build` URL & verb | [developers.jup.ag/docs/swap/build](https://developers.jup.ag/docs/swap/build) — Method: `GET`, Path: `/swap/v2/build`, base host `https://api.jup.ag`. Required query params: `inputMint`, `outputMint`, `amount`, `taker`. Optional: `maxAccounts` (default 64, range 1–64). |
| Jupiter `maxAccounts` semantics | [developers.jup.ag/docs/swap/advanced/reduce-transaction-size](https://developers.jup.ag/docs/swap/advanced/reduce-transaction-size) — *"Very low values (e.g below 50) can result in **no route found** or **significantly worse pricing**."* |
| Solana 1232-byte cap | [solana.com/docs/core/transactions](https://solana.com/docs/core/transactions) — `PACKET_DATA_SIZE = 1,232 bytes` (IPv6 MTU 1280 − 48 byte headers). |
| v0 + ALT 64-key budget | [solana.com/docs/advanced/lookup-tables](https://solana.com/docs/advanced/lookup-tables) |
| Instructions sysvar id | `Sysvar1nstructions1111111111111111111111111` per `solana_sdk::sysvar::instructions`. |
| Pyth receiver program id | `rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ` per `STAGE2_PYTH_ADAPTER_RESEARCH.md`. |

---

## 10. What This Slice Does NOT Claim

- **Not a measurement run.** No live data was produced as part of the
  commit that introduced this harness; the JSON fixture is created only
  by an opt-in run with `CLAW_STAGE2_JUPITER_SIZING=1`.
- **Not a re-classification of the J-prep report.** The J-prep report's
  "Needs measurement" verdict still stands until at least one live run
  fills the matrix and is reviewed.
- **Not a CU verdict.** The harness records `cu_estimate: null` for every
  row and explicitly defers the compute-unit gate to a future slice.
- **Not a green-light for Stage 2 implementation.** All three of the
  J-prep § 13.1 implementation blockers (Jupiter v6 program-id pin, Jito
  tip pubkey allowlist, ClawSol-owned ALT deployment) remain open; the
  per-class allowlist module
  (`programs/clawsol-authority/src/jupiter_boundary.rs`) does not yet
  exist.

---

## 11. Next Slices (Not in This One)

1. **Run the harness against `api.jup.ag`** with a live key (or via the
   public tier, if it still serves `/swap/v2/build` without an
   `x-api-key`). Commit the resulting fixture as a separate artifact and
   write a measurement-results doc that re-classifies the matrix.
2. **CU gate slice.** Add an env-gated `simulateTransaction` probe (read-
   only RPC; no signing) so each row gains a measured `cu_estimate`.
3. **Per-class allowlist module.** Land
   `programs/clawsol-authority/src/jupiter_boundary.rs` with the program-
   id pins from J-prep § 5.2 and the sibling-ix verifier state machine
   from § 5.3 / § 5.4.
4. **ClawSol-owned ALT deployment** if the byte mitigation in J-prep
   § 10.2 item 3 turns out to be necessary after live measurement.

---

## 12. 2026-05-09 Measurement Result

First live `api.jup.ag/swap/v2/build` run committed as
[`stage2_jupiter_sizing_fixtures.json`](./stage2_jupiter_sizing_fixtures.json).

**Scenario.** USDC → wSOL, 5 USDC (`amount_raw = 5_000_000`),
`slippageBps = 50`, `restrictIntermediateTokens = true`.

**maxAccounts matrix.**

| `maxAccounts` | Wrapped v0 tx bytes | Classification | Note |
|---:|---:|---|---|
| 64 | 1648 | Red | over the 1232-byte cap |
| 58 | 1776 | Red | over the cap |
| 55 | 1083 | Green | fits with headroom |
| 52 | 1083 | Green | fits with headroom |
| 50 | 1083 | Green | fits with headroom |
| 48 | — | Inconclusive | HTTP 429 from `api.jup.ag` (rate-limited) |

**Key conclusion.** Stage 2 Jupiter is **not preview-only by default**:
the matrix shows measured green rows at `maxAccounts ∈ {55, 52, 50}`
under the synthetic two-ix bracket model. Implementation **remains
gated** by (a) the exact on-chain bracket wire shape (placeholder ix
data sizes used for the measurement), (b) ALT field-semantics
clarification (see caveat below), and (c) the J-prep § 13.1 blockers
(Jupiter v6 program-id pin, Jito tip allowlist, ClawSol-owned ALT
deployment).

**Caveat — ALT field semantics.** Jupiter returned populated
`addressesByLookupTableAddress` data but an **empty**
`addressLookupTableAddresses` array. The fixture must therefore be read
**conservatively as inline-resolved / no-ALT** until Jupiter's field
semantics are pinned in writing — i.e. the 1083-byte rows assume zero
ALT loading at compile time. If a future `/build` response includes
non-empty `addressLookupTableAddresses` and forces compile-time ALT
resolution, the green-row byte counts may shift; rerun the matrix
before claiming the green classification holds end-to-end.

**Safety.** This measurement was strictly **live API fetch/build only**:
no signing, no broadcast, no `simulateTransaction`, no transaction
submission. The JSON fixture's `safety_markers` array re-asserts
`["NO_SIGNING", "NO_BROADCAST", "NO_TRANSACTION_SUBMISSION",
"FETCH_BUILD_ONLY"]`.
