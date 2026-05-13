# Demo Evidence

This branch (`reset/precise-docs-only`) is the **docs / evidence
layer** of the Solfrontier 2026 redesign. It does not yet carry
mainnet evidence originated from the redesign — those land as
Phase 4+ ships. The hackathon mainnet evidence, reproduced verbatim
below, lives authoritatively in the hackathon repository (frozen for
judging through 2026-06-23).

## Hackathon evidence (verbatim pointers)

These are the mainnet finalizations the loop proved during the
Frontier hackathon. All are independently verifiable on Solscan.

### W5h-lite — chat-first funding round-trip

- **Funding tx:** [`8XjiMX3G…oQen`](https://solscan.io/tx/8XjiMX3GyrtjXweHFsi2fa5WrH7xJLTTSN6fb88gB9bM6NH98LuFQrBWVVCEU96NvtUnPaRBEujFTi7NLRnoQen)
- **Confirmation slot:** 419,183,366
- **Amount:** 0.25 USDC (raw 250,000)
- **State transition:** `funding_pending → budget_reserved`
- **Decision metrics at funding time:** Save display APY 210 bps,
  native on-chain APR 165 bps, threshold 100 bps (condition
  satisfied).

### W5i — autonomous Solend deposit on mainnet

> **Important caveat — read first.** The hackathon W5i smoke
> required an **operator-side SQL override** before the autonomous
> path could complete. The on-chain finalization below is real and
> Solscan-verifiable, but the **complete watcher-only round-trip
> without operator intervention has not yet been proven** on
> mainnet. The exact SQL is inline further down — read it before
> interpreting the deltas.

**Mainnet finalizations:**

- **Funding tx:** [`AvJFBJ7Z…ganB`](https://solscan.io/tx/AvJFBJ7ZSsQrZjCjxdVBHm5LjYdjMKvCmXbjF6TJSkWjmAAPiW88g8uc5K2TNMQJvXLKBFMNFDpyi9pLEAkganB)
  - slot 419,197,204;
  - memo: `claw:w5h:6400000090d00300000000009a62dace:609a14b2a372b04aabaceadad21cce493bc5197ed21487a6d7adb4a2696f097f`
- **Auto Solend deposit tx:** [`takmz89b…jTv5`](https://solscan.io/tx/takmz89b5hLHgJ9jHVBbqJogxdvu8xuMdUgZLEpdHhTD7zvRswrrk1UnxfZf7pbsaM4fygqvDFwFbph5YH2jTv5)
  - finalized slot 419,200,198;
  - 91,893 compute units; 4 outer instructions; 2 Solend program
    invocations; 0 Jupiter; 0 refund.
- **Token deltas** (from `meta.preTokenBalances` /
  `meta.postTokenBalances`):
  - Controlled USDC ATA: **−250,000 raw**
  - Solend reserve USDC vault: **+250,000 raw**
  - Solend collateral pool cToken: **+192,819 raw**
  - Controlled wallet cToken ATA: transit-only (0 → 0)
- **No human signature between funding and execution** — but see
  caveat above; the autonomous path only succeeded after an
  operator SQL override extended the intent's expiry. The watcher
  detected the (now-extended) eligible intent on a 30-second tick,
  claimed the CAS lease, signed with the controlled wallet, and
  broadcast the deposit.

#### Operator-side SQL override (the honest part)

After the live gates were armed but **before** the watcher could
pick the intent up, the original `expires_at_ms` had already
lapsed during the daemon-arming process. The CAS gate behaved
correctly throughout this interval: 17 pre-update ticks returned
`prechecks_failed` because `expires_at_ms < now_ms`. The
autonomous-execution path never bypassed that check.

To allow the loop to complete in that sitting, the operator ran
the following targeted SQL **override** against the running
daemon's intent store:

```sql
UPDATE stage2_w5h_funding_intents
SET expires_at_ms = now_ms + 600_000,
    updated_at_ms = now_ms
WHERE rule_id_hex = '6400000090d00300000000009a62dace'
  AND status = 'budget_reserved';
-- rows_affected = 1
```

Only the two timestamp columns changed. `rule_id_hex`,
`canonical_rule_hash_hex`, `amount_raw`, `funding_signature`,
controlled wallet, and USDC ATA were all unchanged. The CAS gate's
strictness against expiry is what the 17 pre-update ticks
demonstrated; the loop's freedom from operator state mutation is
**not** what the smoke demonstrated.

**This is the gap the redesign needs to close.** A single-sitting
round-trip — deterministic / structured chat command → funding →
expiry-fresh trigger → autonomous execution — without any operator
state mutation, is not yet proven on mainnet. Closing it (likely
by arming the daemon, funding, and triggering the loop within a
single expiry window) is on the roadmap.

## What the redesign will prove (planned, not yet executed)

Once the modules land, the redesign will reproduce the W5i loop
with:

- a single sitting from funding to execution (no operator SQL
  intervention);
- a deterministic chat command that constructs the Intent (same form
  as the hackathon);
- the same memo-anchor + token-delta verifier;
- the same CAS-gated watcher;
- the same controlled-wallet executor.

Each new live proof will be appended to this file with a Solscan
link and the standard "what this proves / what this does not
prove" pair.

## Conventions for this file

Every entry must include:

1. A Solscan link to the on-chain finalization.
2. The confirmation slot.
3. The token-balance deltas in raw units (no rounded display values
   without the raw column).
4. A "what this proves" bullet list.
5. A "what this does NOT prove" bullet list immediately following.

If an entry cannot supply (5), the entry is not ready to land here.
