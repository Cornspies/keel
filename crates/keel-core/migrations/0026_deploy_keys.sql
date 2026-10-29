-- SSH keys for one repository, for servers and CI rather than people:
-- read-only, or allowed to push. A key is either a person's or a
-- repository's deploy key, never both (checked when adding).
CREATE TABLE deploy_keys (
    id           INTEGER PRIMARY KEY,
    repo_id      INTEGER NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    title        TEXT    NOT NULL,
    fingerprint  TEXT    NOT NULL UNIQUE,
    public_key   TEXT    NOT NULL,
    can_write    INTEGER NOT NULL DEFAULT 0,
    creator_id   INTEGER REFERENCES users (id) ON DELETE SET NULL,
    created_at   INTEGER NOT NULL,
    last_used_at INTEGER
);
CREATE INDEX deploy_keys_repo ON deploy_keys (repo_id);
