-- Personal access tokens: an alternative to passwords for git over HTTP.
CREATE TABLE access_tokens (
    id           INTEGER PRIMARY KEY,
    user_id      INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    name         TEXT    NOT NULL,
    -- SHA-256 of the token; the token itself is shown once and never stored.
    token_hash   BLOB    NOT NULL UNIQUE,
    -- Last characters of the token, to tell tokens apart in the UI.
    hint         TEXT    NOT NULL,
    can_write    INTEGER NOT NULL DEFAULT 0,
    created_at   INTEGER NOT NULL,
    last_used_at INTEGER,
    expires_at   INTEGER
);

CREATE INDEX access_tokens_user ON access_tokens (user_id);
