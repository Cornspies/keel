-- Pull mirrors and Git LFS.

-- Repositories kept in sync with one elsewhere. They can't be pushed to.
CREATE TABLE mirrors (
    repo_id       INTEGER PRIMARY KEY REFERENCES repositories (id) ON DELETE CASCADE,
    -- Without credentials, which are kept apart so they're never shown.
    url           TEXT    NOT NULL,
    username      TEXT    NOT NULL DEFAULT '',
    password      TEXT    NOT NULL DEFAULT '',
    interval_secs INTEGER NOT NULL DEFAULT 28800,
    synced_at     INTEGER,
    next_sync_at  INTEGER NOT NULL,
    last_error    TEXT,
    created_at    INTEGER NOT NULL
);
CREATE INDEX mirrors_due ON mirrors (next_sync_at);

-- Large files are stored once (by SHA-256, under data/lfs) and listed per
-- repository: a repository can only download what was uploaded to it (or
-- to the repository it was forked from).
CREATE TABLE lfs_objects (
    repo_id    INTEGER NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    oid        TEXT    NOT NULL,
    size       INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (repo_id, oid)
) WITHOUT ROWID;
CREATE INDEX lfs_objects_oid ON lfs_objects (oid);
