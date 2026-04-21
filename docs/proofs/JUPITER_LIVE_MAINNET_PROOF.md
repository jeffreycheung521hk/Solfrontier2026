# Jupiter JIT Phase 1 — Live Mainnet On-Chain Proof

**Date:** 2026-04-18
**Scope:** End-to-end correctness of the Jupiter JIT path against live
`api.jup.ag` + mainnet RPC, including LLM-driven ingress, real Phantom
signing, and the full daemon completion path
(`complete_v0` + `send_raw_v0_transaction`).
**Status:** ✅ **Finalized on mainnet — user natural language → LLM → daemon → Phantom → daemon broadcast → on chain.**

## Proof layers summary

| Layer | Proven by | Key tx |
|------|-----------|--------|
| `POST /sessions/:id/messages` → OpenAI → `submit_jupiter_swap` | A2-Execute | `52bvUZ1…` |
| LLM MUST NOT inject `wallet_pubkey` — session binding is authoritative | A2-Execute (assert in test) | `52bvUZ1…` |
| Phantom signs V0 → `POST /wallet-signatures` → daemon `complete_v0` + `send_raw_v0_transaction` → mainnet | A1-Execute + A2-Execute | `Q5pb…` + `52bvUZ1…` |
| Live Jupiter API + mainnet ALT resolve + V0 assemble + simulate | `live_jupiter_mainnet_shape` | — |
| Daemon recognizes V0 vs legacy signed bytes | `orchestrator::signed_tx_kind` unit + integration tests | — |

## A2-Execute confirmed mainnet transaction (end-to-end LLM → mainnet)

This is the transaction that went through the **complete product path** —
user natural language → OpenAI routes the tool → daemon enforces session-bound
wallet → policy approves → JIT builds V0 → Phantom signs → daemon
`complete_v0` + `send_raw_v0_transaction` → mainnet finalized.

- **Signature**: `52bvUZ1eEHjrbxRBQBssYWg7BrFNoAY1Zeynw44hr3BKTAe9zTHv9Qz4wQsNi7Jw6sbXpb8zehHafn1gFzLD2dE1`
- **Explorer**: <https://explorer.solana.com/tx/52bvUZ1eEHjrbxRBQBssYWg7BrFNoAY1Zeynw44hr3BKTAe9zTHv9Qz4wQsNi7Jw6sbXpb8zehHafn1gFzLD2dE1>
- **Slot**: 414,027,719 (finalized)
- **Block time**: 2026-04-18 (`blockTime=1776511400`)
- **User message that triggered it**: `"Swap 0.001 SOL to USDC with 1% slippage."`
- **LLM model**: `gpt-4o-mini` (openai) — selected `submit_jupiter_swap` with correct mints, amount, slippage
- **Signer (Phantom)**: `C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW`
  - The tool's `wallet_pubkey` parameter was **resolved by the daemon from the session binding**, not supplied by the LLM. Test asserts `stored_intent.intent.wallet_pubkey == target_wallet`.
- **Swap**: 1,000,000 lamports wSOL → 86,729 µUSDC
- **Fee**: 75,410 lamports (base + Jupiter `auto` priority tip)
- **CU consumed**: 78,590
- **Total SOL moved**: 1,075,410 lamports
- **submitted**: true, **rebuild_required**: false, **retries**: 0

## A1-Execute confirmed mainnet transaction (the load-bearing proof)

This is the transaction that went through **the full daemon completion
path** — Phantom `signTransaction` → POST to daemon → `orchestrator.complete()`
→ `ExternalWalletAdapter::complete_v0()` → `ClawRpcClient::send_raw_v0_transaction()`
→ mainnet finalized.

