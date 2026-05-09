-- Durable storage for Stage 2 watch rules.
--
-- Stage 2 = delegated conditional execution. The off-chain daemon needs
-- a place to keep the user-authorized rule, its canonical hash + bytes
-- (so the on-chain comparator and the daemon agree on what was signed),
-- and the rule lifecycle state (active / condition_met / executing /
-- completed / expired / revoked / failed).
--
-- This is substrate only — no scheduler, no transaction execution, no
-- live RPC. Spec source-of-truth:
--   docs/STAGE2_DELEGATED_CONDITIONAL_EXECUTION_SPEC.md
--   crates/types/src/stage2_watch_rule.rs (canonical schema + hash)
--
-- u64 storage decision: amounts (`max_input_amount_raw`,
-- `used_amount_raw`) and slot counters are stored as INTEGER (SQLite
-- 8-byte signed int). This matches the existing repo style
-- (pending_jupiter_swaps.input_amount, transactions.* slots). All
-- realistic Solana raw amounts (USDC 6dp, SOL 9dp) and slot numbers
-- fit comfortably below i64::MAX (~9.2e18). The repository casts
-- u64 ↔ i64 at the boundary; values strictly above i64::MAX are not
-- representable here and are out of scope for v1.

CREATE TABLE IF NOT EXISTS stage2_watch_rules (
    rule_id                    TEXT    PRIMARY KEY,           -- 32-char hex of WatchRule.rule_id ([u8; 16])
    user_pubkey                TEXT    NOT NULL,              -- base58
    executor_pubkey            TEXT    NOT NULL,              -- base58
    delegated_wallet_pubkey    TEXT    NOT NULL,              -- base58
    canonical_rule_hash        TEXT    NOT NULL,              -- 64-char hex of SHA-256(canonical bytes)
    canonical_rule_bytes_hex   TEXT    NOT NULL,              -- hex of borsh canonical bytes
    rule_json                  TEXT    NOT NULL,              -- canonical serde_json::to_string(WatchRule)
    action_type                TEXT    NOT NULL,              -- WatchRuleActionType::label()
    condition_logic            TEXT    NOT NULL,              -- "all" | "any"
    status                     TEXT    NOT NULL DEFAULT 'active',
    -- active | condition_met | executing | completed | expired | revoked | failed
    max_input_amount_raw       INTEGER NOT NULL,              -- u64 stored as i64
    used_amount_raw            INTEGER NOT NULL DEFAULT 0,    -- u64 stored as i64
    destination_pubkey         TEXT    NOT NULL,              -- base58
    expires_at_slot            INTEGER NOT NULL,
    created_at_slot            INTEGER NOT NULL,
    last_checked_slot          INTEGER,
    last_successful_tick_at_ms INTEGER,
    last_error                 TEXT,
    execution_nonce            INTEGER NOT NULL DEFAULT 0,
    completed                  INTEGER NOT NULL DEFAULT 0,    -- 0/1 bool
    revoked                    INTEGER NOT NULL DEFAULT 0,    -- 0/1 bool
    created_at_ms              INTEGER NOT NULL,
    updated_at_ms              INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_stage2_watch_rules_user    ON stage2_watch_rules(user_pubkey);
CREATE INDEX IF NOT EXISTS idx_stage2_watch_rules_status  ON stage2_watch_rules(status);
CREATE INDEX IF NOT EXISTS idx_stage2_watch_rules_expires ON stage2_watch_rules(expires_at_slot);
