# Devnet on-chain proof

External, verifiable evidence that the ClawSolana signing / swap pipeline
produces real Solana transactions — not simulations, not mocked responses.

Every signature below can be independently confirmed at
`https://explorer.solana.com/tx/<signature>?cluster=devnet`.

---

## How these signatures were produced

**Test file**:
[`crates/solana-core/tests/devnet_orca_swap_e2e.rs`](../crates/solana-core/tests/devnet_orca_swap_e2e.rs)

**Prerequisites at run time**:
- Solana devnet RPC reachable (`https://api.devnet.solana.com`)
- A local keypair with ≥ ~0.5 SOL on devnet
- Keypair path passed via `CLAW_DEVNET_KEYPAIR` (falls back to
  `./data/devnet.json` relative to repo root)

**Command**:
```bash
CLAW_DEVNET_KEYPAIR=C:/Users/jeffr/SolFrontier/data/devnet.json \
  cargo test -p claw-solana-core --test devnet_orca_swap_e2e -- --nocapture
```

**What the test does, in order**:
1. Loads the keypair from disk.
2. Derives the wallet's ATAs for wSOL and devUSDC via `get_associated_token_address`.
3. Submits `create_associated_token_account_idempotent` for both ATAs
   (one tx) — signed, submitted, confirmed.
4. Funds the wSOL ATA with lamports + `sync_native` to produce a real
   wrapped-SOL balance — signed, submitted, confirmed.
5. Builds the Orca `Swap` instruction by hand (program-ID, oracle PDA,
   tick arrays, vaults all derived from chain data).
6. Calls `simulateTransaction` — must succeed.
7. Signs and submits the swap — confirms on-chain.

If any step fails, the test panics and no "success" signature is produced.

---

## Keypair (public key)

```
7YThgGWKvKP1YY8E5Pod9CeaHj2kHA1H3Z4gKwfyPHcT
```

Explorer: <https://explorer.solana.com/address/7YThgGWKvKP1YY8E5Pod9CeaHj2kHA1H3Z4gKwfyPHcT?cluster=devnet>

This keypair was generated locally during the Phase 3 working session via
`cargo run --bin claw-keygen` and funded with **1 SOL** from the official
Solana devnet faucet.

---

## The three transactions

Captured from test stdout during the Phase 3 working session run. All three
are on devnet and are independently verifiable on Solana Explorer.

### 1. Create ATAs (wSOL + devUSDC)

```
2t2oXqSy9EiwRRoASdi8h7Jz5HZR955udazct1dL5C6M2Qka1M3DgdS9bmNoMffbksSU8tMb8fxCasBNprvKengN
```

Explorer: <https://explorer.solana.com/tx/2t2oXqSy9EiwRRoASdi8h7Jz5HZR955udazct1dL5C6M2Qka1M3DgdS9bmNoMffbksSU8tMb8fxCasBNprvKengN?cluster=devnet>

**What it proves**: the test rig can construct, sign, and submit a
transaction that invokes the Associated Token Account program
`ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJe1bC8` and the Token program
`TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA`, and that the network
accepts and confirms it.

### 2. Fund wSOL ATA (transfer + sync_native)

```
5LUocD36yWBDDwK9VQTfDB9SX2tr2Bdyudz371TzFbzvs6w7WaTJjKUfFA5XaGo1q9X4wpxCTKKwaEQaxuX3DFHx
```

Explorer: <https://explorer.solana.com/tx/5LUocD36yWBDDwK9VQTfDB9SX2tr2Bdyudz371TzFbzvs6w7WaTJjKUfFA5XaGo1q9X4wpxCTKKwaEQaxuX3DFHx?cluster=devnet>

**What it proves**: the pipeline correctly wraps native SOL into the wSOL
mint so the downstream swap can consume it.

### 3. Orca Whirlpool swap: SOL → devUSDC

```
xpzQhbD1pqJcJrKF7GiZQPAzUR21t286EMGVi3gGodpQzbPuu6tDvrm6HassCyMKF3n4131QZkqBXHc79JhcwHv
```

Explorer: <https://explorer.solana.com/tx/xpzQhbD1pqJcJrKF7GiZQPAzUR21t286EMGVi3gGodpQzbPuu6tDvrm6HassCyMKF3n4131QZkqBXHc79JhcwHv?cluster=devnet>

