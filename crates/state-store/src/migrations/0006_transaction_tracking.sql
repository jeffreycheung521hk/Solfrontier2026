-- Phase 1.4: Durable transaction lifecycle tracking.
--
-- Tracks transactions from Submitted to terminal state
-- (Finalized / Failed / Expired). Rows are kept for audit
-- correlation and restart recovery.

CREATE TABLE IF NOT EXISTS transaction_tracking (
    signature                TEXT    PRIMARY KEY NOT NULL,
    transaction_id           TEXT    NOT NULL,
    request_id               TEXT    NOT NULL,
    session_id               TEXT    NOT NULL,
    wallet_pubkey            TEXT    NOT NULL,
    state                    TEXT    NOT NULL DEFAULT 'submitted',
    last_valid_block_height  INTEGER NOT NULL,
    submission_block_height  INTEGER NOT NULL DEFAULT 0,
    slot                     INTEGER,
    err                      TEXT,
    confirmed_emitted        INTEGER NOT NULL DEFAULT 0,
    finalized_emitted        INTEGER NOT NULL DEFAULT 0,
    failed_emitted           INTEGER NOT NULL DEFAULT 0,
    dropped_emitted          INTEGER NOT NULL DEFAULT 0,
    expired_emitted          INTEGER NOT NULL DEFAULT 0,
    created_at               INTEGER NOT NULL,
    updated_at               INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_tracking_state
    ON transaction_tracking(state);

CREATE INDEX IF NOT EXISTS idx_tracking_session
    ON transaction_tracking(session_id);

CREATE INDEX IF NOT EXISTS idx_tracking_transaction
    ON transaction_tracking(transaction_id);
