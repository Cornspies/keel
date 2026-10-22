-- "Last commit per directory entry" results, which are expensive to compute
-- on large histories. Kept per (repository, directory, commit); after a push
-- the newest stored result for an ancestor commit is updated incrementally.
-- Newest rows have the highest rowid (INSERT OR REPLACE re-inserts).
CREATE TABLE last_commit_cache (
    id         INTEGER PRIMARY KEY,
    repo_id    INTEGER NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    path       TEXT    NOT NULL,
    commit_id  TEXT    NOT NULL,
    -- JSON object: entry name -> commit id
    entries    TEXT    NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE (repo_id, path, commit_id)
);
