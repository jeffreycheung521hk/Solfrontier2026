# ClawSolana — Current Situation

**Date:** 2026-03-22
**Phase:** Phase 1.4 — Execution Completeness (E2E devnet smoke test)
**Build:** `cargo check` PASS, `cargo test` PASS (274 tests, zero failures)

---

## What Works ✅

### Agent Tool Chain
- `build_transfer → simulate_transaction → submit_for_signing` — all three tool calls succeed
- `tool_count: 11` — `submit_for_signing` injected via wallet config
- LLM (`gpt-4o-mini`) correctly calls tools instead of chatting (execution prompt fixed)
- `finish_reason: tool_calls` confirmed in daemon log

### Local Wallet Signing
- Auto-sign path fully functional
- Transaction signed and signature returned to agent
- Devnet wallet loaded: `ByZH6oQ7kZ5h32t5dJsbayLGAyKK8YetWvoLTHqzdkma`

### External Wallet (Phantom) — Partial
- Phantom Connect: ✅
- Wallet Bind (challenge-response): ✅
- Pending signing request created: ✅
- Bridge displays pending request: ✅
- Phantom sign dialog appears: ✅
- Phantom signs transaction: ✅
- **POST signed tx back to backend: ❌ (400 Bad Request)**

### Backend Hardening
- `tracking_repo.insert()` failure prevents `tracker.track()` (no in-memory-only state)
- `submission_block_height` never writes 0 (poison value eliminated)
- `last_valid_block_height` threaded from pipeline through `ParkedApproval` into `CompletionMeta`
- Final fallback guard: `if fallback == 0 { 1 } else { fallback }`
- 10 regression tests in `n21_submission_invariants.rs`

---

## Current Blocker ❌

### Phantom signed tx rejected: `transaction message bytes modified`

**Root cause confirmed by test:**

```
bincode_msg == message_data: false
bincode_msg[0..10]: [00, 00, 00, 00, 01, 00, 00, 00, 00, 00]  ← bincode 8-byte Vec prefix
msg_data[0..10]:    [01, 00, 01, 03, 00, 00, 00, 00, 00, 00]  ← compact-u16 wire format
```

**`bincode::serialize(Transaction)` ≠ Solana wire format.**

| Component | Vec Prefix | Format Name |
|-----------|-----------|-------------|
| Rust `bincode::serialize` | 8-byte u64 LE | bincode |
| Solana `message_data()` | compact-u16 (1-3 bytes) | wire format |
| web3.js `Transaction.from()` | compact-u16 | wire format |
| Phantom `signTransaction()` | signs wire format message | wire format |

The mismatch exists not only in the signatures Vec prefix, but also **inside the Message** (account_keys Vec, instructions Vec, instruction accounts Vec, instruction data Vec).

### Latest verification log

```
orig_msg_len: 190, signed_msg_len: 202, msg_match: false
```

Backend sends 190 bytes (from `message_data()`), browser round-trips to 202 bytes. The 12-byte difference suggests web3.js `Transaction.from()` → `serialize()` recompiles the message layout.

---

## Current Architecture (after changes)

```
Storage:         finalized_tx_bytes = bincode (unchanged)
API Output:      pending_for_session: bincode → Transaction → message_data() → wire format
Browser:         Transaction.from(wire) → Phantom sign → signed.serialize() → wire format
API Input:       daemon passes wire bytes directly to orchestrator
Verification:    original: bincode → Transaction → message_data() (wire)
                 signed:   parse wire → extract sigs + message
                 compare:  wire message bytes
                 ed25519:  verify against wire message bytes
RPC Submission:  original bincode Transaction + new signatures → bincode::serialize
```

---

## Next Step

**Debug the wire format round-trip.** A debug log has been added to `bridge/index.html`:

```javascript
const rtTx = Transaction.from(txBytes);
const rtBytes = rtTx.serialize({requireAllSignatures: false, verifySignatures: false});
log(`Round-trip: in=${txBytes.length} out=${rtBytes.length} match=...`);
```

On next browser refresh + Refresh button, this will show:

- If `match=false` → the wire format bytes constructed by `pending_for_session` are malformed. Fix the manual wire construction.
- If `match=true` → `Transaction.from()` preserves bytes, but `Phantom.signTransaction()` modifies the message. Would need to graft signatures onto original wire bytes.

**Most likely cause:** The manual wire format construction in `pending_for_session` (daemon.rs) has a subtle bug — possibly message_data() includes a different compact-u16 encoding than what web3.js expects for the outer transaction wrapper.

**Potential fix:** Instead of manually constructing wire bytes, use a tested Solana SDK method that produces correct wire format output, or roundtrip through web3.js on the backend side.

---

## Files Modified

| File | Change |
|------|--------|
| `config/default.toml` | Enabled wallet config with absolute keypair path |
| `crates/agent-runtime/src/personas/execution.rs` | Execution prompt: tool-first, no chatbot behavior |
| `crates/agent-runtime/src/llm/anthropic.rs` | Added `stop_reason` log |
| `crates/agent-runtime/src/llm/openai.rs` | Added `finish_reason` log |
| `crates/gateway/src/tools/signing.rs` | `finalized_tx_bytes` storage remains bincode; `CompletionMeta` uses `parked.last_valid_block_height` |
| `crates/gateway/src/daemon.rs` | `pending_for_session` outputs wire format; submit handler passes wire bytes; `submission_block_height` fallback guard |
| `crates/gateway/src/orchestrator/external_adapter.rs` | Verification uses wire format message comparison + ed25519 verify |
| `crates/gateway/src/pending_signing.rs` | Added `last_valid_block_height` field to `ParkedApproval` |
| `crates/gateway/src/durable_pending.rs` | Recovery path passes `last_valid_block_height = 0` with guard downstream |
| `crates/solana-core/src/rpc/client.rs` | Added `get_block_height()` method |
| `bridge/index.html` | UMD bundle for web3.js, `Transaction.from()` + `signed.serialize()`, debug logs |
| `crates/gateway/tests/n21_submission_invariants.rs` | 10 regression tests + bincode vs wire format comparison test |

---

## What Does NOT Need Fixing

- Submission tracking (`tracking_repo.insert` → `tracker.track`) — hardened, 10 tests
- `submission_block_height` zero poison value — eliminated across all paths
- Exactly-once consume semantics — unchanged, working
- Challenge-response wallet binding — working
- SSE event streaming — working (query param auth fallback for EventSource)
- All 274 tests pass, zero failures
