-- ClawSolana spend tracking ledger
-- Migration: 0002_spend_ledger
--
-- Append-only ledger recording actual spend when transactions are signed.
-- Used by SpendRepository for accumulation queries in policy evaluation.
--
-- Recording point: Signed stage (when signer produces a signature).
-- Spend unit: lamports (fee_lamports from simulation result).
--
-- Double-count protection: UNIQUE(transaction_id) — each transaction can
-- only produce one spend entry. INSERT OR IGNORE is used at the repository
-- level to safely handle duplicate recording attempts.

CREATE TABLE IF NOT EXISTS spend_ledger (
    id                TEXT    PRIMARY KEY NOT NULL,
    wallet_pubkey     TEXT    NOT NULL,
    session_id        TEXT    NOT NULL,
    transaction_id    TEXT    NOT NULL UNIQUE,
    -- Lamports spent (fee_lamports from simulation) — the actual on-chain cost.
    lamports_spent    INTEGER NOT NULL,
    -- UTC date YYYY-MM-DD for daily bucketing.
    recorded_date     TEXT    NOT NULL,
    -- When this spend entry was recorded (Unix epoch milliseconds).
    recorded_at       INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_spend_wallet       ON spend_ledger(wallet_pubkey);
CREATE INDEX IF NOT EXISTS idx_spend_session      ON spend_ledger(session_id);
CREATE INDEX IF NOT EXISTS idx_spend_date         ON spend_ledger(recorded_date);
CREATE INDEX IF NOT EXISTS idx_spend_wallet_date  ON spend_ledger(wallet_pubkey, recorded_date);
CREATE INDEX IF NOT EXISTS idx_spend_tx           ON spend_ledger(transaction_id);
