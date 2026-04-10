-- Approval workflow state machine persistence (Step 4).
--
-- Each approval request has an associated workflow that tracks its lifecycle
-- from Pending through to a terminal state (Approved/Rejected/Expired).
--
-- The `stages_json` column stores the ordered approval stages as a JSON array.
-- Step 4: always exactly one stage. Step 5 will add multi-stage support.
--
-- Rows remain after terminal state for audit correlation. They are never
-- deleted by the application (append-only lifecycle, matching audit semantics).

CREATE TABLE IF NOT EXISTS approval_workflows (
    request_id   TEXT    NOT NULL PRIMARY KEY,
    session_id   TEXT    NOT NULL,
    state        TEXT    NOT NULL DEFAULT 'pending',
    stages_json  TEXT    NOT NULL,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_approval_workflows_session
    ON approval_workflows(session_id);

CREATE INDEX IF NOT EXISTS idx_approval_workflows_state
    ON approval_workflows(state);