**What it proves**: the pipeline builds a valid Orca Whirlpool swap
instruction (correct discriminator, correct account ordering, correct PDA
derivations for oracle and tick arrays) and the Whirlpool program
`whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc` accepts it.

**Captured log excerpt** (from test stdout):

```
Program whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc invoke [1]
Program log: Instruction: Swap
Program TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA invoke [2]
Program TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA success
Program TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA invoke [2]
Program TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA success
Program whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc consumed 29,307 of 200,000 CU
Program whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc success
```

The two SPL Token CPIs inside the swap correspond to the two legs of the
AMM transfer (SOL vault in, devUSDC vault out).

---

## Supporting (non-signing) devnet probes

These tests verify that the Orca devnet state referenced by the E2E is
still live. They do not require a funded keypair and can be run by any
reviewer:

| File | Verifies |
|---|---|
| `crates/solana-core/tests/devnet_orca_check.rs` | Orca Whirlpool program and the SOL/devUSDC pool exist on devnet |
| `crates/solana-core/tests/devnet_orca_swap_sim.rs` | A swap instruction built from our code compiles and is accepted by `simulateTransaction` (expected error is `AccountNotInitialized` from the dummy wallet, which is a wallet issue, not an instruction issue) |

Accounts referenced by both the probes and the E2E:

| Role | Address |
|---|---|
| Orca Whirlpool program | [`whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc`](https://explorer.solana.com/address/whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc?cluster=devnet) |
| SOL/devUSDC pool | [`3KBZiL2g8C7tiJ32hTv5v3KM7aK9htpqTw4cTXz1HvPt`](https://explorer.solana.com/address/3KBZiL2g8C7tiJ32hTv5v3KM7aK9htpqTw4cTXz1HvPt?cluster=devnet) |
| Pool vault A (SOL) | [`C9zLV5zWF66j3rZj3uuhDqvfuA8esJyWnruGzDW9qEj2`](https://explorer.solana.com/address/C9zLV5zWF66j3rZj3uuhDqvfuA8esJyWnruGzDW9qEj2?cluster=devnet) |
| Pool vault B (devUSDC) | [`7DM3RMz2yzUB8yPRQM3FMZgdFrwZGMsabsfsKopWktoX`](https://explorer.solana.com/address/7DM3RMz2yzUB8yPRQM3FMZgdFrwZGMsabsfsKopWktoX?cluster=devnet) |
| Wallet wSOL ATA | [`F7FGBKXYShhRSvBryqziDGxWmGMJPq7sYZWVCobzy1Ed`](https://explorer.solana.com/address/F7FGBKXYShhRSvBryqziDGxWmGMJPq7sYZWVCobzy1Ed?cluster=devnet) |
| Wallet devUSDC ATA | [`37FZG6okx18cBx8vp9yWwagNun4d92gZbNfC6LR9BGaQ`](https://explorer.solana.com/address/37FZG6okx18cBx8vp9yWwagNun4d92gZbNfC6LR9BGaQ?cluster=devnet) |

---

## Coverage gaps

The on-chain evidence above is **limited in scope** — it demonstrates that
the signing pipeline works against a real DEX. It does **not** currently
include:

1. **A mainnet-beta run.** All signatures are on devnet.
2. **A transaction that flows through the policy / approval workflow
   end-to-end and lands on-chain.** The E2E test builds + signs + submits
   directly; it does not go through `/sessions/:id/propose-transfer` →
   human approval → signing resume. That combined path is exercised
   in-memory by `n20_external_wallet_e2e.rs`, but the terminal signed tx
   in that test is not broadcast to devnet.
3. **A USDC transfer hitting the approval chain on-chain.** The USDC
   policy rules are tested via unit + integration tests, and the USDC
   demo dashboard is populated via `/debug/seed-demo`; but there is no
   real-wire USDC transfer on devnet recorded in this repo.
4. **A rejected / expired tx demonstrated on-chain.** Rejection and
   lease-expiry are by construction *not* submitted, so there is nothing
   on-chain to show. The audit rows for those outcomes are proven
   in-memory by `p1_core_correctness.rs` and `p2_api_blackbox.rs`.

Each gap is a candidate for future evidence capture. Suggested additions:

- A one-shot `bins/demo-signed-approval` binary that opens a session,
  proposes a transfer, approves it, and submits to devnet — the final
  signature would close gap (2).
- Funding an SPL-token ATA for devUSDC and driving a TransferChecked
  through the pipeline for gap (3).
