-- Per-session policy override persistence (Step 2).
--
-- When a session is created with `policy_overrides`, the rules are serialized
-- to JSON and stored here so they survive daemon restarts. On startup, the
-- daemon loads all rows back into the in-memory SessionPolicyStore.
--
-- Each session has at most one row. The `rules_json` column contains a
-- JSON array of PolicyRule objects (same format as `[[policy.rules]]` in TOML).
--
-- Rows are deleted on session close via `delete_session()`.

CREATE TABLE IF NOT EXISTS session_policy_overrides (
    session_id   TEXT    NOT NULL PRIMARY KEY,
    rules_json   TEXT    NOT NULL,
    created_at   INTEGER NOT NULL
);
