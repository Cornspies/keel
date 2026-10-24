-- Push mirrors and Git LFS file locks.

-- Remote repositories that get every change pushed to them.
CREATE TABLE push_mirrors (
    id              INTEGER PRIMARY KEY,
    repo_id         INTEGER NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    -- Without credentials, which are kept apart so they're never shown.
    url             TEXT    NOT NULL,
    username        TEXT    NOT NULL DEFAULT '',
    password        TEXT    NOT NULL DEFAULT '',
    -- Changes are waiting to be pushed.
    pending         INTEGER NOT NULL DEFAULT 1,
    next_attempt_at INTEGER NOT NULL,
    -- Failed attempts since the last success, for backing off.
    attempts        INTEGER NOT NULL DEFAULT 0,
    pushed_at       INTEGER,
    last_error      TEXT,
    created_at      INTEGER NOT NULL
);
CREATE INDEX push_mirrors_repo ON push_mirrors (repo_id);
CREATE INDEX push_mirrors_due ON push_mirrors (next_attempt_at) WHERE pending = 1;

-- Files locked with `git lfs lock`, so others don't edit them meanwhile.
CREATE TABLE lfs_locks (
    id         INTEGER PRIMARY KEY,
    repo_id    INTEGER NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    path       TEXT    NOT NULL,
    ref_name   TEXT    NOT NULL DEFAULT '',
    owner_id   INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    UNIQUE (repo_id, path)
);

-- LFS files a pull mirror still has to download (a JSON array of
-- [oid, size]), retried on the next sync.
ALTER TABLE mirrors ADD COLUMN lfs_pending TEXT NOT NULL DEFAULT '[]';
