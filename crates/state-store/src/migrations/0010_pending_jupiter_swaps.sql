-- Durable state for Jupiter swap intents flowing through the approval workflow.
--
-- Stores the INTENT (mints, amounts, slippage), never transaction bytes,
-- blockhashes, lookup tables, or instruction payloads — those are JIT
-- build-time concerns and must not be frozen into the durable intent.

CREATE TABLE IF NOT EXISTS pending_jupiter_swaps (
    intent_id     TEXT    PRIMARY KEY,
    session_id    TEXT    NOT NULL,
    request_id    TEXT,                              -- approval request ID (NULL if auto-approved / rejected)
    wallet_pubkey TEXT    NOT NULL,
    network       TEXT    NOT NULL,
    input_mint    TEXT    NOT NULL,
    output_mint   TEXT    NOT NULL,
    input_amount  INTEGER NOT NULL,
    slippage_bps  INTEGER NOT NULL,
    description   TEXT,
    state         TEXT    NOT NULL DEFAULT 'pending_approval',
    -- pending_approval | approved_waiting_jit | jit_building | jit_build_failed
    -- | awaiting_wallet_signature | submitted | rejected | expired
    policy_verdict_json TEXT,                         -- JSON-serialized PolicyVerdict
    preview_json  TEXT,                               -- JSON-serialized SwapPreview (informational only)
    last_error    TEXT,                               -- error detail for state=rejected/expired
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_jupiter_swaps_session    ON pending_jupiter_swaps(session_id);
CREATE INDEX IF NOT EXISTS idx_jupiter_swaps_state      ON pending_jupiter_swaps(state);
CREATE INDEX IF NOT EXISTS idx_jupiter_swaps_request_id ON pending_jupiter_swaps(request_id);