- **Signature**: `Q5pbBmjjn7tbFKEyswzyFsAkYoGHVJe4TT2m2viGbBCa2j9LXrFQ8X8AeKjdXw3seom2YYezFdtS8jBbpA1zK6H`
- **Explorer**: <https://explorer.solana.com/tx/Q5pbBmjjn7tbFKEyswzyFsAkYoGHVJe4TT2m2viGbBCa2j9LXrFQ8X8AeKjdXw3seom2YYezFdtS8jBbpA1zK6H>
- **Slot**: 413,993,948 (finalized)
- **Block time**: 2026-04-18 (`blockTime=1776498064`)
- **Signer (Phantom)**: `C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW`
- **Swap**: 1,000,000 lamports wSOL → 88,428 µUSDC
- **Fee**: 23,082 lamports (base fee + Jupiter `auto` priority tip)
- **CU consumed**: 75,388
- **Total SOL moved**: 1,023,082 lamports
- **submitted**: true, **rebuild_required**: false, **retries**: 1 (first
  daemon-submitted tx without priority fee was dropped by leaders under
  mainnet congestion; fix was to add `prioritizationFeeLamports: "auto"`
  to the Jupiter build request)

### Earlier Phase 1 closeout transactions (bypassed daemon completion path)

The following two broadcasts happened via the HTML's `signAndSendTransaction`
path before A1-Execute existed. They confirm live Jupiter + V0 assembly +
Phantom V0 compat on mainnet, but they **did not** exercise `complete_v0`
or daemon-side submit — Phantom's own RPC broadcast them directly.

- `3MtJe2mW4pXvY9B5DT65Kr5hxTuAMKavzYd7fudb62F5w124h6vCrWemUUCmQ7iY19FsPyQ5PM2i6sS1dR5nFDym` — 0.001 SOL → 88,366 µUSDC, 9,931 lamports fee, route HumidiFi
- `4bV72Ww7WVWMKNjEwoETBrrx3QmQfeN71y9Q2tptsNSvj8QX5DMUqUmK8FDPzNGLfN5ChFg9SaswfR5fRs4YBbLB` — 0.001 SOL → 88,319 µUSDC, 7,049 lamports fee

---

## What this proof demonstrates

1. **Live Jupiter API contract is correctly modelled**: `/swap/v1/quote` + `/swap/v1/swap-instructions` responses deserialize into `SwapQuoteResponse` + `SwapBuildResponse`.
2. **Live V0 assembly works**: the resulting `VersionedTransaction` is byte-compatible with the Solana V0 format and ready for signing.
3. **Phantom accepts V0 + ALT transactions produced by our assembler**: the signed output from Phantom verifies on-chain without any tampering needed (no `rebuild_required` signal emitted — Phantom preserved the blockhash).
4. **Mainnet executes our Jupiter-routed V0 tx as intended**: the on-chain balance changes match the quoted route.
5. **ALT resolution via mainnet RPC is correct**: the 1 ALT referenced by Jupiter's response was resolved client-side into concrete addresses, and the resulting V0 message header correctly declared the table-lookup slot usage.

## What this proof does NOT claim

- **Not a devnet confirmation.** Jupiter only indexes mainnet liquidity — devnet Jupiter is architecturally impossible. See [docs/JUPITER_JIT_PHASE1.md](../JUPITER_JIT_PHASE1.md).
- **Not an LLM / natural-language intent proof.** `submit_jupiter_swap` was invoked in-process by the test driver, bypassing the agent runtime + OpenAI routing. That path (A2) is not part of A1-Execute's scope.
- **Bind challenge is programmatically signed by the user with a real Phantom** — but the test driver runs the bind-confirm HTTP call itself; it does not go through the frontend chat UI.

---

## How to reproduce

**Prerequisites**
- Network access to `api.jup.ag` and `api.mainnet-beta.solana.com`
- Opt-in env var (CI does not set this)

**Command**
```bash
CLAW_LIVE_JUPITER_MAINNET=1 \
  cargo test -p claw-gateway --test live_jupiter_mainnet_shape -- --nocapture
```

**Test source**: [`crates/gateway/tests/live_jupiter_mainnet_shape.rs`](../../crates/gateway/tests/live_jupiter_mainnet_shape.rs)

**Captured output**: [`jupiter_live_mainnet_shape.log`](./jupiter_live_mainnet_shape.log)

---

## What the run proves, step by step

### Step 1 — live Jupiter quote (`GET /swap/v1/quote`)
- Input: 1,000,000 lamports wSOL → USDC, 100 bps slippage
- Quoted out: ~88,900 µUSDC (≈ $0.089) via a single-hop `HumidiFi` route
- The `SwapQuoteResponse` deserializes cleanly from the live API.

