-- ClawSolana initial schema
-- Migration: 0001_initial
-- All timestamps are stored as INTEGER (Unix epoch milliseconds).
-- All JSON fields are TEXT with JSON content.
-- The audit_events table is append-only by convention (enforced in the repository).

-- ─── Sessions ────────────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS sessions (
    id              TEXT    PRIMARY KEY NOT NULL,
    state           TEXT    NOT NULL DEFAULT 'active',
    agent_role      TEXT    NOT NULL,
    channel         TEXT    NOT NULL,
    -- JSON array of capability strings
    capabilities    TEXT    NOT NULL DEFAULT '[]',
    -- JSON array of wallet pubkeys this session can touch (null = any)
    wallet_scope    TEXT,
    created_at      INTEGER NOT NULL,
    closed_at       INTEGER,
    message_count   INTEGER NOT NULL DEFAULT 0,
    tool_call_count INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_sessions_state      ON sessions(state);
CREATE INDEX IF NOT EXISTS idx_sessions_created_at ON sessions(created_at);

-- ─── Audit log (append-only) ─────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS audit_events (
    id              TEXT    PRIMARY KEY NOT NULL,
    session_id      TEXT,
    correlation_id  TEXT    NOT NULL,
    occurred_at     INTEGER NOT NULL,
    event_type      TEXT    NOT NULL,
    actor           TEXT    NOT NULL,
    -- JSON payload (sanitized — no key material)
    payload         TEXT    NOT NULL DEFAULT '{}',
    severity        TEXT    NOT NULL DEFAULT 'info'
);

CREATE INDEX IF NOT EXISTS idx_audit_session    ON audit_events(session_id);
CREATE INDEX IF NOT EXISTS idx_audit_type       ON audit_events(event_type);
CREATE INDEX IF NOT EXISTS idx_audit_occurred   ON audit_events(occurred_at);
CREATE INDEX IF NOT EXISTS idx_audit_corr       ON audit_events(correlation_id);

-- ─── Tool traces ─────────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS tool_traces (
    id              TEXT    PRIMARY KEY NOT NULL,
    session_id      TEXT    NOT NULL,
    correlation_id  TEXT    NOT NULL,
    tool_name       TEXT    NOT NULL,
    started_at      INTEGER NOT NULL,
    finished_at     INTEGER,
    status          TEXT    NOT NULL DEFAULT 'running',
    -- Sanitized input/output JSON
    input           TEXT    NOT NULL DEFAULT '{}',
    output          TEXT,
    error           TEXT,
    duration_ms     INTEGER,
    FOREIGN KEY (session_id) REFERENCES sessions(id)
);

CREATE INDEX IF NOT EXISTS idx_tool_traces_session  ON tool_traces(session_id);
CREATE INDEX IF NOT EXISTS idx_tool_traces_tool     ON tool_traces(tool_name);
CREATE INDEX IF NOT EXISTS idx_tool_traces_started  ON tool_traces(started_at);

-- ─── Transaction records ─────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS transaction_records (
    id                  TEXT    PRIMARY KEY NOT NULL,
    session_id          TEXT    NOT NULL,
    wallet_pubkey       TEXT    NOT NULL,
    network             TEXT    NOT NULL,
    status              TEXT    NOT NULL DEFAULT 'proposed',
    description         TEXT    NOT NULL DEFAULT '',
    -- Full JSON-serialized TransactionProposal
    proposal            TEXT    NOT NULL DEFAULT '{}',
    -- JSON SimulationResult, nullable
    simulation_result   TEXT,
    -- JSON PolicyVerdict, nullable
    policy_verdict      TEXT,
    -- On-chain transaction signature, nullable until sent
    signature           TEXT,
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL,
    FOREIGN KEY (session_id) REFERENCES sessions(id)
);

CREATE INDEX IF NOT EXISTS idx_tx_session   ON transaction_records(session_id);
CREATE INDEX IF NOT EXISTS idx_tx_wallet    ON transaction_records(wallet_pubkey);
CREATE INDEX IF NOT EXISTS idx_tx_status    ON transaction_records(status);
CREATE INDEX IF NOT EXISTS idx_tx_sig       ON transaction_records(signature);
CREATE INDEX IF NOT EXISTS idx_tx_created   ON transaction_records(created_at);

-- ─── Wallet registry ─────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS wallets (
    pubkey      TEXT    PRIMARY KEY NOT NULL,
    label       TEXT,
    signer_type TEXT    NOT NULL,
    network     TEXT    NOT NULL,
    created_at  INTEGER NOT NULL,
    is_active   INTEGER NOT NULL DEFAULT 1
);

-- ─── Active subscriptions ────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS subscriptions (
    id          TEXT    PRIMARY KEY NOT NULL,
    sub_type    TEXT    NOT NULL,
    -- pubkey, program ID, or filter expression
    target      TEXT    NOT NULL,
    network     TEXT    NOT NULL,
    created_at  INTEGER NOT NULL,
    is_active   INTEGER NOT NULL DEFAULT 1
);

-- ─── Alert history ───────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS alerts (
    id           TEXT    PRIMARY KEY NOT NULL,
    session_id   TEXT,
    alert_type   TEXT    NOT NULL,
    severity     TEXT    NOT NULL,
    message      TEXT    NOT NULL,
    payload      TEXT    NOT NULL DEFAULT '{}',
    occurred_at  INTEGER NOT NULL,
    acknowledged INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_alerts_severity ON alerts(severity);
CREATE INDEX IF NOT EXISTS idx_alerts_occurred ON alerts(occurred_at);
CREATE INDEX IF NOT EXISTS idx_alerts_ack      ON alerts(acknowledged);
