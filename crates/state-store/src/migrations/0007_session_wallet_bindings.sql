-- Session-wallet bindings persistence (C4).
--
-- When an external wallet is bound to a session (via challenge-response
-- or direct bind), the binding is persisted here so it survives daemon
-- restarts. On startup, the daemon loads all active bindings back into
-- the in-memory ExternalWalletStore.
--
-- UNIQUE constraint prevents duplicate (session, pubkey) pairs.
-- The unbind_session() call deletes all rows for a session on close.

CREATE TABLE IF NOT EXISTS session_wallet_bindings (
    session_id      TEXT    NOT NULL,
    wallet_pubkey   TEXT    NOT NULL,
    created_at      INTEGER NOT NULL,
    PRIMARY KEY (session_id, wallet_pubkey)
);

CREATE INDEX IF NOT EXISTS idx_session_wallet_bindings_session
    ON session_wallet_bindings(session_id);
