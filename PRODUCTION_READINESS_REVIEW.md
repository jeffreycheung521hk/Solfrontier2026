# Production Readiness Review — External Signer Bridge

**Reviewer:** Principal Engineer
**Scope:** `ExternalWalletStore`, `GatewayWalletSignatureHandler`, `submit_wallet_signature` HTTP route
**Date:** 2026-03-19
**Verdict:** NOT PRODUCTION-READY. Acceptable for single-operator devnet/testnet. Critical gaps must be resolved before mainnet.

---

## 1. Executive Summary

The external signer bridge is **functionally correct** for the happy path. Verification is sound (message-bytes + ed25519), the typestate pipeline is preserved, and the test suite (108 tests) covers correctness thoroughly. However, the implementation has **6 critical production gaps** related to durability, atomicity, expiry, and session-binding enforcement at the submit boundary.

---

## 2. Issue Registry

### P0 — Must Fix Before Production

#### P0-1: Non-atomic read-verify-take window in `submit_signed_tx_inner`

**Location:** `daemon.rs:816-926`

**Problem:** The daemon performs 4 separate DashMap reads (`get()`, `rejection_metadata()`, `expected_signer()`) followed by a `take()` on line 926 — with verification and audit I/O in between. Between `get()` at line 816 and `take()` at line 926, a concurrent request with the same `request_id` can also pass `get()`, pass verification, and both callers proceed to `take()`.

**Race window:**
```
Thread A: get() → ✓ (has bytes)     → verify → ✓     → take() → Some(entry)
Thread B: get() → ✓ (same bytes!)   → verify → ✓     → take() → None (already taken)
```

**Impact:** Thread B's `take()` returns `None`, so `transaction_id` becomes `Uuid::nil()` and `fee_lamports` becomes `None`. The spend is **not recorded** for Thread B, but it still emits events with nil transaction_id. More critically: Thread A and Thread B both return `accepted: true` to the HTTP caller — the client sees a double-accept even though only one spend is recorded.

**Concrete risk:** Client retries the same signed_tx_b64 (network timeout, retry middleware). Both return `accepted: true`. Frontend shows two "success" confirmations. Audit shows one `wallet_signature_received` with a real transaction_id and one with `Uuid::nil()`.

**Fix:** Replace the multi-step `get() → verify → take()` with a single atomic `take_and_verify()`:

```rust
/// Atomically removes the entry and verifies the signed tx.
/// If verification fails, the entry is NOT returned to the map.
pub fn take_and_verify(
    &self,
    request_id: &Uuid,
    signed_tx_bytes: &[u8],
) -> Result<(Transaction, PendingWalletSignature), SubmitError> {
    let entry = self.pending.remove(request_id)
        .map(|(_, v)| v)
        .ok_or(SubmitError::NotFound)?;

    // From here, we own the entry exclusively. No race possible.
    let signed_tx = bincode::deserialize(signed_tx_bytes)...;
    let original_tx = bincode::deserialize(&entry.tx_bytes)...;
    verify_signed_tx(&original_tx, &signed_tx, &expected)?;

    // Signal the waiting task
    if let Some(tx) = entry.response_tx { tx.send(Some(signed_tx.clone())); }

    Ok((signed_tx, entry))
}
```

**Severity:** P0. Under concurrent HTTP requests, produces incorrect audit and double-accept responses.

---

#### P0-2: No request expiry — unbounded memory growth + stale blockhash acceptance

**Location:** `ExternalWalletStore::park()` stores no timestamp. No reaper task exists.

**Problem:** Parked entries live forever. If the client never submits the signed tx:
- Memory grows without bound (each entry holds ~1KB of serialized tx + metadata)
- The blockhash in the parked tx expires after ~90 seconds (150 slots)
- A client could submit a "valid" signed tx hours later — it passes verification but would fail on-chain submission (blockhash expired)
- The oneshot channel's receiver side is held by a spawned task, which is **permanently blocked**

**Impact:**
- Memory leak proportional to abandoned sign requests
- Tokio task leak (each parked request holds a blocked spawned task via the oneshot)
- If on-chain submission is later added, stale-blockhash txs would silently fail

**Fix:** Add `parked_at: Instant` to `PendingWalletSignature`. Add a reaper task:

