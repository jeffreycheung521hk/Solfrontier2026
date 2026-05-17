# Demo Evidence

This document is a pointer to mainnet evidence. The detailed proof
walk-through for the Phase 5c-lite live run lives in
[`PHASE5C_LITE_MAINNET_PROOF.md`](PHASE5C_LITE_MAINNET_PROOF.md).
All token-balance deltas in this repo are computed programmatically
from parsed `meta.preTokenBalances` / `meta.postTokenBalances`
(with `accountIndex → message.accountKeys` resolution) — Solscan
visual inspection alone is **not** used as proof.

## Phase 5c-lite — 0.5 USDC mainnet round-trip ★

Date: **2026-05-16**. Source commit: `5c37e1a`.

Headline transactions:

| Leg | Signature | Slot |
|---|---|---|
| Funding (user-signed) | [`oF5XWWGa…Je9`](https://solscan.io/tx/oF5XWWGaU7XeHc7rAfYHjekhW2cLM7zxKUReEWPh6U38SmRnDw9FV98xWLs9RNWwyWvXdQe7bJraUhcNquK8Je9) | 420,130,565 |
| Auto-execute (controlled-wallet Solend deposit) | [`2jynvLkV…UVnJ`](https://solscan.io/tx/2jynvLkVoD9TQfFH3nvMacNQZoJRpK9146TAonds1LFeBsjkBvzn2jFHsgFZASCSR5LRJGQqNgcLMQN2zSLHUVnJ) | 420,131,308 |

The complete deltas table, the LLM draft body, the SQL state before
and after, the operator-SQL caveat, and the reviewer-side SQL
template are in
[`PHASE5C_LITE_MAINNET_PROOF.md`](PHASE5C_LITE_MAINNET_PROOF.md).

Caveats from that proof, summarised so you do not have to click
through to find them:

- Operator SQL override was required to refresh `expires_at_ms`
  before the watcher would lease. **A single-sitting
  watcher-only round-trip without operator SQL is NOT yet proven
  on Phase 5c-lite.**
- `gpt-4o` was used; `gpt-4o-mini` consistently rejected the spec
  phrasing.
- UI copy bug briefly showed "0.25 USDC" on the funding card.
- The runtime is **not production-ready.**

## Hackathon predecessors (carried as context, not part of Phase 5c-lite)

These mainnet finalizations were proven during the Frontier
hackathon and live in the `testingcrypto2` repo (frozen at
`44a807d` for judging). They are the building blocks Phase 5c-lite
inherited.

### W5h-lite — chat-first funding round-trip (0.25 USDC)

- **Funding tx:** [`8XjiMX3G…oQen`](https://solscan.io/tx/8XjiMX3GyrtjXweHFsi2fa5WrH7xJLTTSN6fb88gB9bM6NH98LuFQrBWVVCEU96NvtUnPaRBEujFTi7NLRnoQen)
- Confirmation slot: 419,183,366. Amount: 0.25 USDC.
- State transition: `funding_pending → budget_reserved`.

### W5i — autonomous Solend deposit on mainnet (0.25 USDC)

- **Funding tx:** [`AvJFBJ7Z…ganB`](https://solscan.io/tx/AvJFBJ7ZSsQrZjCjxdVBHm5LjYdjMKvCmXbjF6TJSkWjmAAPiW88g8uc5K2TNMQJvXLKBFMNFDpyi9pLEAkganB)
- **Auto-execute tx:** [`takmz89b…jTv5`](https://solscan.io/tx/takmz89b5hLHgJ9jHVBbqJogxdvu8xuMdUgZLEpdHhTD7zvRswrrk1UnxfZf7pbsaM4fygqvDFwFbph5YH2jTv5)
- Finalized slot 419,200,198, 91,893 CU.
- **Honest caveat:** required the operator SQL override that
  Phase 5c-lite also needed. The single-sitting watcher-only round-trip
  was never proven on the hackathon W5i either. That gap is
  inherited.

### W6 — operator-initiated refund proven once

- **Refund tx:** `2DybD46v…hNQCd` (5 USDC, controlled wallet → user
  USDC ATA, env-gated + approval-phrase-gated).
- Documented at the hackathon repo's `docs/WEEKLY_UPDATES.md`.

## Conventions for this file

Every entry must include:

1. A Solscan link to the on-chain finalization.
2. The confirmation slot.
3. The token-balance deltas in raw units (in
   [`PHASE5C_LITE_MAINNET_PROOF.md`](PHASE5C_LITE_MAINNET_PROOF.md)
   for the Phase 5c-lite run; in the hackathon repo for the
   predecessors).
4. A "what this proves" bullet list.
5. A "what this does NOT prove" bullet list immediately following.

If an entry cannot supply (5), the entry is not ready to land here.
