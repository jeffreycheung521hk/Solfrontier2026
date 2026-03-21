-- P1-1: Durable persistence for live pending workflow state.
--
-- Ensures pending approvals, pending signatures, and completion metadata
-- survive daemon restarts. All tables use TEXT primary keys (UUID strings).
-- Rows are removed when the request reaches a terminal state (decided,
-- completed, expired, rejected). No history is retained here — that
-- belongs to audit_events and spend_ledger.

-- Pending approval requests awaiting human decision.
-- Also carries the PendingSigningStore data (tx_bytes, simulation, verdict)
-- so that both ApprovalStore and PendingSigningStore can be reconstructed
-- from a single row on recovery.
CREATE TABLE IF NOT EXISTS pending_approvals (
    request_id      TEXT    PRIMARY KEY NOT NULL,
    session_id      TEXT    NOT NULL,
    transaction_id  TEXT    NOT NULL,
    description     TEXT    NOT NULL DEFAULT '',
    wallet_pubkey   TEXT    NOT NULL DEFAULT '',
    -- Full ApprovalRequest as JSON (includes policy_verdict, simulation, etc.)
    approval_json   TEXT    NOT NULL,
    -- Finalized transaction bytes (hex-encoded bincode). Needed to reconstruct
    -- PendingSigningStore on recovery.
    tx_bytes_hex    TEXT    NOT NULL,
    -- SimulationResult as JSON. Stored separately for recovery convenience.
    simulation_json TEXT    NOT NULL,
    -- PolicyVerdict as JSON.
    verdict_json    TEXT    NOT NULL,
    created_at      INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_pending_approvals_session
    ON pending_approvals(session_id);

-- Pending signature requests in the orchestrator (Execution Plane).
-- These are post-approval requests waiting for external wallet completion.
-- Local adapter requests complete synchronously and are never persisted here.
CREATE TABLE IF NOT EXISTS pending_signatures (
    request_id          TEXT    PRIMARY KEY NOT NULL,
    session_id          TEXT    NOT NULL,
    transaction_id      TEXT    NOT NULL,
    expected_signer     TEXT    NOT NULL,
    description         TEXT    NOT NULL DEFAULT '',
    finalized_tx_bytes  BLOB    NOT NULL,
    adapter_type        TEXT    NOT NULL,
    created_at          INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_pending_signatures_session
    ON pending_signatures(session_id);

-- Completion metadata (fee_lamports) stashed by the Control Plane for
-- spend recording when the external wallet signature arrives.
CREATE TABLE IF NOT EXISTS pending_completion_meta (
    request_id      TEXT    PRIMARY KEY NOT NULL,
    fee_lamports    INTEGER,
    created_at      INTEGER NOT NULL
);