```rust
const SIGN_REQUEST_TTL: Duration = Duration::from_secs(120); // ~blockhash lifetime

async fn reap_expired(store: ExternalWalletStore, interval: Duration) {
    let mut ticker = tokio::time::interval(interval);
    loop {
        ticker.tick().await;
        let now = Instant::now();
        let expired: Vec<Uuid> = store.pending.iter()
            .filter(|e| now.duration_since(e.parked_at) > SIGN_REQUEST_TTL)
            .map(|e| *e.key())
            .collect();
        for id in expired {
            store.reject(&id);
            store.remove(&id);
            // emit WalletSignatureExpired event
        }
    }
}
```

**Severity:** P0. Memory + task leak under production load. Silent acceptance of stale-blockhash txs.

---

#### P0-3: Submit endpoint does not verify session-request binding

**Location:** `daemon.rs:782-992`, `wallet_signatures.rs:57-93`

**Problem:** The `POST /sessions/:id/wallet-signatures` endpoint receives `session_id` from the URL path and `request_id` from the body. But the daemon handler (`submit_signed_tx_inner`) **never checks** that the `request_id` actually belongs to the session in the URL path. Any authenticated caller who knows a valid `request_id` can submit a signed tx against it via any session.

**Attack scenario:**
1. Session A parks tx for wallet W1 → gets `request_id = X`
2. Attacker (has the bearer token) calls `POST /sessions/B/wallet-signatures` with `request_id = X`
3. The handler processes it. The `session_id` passed to audit/events is session B (from URL), but the actual tx belongs to session A

**Impact:** Cross-session spoofing of wallet signatures. Audit trail records wrong session. Events routed to wrong SSE subscriber.

**Fix:** After retrieving the parked entry, verify `entry.proposal.session_id == session_id`:

```rust
let parked_session = self.external_wallet.parked_session(&request_id);
if parked_session.as_ref() != Some(session_id) {
    return WalletSignatureOutcome { accepted: false, error: "request does not belong to this session" };
}
```

**Severity:** P0. Session boundary violation. Audit integrity breach.

---

### P1 — High Priority

#### P1-1: All pending state is in-memory — lost on process restart

**Location:** `ExternalWalletStore`, `PendingSigningStore`

**Problem:** Both stores use `DashMap` with no persistence. A daemon restart (crash, deploy, OOM kill) drops all pending wallet signature requests and all pending approval requests. The spawned tasks awaiting oneshot channels are killed. The client has a valid `request_id` and a valid signed transaction, but no way to submit it.

**Impact:** User signs a tx in Phantom, switches back to the app, the daemon restarted — the signed tx is rejected with "not found". User must redo the entire flow. For high-value transactions with human approval, the operator must re-approve.

**Fix:** See V2 Architecture section below.

---

#### P1-2: No rate limiting on submit endpoint

**Location:** `wallet_signatures.rs:57-93`

