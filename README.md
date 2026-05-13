# Solfrontier 2026

**An AI agent should not hold a user's private keys.** But a user often
wants the agent to act on their behalf — deposit when a yield clears a
threshold, swap when a price hits a target, rebalance when a position
drifts. The two requirements look opposed.

Solfrontier 2026 is the runtime that resolves them. The user pre-funds
a bounded action and signs **once**, at funding time. The AI agent
executes only within that pre-committed budget — no further user
signature required and no access to the user's main wallet. We call
this the **Bounded Intent Execution Loop**.

The reusable product is the **loop**, not any single DeFi adapter on
top of it. Solend is the first adapter; Jupiter is the next; subsequent
adapters (other lending markets, swap aggregators, perps) follow the
same template.

> **Status:** scaffolding. This branch (`reset/precise-architecture`)
> is a ground-up redesign of the runtime prototyped during the Frontier
> hackathon. The hackathon prototype lives at
> [testingcrypto2](https://github.com/jeffreycheung521hk/testingcrypto2)
> (frozen for judging through 2026-06-23). Source-grade implementation
> lands in Phase 1+; what ships in this commit is the architecture
> spec, the module-boundary skeleton, and the no-overclaim discipline
> that gates every subsequent commit.

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

- **No arbitrary natural language without schema validation.** Phases
  0–4 use a deterministic regex recogniser for the small set of
  structured commands the hackathon proved. Phase 5 adds an
  LLM-assisted parser — but **every** LLM output must validate
  against the typed Intent schema or be rejected. There is no path
  where a loose LLM interpretation becomes runtime state.
- **No LLM in the autonomous execution hot path.** The watcher, the
  CAS gate, the executor, and the live-transaction broadcast are
  plain Rust — across **all** phases. The LLM-assisted parser
  (Phase 5+) lives only at the chat → Intent boundary. **The LLM
  expands what the surface accepts; it does not relax what the
  runtime trusts.**
- **No production scheduler.** The watcher is a
  `tokio::time::interval` loop in a single daemon process. No
  distributed coordination, no durable job queue, no leader
  election.
- **No `clawsol-authority` `ExecuteAction` program signer.** The
  executor signs with a plain controlled-wallet keypair. The
  PDA-authority design (and the `AuthorizationRecord` PDA) is
  post-Hackathon work, not in this scaffold.
- **No real third-party user delegation.** The "controlled wallet"
  is operator-controlled today. A genuinely user-delegated
  controlled wallet is a separate, later phase.

## Adapters

- **Solend** — first adapter (deposit; withdraw planned). The
  hackathon prototype proved one **specific pinned shape** (0.25
  USDC into Solend Main Pool USDC) end-to-end on mainnet — see
  [`docs/DEMO_EVIDENCE.md`](docs/DEMO_EVIDENCE.md), which includes
  the honest operator-SQL-override caveat the hackathon W5i smoke
  required.
- **Jupiter** — next adapter. Sizing and feasibility work happened
  during the hackathon; the conditional bracket implementation is
  the next slice after Solend completion.
- **The reusable product is the loop, not the adapters.** Every
  adapter exposes a typed action, validates it against a pinned
  shape, and hands a pre-built unsigned tx to the executor. The
  controlled wallet signs and broadcasts via the same single code
  path. Adding a new adapter does not modify the watcher, the CAS
  gate, or the executor's signing path.

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
