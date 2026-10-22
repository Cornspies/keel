-- Issues and pull requests share this table (and one number sequence per
-- repository, so "#12" is unambiguous); pull requests add a row in
-- `pull_requests`.
CREATE TABLE issues (
    id            INTEGER PRIMARY KEY,
    repo_id       INTEGER NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    number        INTEGER NOT NULL,
    author_id     INTEGER REFERENCES users (id) ON DELETE SET NULL,
    title         TEXT    NOT NULL,
    body          TEXT    NOT NULL DEFAULT '',
    is_pull       INTEGER NOT NULL DEFAULT 0,
    -- 'merged' only for pull requests.
    state         TEXT    NOT NULL DEFAULT 'open' CHECK (state IN ('open', 'closed', 'merged')),
    comment_count INTEGER NOT NULL DEFAULT 0,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    closed_at     INTEGER,
    UNIQUE (repo_id, number)
);
CREATE INDEX issues_by_state ON issues (repo_id, is_pull, state, number DESC);

-- The timeline of an issue: comments and events (closed, reopened, merged).
CREATE TABLE comments (
    id         INTEGER PRIMARY KEY,
    issue_id   INTEGER NOT NULL REFERENCES issues (id) ON DELETE CASCADE,
    author_id  INTEGER REFERENCES users (id) ON DELETE SET NULL,
    kind       TEXT    NOT NULL DEFAULT 'comment'
                       CHECK (kind IN ('comment', 'closed', 'reopened', 'merged')),
    body       TEXT    NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX comments_by_issue ON comments (issue_id, id);
