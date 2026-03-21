-- Wallet ownership proof via challenge-response (Phase 1).
--
-- When a client wants to bind an external wallet to a session,
-- the server generates a nonce challenge. The client signs the
-- challenge message with their wallet. The server verifies the
-- ed25519 signature before calling bind_wallet().
--
-- Rows are consumed (used_at set) after successful verification.
-- Expired/consumed rows are kept for audit and cleaned by purge.

CREATE TABLE IF NOT EXISTS wallet_bind_challenges (
    id              TEXT    PRIMARY KEY NOT NULL,
    session_id      TEXT    NOT NULL,
    wallet_pubkey   TEXT    NOT NULL,
    nonce           TEXT    NOT NULL,
    message         TEXT    NOT NULL,
    created_at      INTEGER NOT NULL,
    expires_at      INTEGER NOT NULL,
    used_at         INTEGER
);

CREATE INDEX IF NOT EXISTS idx_wallet_bind_challenges_session
    ON wallet_bind_challenges(session_id);
CREATE INDEX IF NOT EXISTS idx_wallet_bind_challenges_nonce
    ON wallet_bind_challenges(nonce);