### Step 2 — live Jupiter swap-instructions (`POST /swap/v1/swap-instructions`)
- Returns 2 compute-budget + 4 setup + 1 swap + 1 cleanup instructions
- `addressLookupTableAddresses` populated, `addressesByLookupTableAddress` null (live contract)
- Deserialized correctly by the patched `SwapBuildResponse`
- Swap program ID matches Jupiter V6 aggregator (`JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4`)

### Step 3 — mainnet ALT resolution (`getAccount` per ALT)
- One ALT address returned by Jupiter
- Resolved via `ClawAltFetcher::fetch_alt_data` → `AddressLookupTable::deserialize`
- Account held 144–253 inner addresses (varies per run — Jupiter picks different routes)

### Step 4 — V0 transaction assembly (offline)
- `assemble_v0_transaction_with_resolved_alts` produced a `VersionedTransaction`
- Confirmed V0, not legacy
- 8 instructions, 10–12 static account keys, 1 address-table-lookups entry
- 1 required signature slot, all zero (unsigned as expected)

### Step 5 — mainnet simulate (`simulateTransaction`, read-only)
- `simulate.success = true`
- `simulate.error = None`
- `compute_units = 2915`

Mainnet state machine accepts this transaction as well-formed. Compute usage fits well within Solana's 1,400,000 CU ceiling.

---

## Human-review payload — the actual broadcast run

| Field | Value |
|-------|-------|
| wallet (signer) | `C4QQjzWxnJ5QFAbkzhQJ3wTzyX6nw1vyFvJwbPXJGPNW` (Phantom, mainnet) |
| signer path | EXTERNAL_WALLET / Phantom V0 |
| input_mint | `So11111111111111111111111111111111111111112` (wSOL) |
| output_mint | `EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v` (USDC mainnet) |
| input_amount | 1,000,000 lamports (0.001 SOL) |
| slippage_bps | 100 (1%) |
| quoted_out (at build time) | 88,904 µUSDC |
| min_accepted_out | 88,015 µUSDC |
| **delivered_out (on chain)** | **88,366 µUSDC** (~0.6% slippage used) |
| route | `HumidiFi` (selected by Jupiter at build time) |
| ATAs created in setup | yes (wSOL wrap + USDC ATA for signer) |
| ALT count | 1 (resolved from `addressLookupTableAddresses`) |
| compute_units (actual) | 102,236 |
| priority fee | 0 (dynamicComputeUnitLimit enabled) |
| base fee | 9,931 lamports |
| total SOL moved from signer | 3,049,211 lamports |
| ready_to_submit | **YES — finalized on chain at slot 413,969,975** |

---

## What bugs were fixed to make this proof run

Before this validation, the Jupiter production path was code-compiled and unit-tested, but had never been pointed at the real Jupiter API. The proof surfaced the following wire-contract bugs, all fixed as the minimum necessary to make a live run succeed:

1. **Wrong endpoint** — `POST /swap/v2/build` (404 Not Found) was replaced with `POST /swap/v1/swap-instructions`. `GET /v6/quote` was replaced with `GET /swap/v1/quote`.
2. **Missing response fields** — `addressLookupTableAddresses` and `tokenLedgerInstruction` are present in live responses; added to `SwapBuildResponse`.
3. **Nullable field rejected** — live API returns `addressesByLookupTableAddress: null`. Added `null_or_map` custom deserializer.
4. **Blockhash format** — live API returns blockhash as a 32-byte array, not a base58 string. Added dual-format `blockhash_string_or_bytes` deserializer that normalizes to base58.
5. **Optional `feeAmount` / `feeMint`** — some AMMs (HumidiFi, GoonFi V2, BisonFi) omit these. Made optional in `SwapInfo`.
6. **ALT resolution missing entirely** — live API only returns ALT addresses, not contents. Added `jupiter_alt` module + `AltAccountFetcher` trait + `ClawAltFetcher` production impl.
7. **Reqwest query builder panic** — `.query(&Option<(_, String)>)` returned `builder error: unsupported pair`. Fixed by building the query vec cleanly.

---

## How the broadcast was performed (handoff flow)

Because the Phantom extension is a browser-only component, the CLI session cannot produce a signed-and-broadcast transaction by itself. The flow used was:

