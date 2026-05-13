# Solfrontier 2026

Post-Hackathon clean architecture for the **Bounded Intent Execution Loop** — a runtime that lets a user pre-commit to a conditional on-chain action without handing over wallet keys or pre-signing every possible execution leg.

> **Status:** scaffolding. This branch (`reset/precise-architecture`) is a
> ground-up redesign of the runtime layer prototyped during the Frontier
> hackathon. The hackathon prototype lives at
> [testingcrypto2](https://github.com/jeffreycheung521hk/testingcrypto2)
> and is frozen for judging through 2026-06-23.
>
> Source-grade implementation is **not yet present** in this branch. What
> ships here is the architecture spec, the module-boundary skeleton, and
> the no-overclaim discipline that will gate every subsequent commit.

## The loop

```
chat command (deterministic structured form)
  ↓
typed Intent (rule_id + canonical hash)
  ↓
on-chain Memo anchor (SPL Memo ix in the funding tx)
  ↓
Phantom budget funding (user signs ONE SPL TransferChecked + Memo)
  ↓
backend funding verifier (RPC: memo exact + token delta exact)
  ↓
state: budget_reserved
  ↓
watcher (tokio interval ticker; re-fetches the pre-set condition)
  ↓
CAS gate (lease_execution_if_budget_reserved)
  ↓
controlled-wallet executor (signs + broadcasts the adapter tx)
  ↓
state: completed (tx signature + on-chain delta evidence)
```

The user signs once, at funding time. Everything after that runs within the
budget the user pre-committed; no further user signature is required for the
auto-execution leg.

## What this does NOT claim

This README is precise about the gap between what's wired and what's
aspirational, because the hackathon prototype already established the
no-overclaim discipline and this redesign keeps it.

- **No arbitrary natural-language parsing.** The chat surface recognises a
  small set of deterministic structured commands (one English form and one
  Chinese form). The LLM is not the parser. Free-form NL → typed Intent is
  out of scope.
- **No LLM in the autonomous execution hot path.** The watcher and executor
  are plain Rust. The LLM appears only on the user-facing chat surface,
  before an Intent is materialised.
- **No production scheduler.** The watcher is a `tokio::time::interval`
  loop in a single daemon process. No distributed coordination, no durable
  job queue, no leader election.
- **No `clawsol-authority` `ExecuteAction` program signer.** The executor
  signs with a plain controlled-wallet keypair. The PDA-authority design
  (and the `AuthorizationRecord` PDA) is post-Hackathon work, not in this
  scaffold.
- **No real third-party user delegation.** The "controlled wallet" is
  operator-controlled today. A genuinely user-delegated controlled wallet
  is a separate, later phase.

## Adapters

- **Solend** — first adapter (deposit; withdraw planned). The hackathon
  prototype proved this end-to-end on mainnet — see [`docs/DEMO_EVIDENCE.md`](docs/DEMO_EVIDENCE.md)
  for the W5h-lite + W5i tx signatures.
- **Jupiter** — next adapter. Sizing and feasibility work happened during
  the hackathon; the conditional bracket implementation is the next slice.

## Layout

| Path | Role |
|---|---|
| `crates/intent-core` | typed Intent, canonical hashing, rule lifecycle |
| `crates/funding` | funding-tx verifier (memo + token delta) |
| `crates/watcher` | budget-reserved scanner + condition evaluator + CAS gate |
| `crates/executor` | controlled-wallet signer + adapter dispatcher |
| `crates/adapters/solend` | Solend reserve interaction |
| `crates/adapters/jupiter` | Jupiter conditional swap (scaffold) |
| `apps/gateway` | HTTP surface (REST + chat dispatch) |
| `frontend/chat-demo` | reviewer-facing chat demo UI |

## Where to start as a reviewer

Read [`docs/REVIEWER_QUICK_PATH.md`](docs/REVIEWER_QUICK_PATH.md) — a
five-minute guided walk through the architecture, the discipline, and the
hackathon proof links.

## License

[MIT](LICENSE).
