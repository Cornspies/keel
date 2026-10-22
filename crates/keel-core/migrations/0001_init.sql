CREATE TABLE users (
    id            INTEGER PRIMARY KEY,
    username      TEXT    NOT NULL UNIQUE COLLATE NOCASE,
    email         TEXT    NOT NULL UNIQUE COLLATE NOCASE,
    password_hash TEXT    NOT NULL,
    is_admin      INTEGER NOT NULL DEFAULT 0,
    created_at    INTEGER NOT NULL
);

CREATE TABLE sessions (
    -- SHA-256 of the cookie value; the token itself is never stored.
    token_hash BLOB    PRIMARY KEY,
    user_id    INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
) WITHOUT ROWID;

CREATE INDEX sessions_user ON sessions (user_id);

CREATE TABLE ssh_keys (
    id           INTEGER PRIMARY KEY,
    user_id      INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    name         TEXT    NOT NULL,
    -- "SHA256:<base64>" as printed by `ssh-keygen -l`.
    fingerprint  TEXT    NOT NULL UNIQUE,
    public_key   TEXT    NOT NULL,
    created_at   INTEGER NOT NULL,
    last_used_at INTEGER
);

CREATE INDEX ssh_keys_user ON ssh_keys (user_id);

CREATE TABLE repositories (
    id          INTEGER PRIMARY KEY,
    owner_id    INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    name        TEXT    NOT NULL COLLATE NOCASE,
    description TEXT    NOT NULL DEFAULT '',
    is_private  INTEGER NOT NULL DEFAULT 0,
    created_at  INTEGER NOT NULL,
    -- Bumped on every push.
    updated_at  INTEGER NOT NULL,
    UNIQUE (owner_id, name)
);

CREATE INDEX repositories_updated ON repositories (is_private, updated_at DESC);
