-- W5h — chat-budget funding + 3-minute expiry / refund state machine.
--
-- An intent is the W5h orthogonal layer between the W5e/W5f watch rule
-- and the W5g live deposit. It tracks:
--
--   * the operator-approved budget (always 250 000 raw USDC in this slice)
--   * the user-paid funding tx (after Phantom signs)
--   * the lease that prevents execution vs. refund double-spend
--   * the terminal-of-record (completed | refunded | failed)
--
-- One intent per rule_id. The intent_id IS the rule_id_hex (32 hex chars)
-- so the W5g executor can look up the gate by the same key it already
-- uses for the WatchRule canonical-hash check.
--
-- # Race-safety contract (W5h addendum §3)
--
-- All non-trivial transitions are CAS-guarded by `WHERE status = ?` so
-- a wrong-state attempt updates zero rows. Caller treats `rows_affected
-- != 1` as a typed lease-failure.
--
-- Lease transitions:
--   funding_required    → funding_submitted   (user POSTs the signature)
--   funding_submitted   → budget_reserved     (tx finalized + 250 000 delta)
--   funding_submitted   → funding_invalid     (tx finalized + delta wrong)
--   budget_reserved     → executing           (W5g acquires execution lease)
--   executing           → completed           (W5g tx finalized)
--   executing           → failed              (W5g tx failed on-chain)
--   budget_reserved     → expired             (now >= expires_at_ms;
--                                              swept by read-side or worker)
--   expired             → refunding           (W5h acquires refund lease)
--   refunding           → refunded            (refund tx finalized)
--   refunding           → failed              (refund tx failed)
--
-- The execution and refund leases are mutually exclusive. Whichever
-- transaction wins the CAS holds the budget; the loser sees a typed
-- "wrong status" error and returns without building any tx.

CREATE TABLE IF NOT EXISTS stage2_w5h_funding_intents (
    -- Identity (matches the W5e/W5f WatchRule.rule_id by construction).
    intent_id               TEXT    PRIMARY KEY,           -- 32-char hex
    rule_id_hex             TEXT    NOT NULL,              -- duplicate for clarity
    canonical_rule_hash_hex TEXT    NOT NULL,              -- 64-char hex, persisted

    -- Wallet topology — fixed at intent creation, never edited.
    user_wallet             TEXT    NOT NULL,              -- base58
    user_usdc_ata           TEXT    NOT NULL,
    controlled_wallet       TEXT    NOT NULL,
    controlled_usdc_ata     TEXT    NOT NULL,

    -- Budget — always 250 000 raw USDC in this slice. Stored as INTEGER
    -- (matches the pre-existing repo style: u64 ↔ i64 at the boundary).
    amount_raw              INTEGER NOT NULL,

    -- Decision-metric snapshot at chat-creation time.
    threshold_bps                                INTEGER NOT NULL,
    save_display_apy_bps_at_creation             INTEGER NOT NULL,
    native_onchain_apr_bps_at_creation           INTEGER NOT NULL,

    -- Timing.
    created_at_ms           INTEGER NOT NULL,
    expires_at_ms           INTEGER NOT NULL,              -- created + 180 000 ms

    -- Lifecycle status.
    status                  TEXT    NOT NULL DEFAULT 'funding_required',
    -- Allowed values (also enforced in Rust via WatchW5hIntentStatus::parse):
    --   funding_required
    --   funding_submitted
    --   funding_invalid     (terminal — user must mint a new intent)
    --   budget_reserved
    --   executing
    --   completed           (terminal)
    --   expired
    --   refunding
    --   refunded            (terminal)
    --   failed              (terminal)

    -- Signatures captured along the way (all base58 strings; NULL until set).
    funding_signature       TEXT,
    funding_finalized_slot  INTEGER,
    execution_signature     TEXT,
    refund_signature        TEXT,

    -- Last error message (for `failed` / `funding_invalid` terminal states).
    last_error              TEXT,

    updated_at_ms           INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_w5h_funding_status     ON stage2_w5h_funding_intents(status);
CREATE INDEX IF NOT EXISTS idx_w5h_funding_expires_at ON stage2_w5h_funding_intents(expires_at_ms);
CREATE INDEX IF NOT EXISTS idx_w5h_funding_user       ON stage2_w5h_funding_intents(user_wallet);
