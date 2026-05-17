-- P1-1 hardening: Add lifecycle state to pending tables.
--
-- Previously, rows were DELETE'd on terminal transitions. This made it
-- impossible to distinguish "never existed" from "was consumed" after a
-- crash. Adding an explicit state column lets us:
--   1. Mark rows as consumed/expired/rejected BEFORE removing from memory
--   2. Only recover rows in 'pending' state on startup
--   3. Clean up terminal rows lazily without resurrection risk
--
-- States:
--   'pending'  — live, recoverable on restart
--   'consumed' — terminal: decision made / signature completed
--   'expired'  — terminal: TTL exceeded
--   'rejected' — terminal: verification failed or operator rejected

ALTER TABLE pending_approvals ADD COLUMN state TEXT NOT NULL DEFAULT 'pending';
ALTER TABLE pending_signatures ADD COLUMN state TEXT NOT NULL DEFAULT 'pending';
ALTER TABLE pending_completion_meta ADD COLUMN state TEXT NOT NULL DEFAULT 'pending';

-- Terminal reason (optional, for audit correlation).
ALTER TABLE pending_approvals ADD COLUMN terminal_reason TEXT;
ALTER TABLE pending_signatures ADD COLUMN terminal_reason TEXT;

-- Indexes for recovery queries (only load 'pending' rows).
CREATE INDEX IF NOT EXISTS idx_pending_approvals_state
    ON pending_approvals(state);
CREATE INDEX IF NOT EXISTS idx_pending_signatures_state
    ON pending_signatures(state);
CREATE INDEX IF NOT EXISTS idx_pending_completion_meta_state
    ON pending_completion_meta(state);
