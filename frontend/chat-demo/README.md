# chat-demo

Reviewer-facing chat demo UI for Solfrontier 2026.

## Status

Scaffold. The Next.js app + chat page land alongside Phases 2–4 of
the [`ROADMAP`](../../docs/ROADMAP.md). The hackathon prototype
shipped a working chat UI; that surface is the design reference but
will be rebuilt against the cleaner module boundaries described in
[`ARCHITECTURE.md`](../../docs/ARCHITECTURE.md).

## What the chat demo will do

1. Accept a deterministic structured chat command (English or
   Chinese form per the architecture spec).
2. Render a funding card (rule_id, canonical hash, memo preview,
   pinned demo amount, Phantom Fund button).
3. Trigger exactly one `signTransaction` + one
   `sendRawTransaction` on Fund click. The frontend has no other
   signing call site.
4. POST the funding signature to the gateway's
   `POST /sessions/:id/funding/confirm` route.
5. After `budget_reserved`, poll the read-only order-status
   endpoint and render the watcher's terminal state (`completed`
   with tx + Solscan, or one of the typed terminal failures).

## What the chat demo will NOT do

- Hold a keypair. (Boundary B2.)
- Issue a second `signTransaction` for the execution leg. (Boundary
  B1.)
- Render an "Execute" / "Approve" / "Send Solend" / "Confirm
  Deposit" button. The chat input is the only path.
- Promise an automatic refund. (Refund is operator-initiated; see
  ROADMAP Phase 5.)