**Problem:** The submit endpoint performs:
- Base64 decode of arbitrary-length input
- Bincode deserialization (CPU-bound, can be DoS'd with crafted payloads)
- Ed25519 signature verification (CPU-intensive)
- 4 DashMap lookups
- 2 SQLite writes (audit + spend)

There is no rate limit, no request size limit, and no per-session concurrency limit.

**Attack:** Attacker sends 10K concurrent requests with random `request_id`s. Each goes through base64 decode + bincode deserialize before hitting the "not found" check. With crafted payloads, bincode deserialization can be CPU-expensive.

**Fix:** Add middleware: max body size (e.g., 2KB for a signed tx), per-session concurrency limit (e.g., 5), global rate limit.

---

#### P1-3: `bind_wallet` has no ownership proof

**Location:** `wallet_signatures.rs:119-146`

**Problem:** `POST /sessions/:id/bind-wallet` accepts any pubkey string. There is no challenge-response or signature proof that the caller actually controls the private key for that pubkey. Any authenticated API caller can bind any pubkey to any session.

**Impact:** An attacker can bind a victim's wallet pubkey to their own session, causing the signing tool to route transactions through the external path instead of the local path. This doesn't directly enable fund theft (the attacker can't sign), but it can cause denial-of-service by preventing legitimate transactions from being processed via the local signer.

**Fix:** Require a signed challenge:
```
POST /sessions/:id/bind-wallet
{
    "pubkey": "...",
    "challenge": "<random nonce from server>",
    "signature": "<ed25519 sig of challenge by pubkey>"
}
```

---

#### P1-4: Events are fire-and-forget — no delivery guarantee

**Location:** `EventBus::publish()` returns `usize` (receiver count), errors silently dropped

**Problem:** `broadcast::Sender::send()` returns `Err` if no receivers exist, and receivers can lag and lose events. The daemon does not check whether event delivery succeeded. If the SSE connection drops momentarily, the client misses the `WalletSignatureReceived` event and has no way to know the tx was accepted.

**Impact:** Client shows "pending" forever even though the tx was accepted. No retry mechanism.

**Fix:** Events should be persisted to an event log table before publishing to the broadcast channel. SSE consumers should be able to replay from a cursor. See V2 Architecture.

---

### P2 — Nice to Have

#### P2-1: `pending_for_session()` iterates entire DashMap

O(n) scan. Fine for V1 (small N), but add a session index for V2.

#### P2-2: No metrics or counters exposed

No Prometheus/OpenTelemetry metrics for: pending request count, submit latency, verification failure rate, reaper evictions. Essential for production alerting.

#### P2-3: Signed tx is not submitted on-chain

The current flow stops after verification. The daemon returns `accepted: true` but never calls `send_transaction()` on the Solana RPC. This is a documented V1 limitation but is obviously required for production.

#### P2-4: No idempotency key on the submit endpoint

If the client retries a successful submit (network timeout on the 202 response), the second call returns "not found" (entry was taken). The client can't distinguish "already succeeded" from "never existed". An idempotency key would let the server return the cached outcome.

#### P2-5: `complete()` and `take()` are separate operations

In `submit_signed_transaction()`, `complete()` signals the oneshot and `take()` removes the entry. Between them, another caller could observe the entry (via `get()`) but find the oneshot already consumed. This is mitigated by P0-1's fix (atomic take-and-verify).

---

## 3. V2 Architecture Proposal

### 3.1 Persistent Pending Store

Replace in-memory `DashMap` with SQLite-backed store + in-memory index:

```sql
CREATE TABLE pending_wallet_signatures (
    request_id       TEXT PRIMARY KEY,
    session_id       TEXT NOT NULL,
    transaction_id   TEXT NOT NULL,
    wallet_pubkey    TEXT NOT NULL,
    tx_bytes         BLOB NOT NULL,            -- bincode-serialized unsigned Transaction
    simulation_json  TEXT NOT NULL,             -- JSON-serialized SimulationResult
    policy_verdict   TEXT NOT NULL,             -- label string
    proposal_json    TEXT NOT NULL,             -- JSON-serialized TransactionProposal
    status           TEXT NOT NULL DEFAULT 'pending',  -- pending | completed | rejected | expired
    created_at       INTEGER NOT NULL,         -- epoch millis
    expires_at       INTEGER NOT NULL,         -- epoch millis (created_at + TTL)
    completed_at     INTEGER,                  -- epoch millis
    outcome_signature TEXT,                    -- on-chain sig if completed
    outcome_error    TEXT,                     -- rejection reason
    FOREIGN KEY (session_id) REFERENCES sessions(id)
);

CREATE INDEX idx_pws_session ON pending_wallet_signatures(session_id, status);
CREATE INDEX idx_pws_expires ON pending_wallet_signatures(expires_at) WHERE status = 'pending';
```

**Status transitions (enforced by SQL):**
```
pending → completed  (verified signed tx accepted)
pending → rejected   (verification failed)
pending → expired    (TTL exceeded, reaper)
completed → (terminal)
rejected → (terminal)
expired → (terminal)
```

**Update uses optimistic locking:**
```sql
UPDATE pending_wallet_signatures
SET status = 'completed', completed_at = ?, outcome_signature = ?
WHERE request_id = ? AND status = 'pending'
```
Returns `rows_affected = 0` if already terminal → atomic exactly-once.

### 3.2 Session-Wallet Binding Table

```sql
CREATE TABLE session_wallet_bindings (
    session_id    TEXT NOT NULL,
    wallet_pubkey TEXT NOT NULL,
    bound_at      INTEGER NOT NULL,
    challenge     TEXT,                -- challenge nonce for proof-of-ownership
    verified      BOOLEAN NOT NULL DEFAULT false,
    PRIMARY KEY (session_id, wallet_pubkey)
);
```

### 3.3 Idempotency Key Strategy

Add an `idempotency_key` header to `POST /wallet-signatures`:

```sql
CREATE TABLE idempotency_cache (
    idempotency_key TEXT PRIMARY KEY,
    request_id      TEXT NOT NULL,
    response_json   TEXT NOT NULL,
    created_at      INTEGER NOT NULL,
    expires_at      INTEGER NOT NULL   -- TTL e.g. 24 hours
);
```

Flow:
1. Client sends `Idempotency-Key: <uuid>` header
2. Server checks cache: if hit, return cached response immediately
3. If miss, process request, store response in cache, return

### 3.4 Event Durability Model

```sql
CREATE TABLE event_log (
    sequence_id    INTEGER PRIMARY KEY AUTOINCREMENT,
    event_type     TEXT NOT NULL,
    session_id     TEXT,
    payload_json   TEXT NOT NULL,
    created_at     INTEGER NOT NULL
);

CREATE INDEX idx_events_session ON event_log(session_id, sequence_id);
```

SSE endpoint accepts `?after=<sequence_id>` to replay missed events. Broadcast channel is still used for real-time push, but the event log is the source of truth for replay.

### 3.5 Timeout / Retry / Cleanup Model

| Component | TTL | Cleanup |
|---|---|---|
| Pending wallet signatures | 120s (blockhash lifetime) | Reaper task every 30s |
| Pending human approvals | 300s (configurable) | Reaper task every 60s |
| Idempotency cache | 24h | Reaper task every 1h |
| Event log | 7d | Reaper task every 6h |
| Session-wallet bindings | Session lifetime | Cleanup on session close |

### 3.6 Metrics to Expose

```
# Counters
claw_wallet_signatures_submitted_total{status="accepted|rejected|expired|not_found"}
claw_wallet_signatures_parked_total
claw_wallet_bindings_total{action="bind|unbind"}
claw_wallet_verification_failures_total{reason="message_mismatch|signer_not_found|did_not_sign|invalid_signature"}

# Gauges
claw_wallet_signatures_pending_count
claw_wallet_signatures_oldest_pending_age_seconds

# Histograms
claw_wallet_signature_submit_duration_seconds
claw_wallet_signature_park_to_submit_duration_seconds
claw_wallet_verification_duration_seconds
```

---

## 4. Recommended Fix Order

| Order | Issue | Effort | Risk if Skipped |
|-------|-------|--------|-----------------|
| 1 | P0-1 (atomic take-and-verify) | Small | Double-accept, audit corruption |
| 2 | P0-3 (session-request binding check) | Small | Cross-session spoofing |
| 3 | P0-2 (request expiry + reaper) | Medium | Memory/task leak, stale blockhash |
| 4 | P1-2 (rate limiting) | Small | DoS vector |
| 5 | P1-1 (persistent store) | Large | State loss on restart |
| 6 | P1-3 (bind ownership proof) | Medium | DoS via binding hijack |
| 7 | P1-4 (event durability) | Medium | Missed events |
| 8 | P2-4 (idempotency key) | Medium | Client confusion on retry |

---

## 5. What Is Already Good

- **Verification logic is sound.** 4-step verification (message bytes → signer presence → non-default sig → ed25519) is the correct order and covers all tampering vectors.
- **Spend tracking is idempotent.** `INSERT OR IGNORE` with `UNIQUE(transaction_id)` prevents double-count even under races.
- **Audit trail is append-only.** No update/delete operations possible.
- **Typestate pipeline is preserved.** External signer path still flows through `simulate → policy → (approve) → park`, same as local signer.
- **Test coverage is strong.** 108 tests including concurrency races, restart resilience, full approval flow, and HTTP integration.
- **Finalized tx invariant is correct.** The tx handed off to the client is the post-pipeline tx (with compute budget, blockhash set), not the original draft.