1. `cargo test -p claw-gateway --test live_jupiter_mainnet_shape live_jupiter_mainnet_emit_phantom_handoff -- --nocapture` with `CLAW_LIVE_JUPITER_HANDOFF=1` and `CLAW_LIVE_JUPITER_WALLET=<signer pubkey>` emits two artifacts under `docs/proofs/`:
   - `jupiter_mainnet_unsigned_tx.b64` — raw base64 of the unsigned V0 tx (reference)
   - `phantom_sign_and_broadcast.html` — a self-contained page that embeds the bytes and uses `window.phantom.solana.signAndSendTransaction` to sign + broadcast via Phantom's own RPC path
2. The operator runs `python -m http.server 8080` inside `docs/proofs/` (file:// URLs break the Phantom injection).
3. The operator opens `http://localhost:8080/phantom_sign_and_broadcast.html`, connects Phantom, reviews both the page's swap summary AND the details Phantom displays in its own signing UI, then clicks "Sign & Send".
4. The page renders the returned signature and an Explorer link. The operator pastes the signature back into the working session for verification.

## Postmortem — the stranded `ByZH…` pubkey

The original `config/phantom-e2e.example.toml` and early drafts of this proof doc referenced a pubkey `ByZH6oQ7kZ5h32t5dJsbayLGAyKK8YetWvoLTHqzdkma` as "the Phantom wallet". That identifier was a placeholder that ended up with no corresponding seed in the repo or anyone's possession. Before catching this, 0.03 SOL was routed to that address and is now irrecoverably stranded on mainnet.

Actions taken in response:
- `crates/gateway/tests/live_jupiter_mainnet_shape.rs` no longer hard-codes a pubkey. It requires `CLAW_LIVE_JUPITER_WALLET` at run time and panics with a clear message if missing.
- `config/jupiter-mainnet-e2e.example.toml` replaced the placeholder pubkey with an explicit `REPLACE_WITH_YOUR_PHANTOM_MAINNET_PUBKEY` marker plus a warning comment describing what went wrong.
- Before any future proof run, the operator MUST verify they control the seed for the pubkey they supply, BEFORE any mainnet transfer.

---

## A1-Execute — what it closed (the daemon-completion gap)

The A1-Execute live proof above (tx `Q5pbBmjjn7t…`) closes the gap that the
earlier Phase 1 closeout broadcasts (`3MtJe2m…`, `4bV72W…`) deliberately left
open. Those used `signAndSendTransaction` and bypassed our daemon; A1-Execute
goes through `POST /sessions/:id/wallet-signatures` → `orchestrator.complete()`
→ `ExternalWalletAdapter::complete_v0()` → `ClawRpcClient::send_raw_v0_transaction()`.

### Two live blockers surfaced and fixed during A1-Execute

1. **`complete_v0` rejected Phantom-canonicalized V0 messages**
   (`"V0 instruction 1 accounts mismatch"` on the first attempt). Root cause:
   web3.js / Phantom's `signTransaction` re-encodes the V0 message with a
   canonical `account_keys` ordering and correspondingly re-indexed
   instruction account arrays. Our check compared raw `Vec<u8>` of indices,
   which fails on semantically-equal re-encodings.
   Fix: compare *resolved account identities* (static_keys → pubkey; ALT
   references → `(table, inner_index, writable)` tuple) per instruction
   position. Tampering (swap pubkey at same index) still fails; benign
   reordering passes. See `crates/gateway/src/orchestrator/external_adapter.rs`.

2. **daemon-submitted tx dropped by mainnet leaders** (tx `3sJz38Ti…` —
   `submitted:true` on our side but never landed on chain, balance unchanged).
   Root cause: our Jupiter build request passed `prioritization_fee_lamports:
   None`, giving the tx zero priority. Phantom's own `signAndSendTransaction`
   path had silently added a priority fee on earlier broadcasts, masking the
   issue.
   Fix: `prioritization_fee_lamports: Some(json!("auto"))` — Jupiter sizes
   the tip against current congestion.

## Invariants preserved

This validation did **not** weaken any of the Phase 1 invariants (approval-before-build, simulate-before-sign, strict blockhash, etc). The Jupiter deterministic suite remains green after the shape fixes.
