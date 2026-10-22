-- no-transaction
-- Forks, labels, assignees and code review. Comment kinds are a CHECK
-- constraint, which SQLite can only change by rebuilding the table, with
-- foreign key enforcement off meanwhile (see 0004).
PRAGMA foreign_keys = OFF;
BEGIN;

ALTER TABLE repositories ADD COLUMN forked_from INTEGER REFERENCES repositories (id) ON DELETE SET NULL;
CREATE INDEX repositories_forks ON repositories (forked_from);

-- The repository the head branch lives in: the base repository itself or
-- a fork. NULL once that fork has been deleted.
ALTER TABLE pull_requests ADD COLUMN head_repo_id INTEGER REFERENCES repositories (id) ON DELETE SET NULL;
UPDATE pull_requests SET head_repo_id = (SELECT repo_id FROM issues WHERE issues.id = pull_requests.issue_id);
CREATE INDEX pull_requests_head_repo ON pull_requests (head_repo_id);

CREATE TABLE labels (
    id          INTEGER PRIMARY KEY,
    repo_id     INTEGER NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    name        TEXT    NOT NULL COLLATE NOCASE,
    -- One of a fixed palette (see `LABEL_COLORS`), so labels stay readable
    -- in light and dark mode and need no inline styles.
    color       TEXT    NOT NULL,
    description TEXT    NOT NULL DEFAULT '',
    created_at  INTEGER NOT NULL,
    UNIQUE (repo_id, name)
);

CREATE TABLE issue_labels (
    issue_id INTEGER NOT NULL REFERENCES issues (id) ON DELETE CASCADE,
    label_id INTEGER NOT NULL REFERENCES labels (id) ON DELETE CASCADE,
    PRIMARY KEY (issue_id, label_id)
) WITHOUT ROWID;
CREATE INDEX issue_labels_label ON issue_labels (label_id);

CREATE TABLE issue_assignees (
    issue_id INTEGER NOT NULL REFERENCES issues (id) ON DELETE CASCADE,
    user_id  INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    PRIMARY KEY (issue_id, user_id)
) WITHOUT ROWID;
CREATE INDEX issue_assignees_user ON issue_assignees (user_id);

-- Existing repositories get the labels new ones start with.
INSERT INTO labels (repo_id, name, color, description, created_at)
    SELECT r.id, d.name, d.color, d.description, r.created_at
    FROM repositories r, (
        SELECT 'bug' AS name, 'red' AS color, 'Something isn''t working' AS description
        UNION ALL SELECT 'documentation', 'blue', 'Improvements or additions to documentation'
        UNION ALL SELECT 'enhancement', 'teal', 'New feature or request'
        UNION ALL SELECT 'good first issue', 'purple', 'Good for newcomers'
        UNION ALL SELECT 'help wanted', 'green', 'Extra attention is needed'
        UNION ALL SELECT 'question', 'pink', 'Further information is requested'
    ) d;

CREATE TABLE comments_new (
    id         INTEGER PRIMARY KEY,
    issue_id   INTEGER NOT NULL REFERENCES issues (id) ON DELETE CASCADE,
    author_id  INTEGER REFERENCES users (id) ON DELETE SET NULL,
    -- comment: a plain comment. inline: a comment on a line of a pull
    -- request's diff. approved, changes_requested: a review verdict (with
    -- an optional body). The rest are events; for labeled/unlabeled and
    -- assigned/unassigned the body is the label or user name.
    kind       TEXT    NOT NULL DEFAULT 'comment' CHECK (kind IN (
                   'comment', 'inline', 'approved', 'changes_requested',
                   'closed', 'reopened', 'merged',
                   'labeled', 'unlabeled', 'assigned', 'unassigned')),
    body       TEXT    NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    -- Inline comments: where they were made. `line` is on the `side` (old
    -- or new) version of `path` in the diff against `commit_id`; `blob_id`
    -- is that version's blob, so later diffs can tell whether the line is
    -- still there. `hunk` is the diff context, kept for display.
    path       TEXT,
    side       TEXT    CHECK (side IN ('old', 'new')),
    line       INTEGER,
    commit_id  TEXT,
    blob_id    TEXT,
    hunk       TEXT,
    -- Replies to an inline comment form a thread with it.
    reply_to   INTEGER REFERENCES comments (id) ON DELETE CASCADE
);
INSERT INTO comments_new (id, issue_id, author_id, kind, body, created_at, updated_at)
    SELECT id, issue_id, author_id, kind, body, created_at, updated_at FROM comments;
DROP TABLE comments;
ALTER TABLE comments_new RENAME TO comments;
CREATE INDEX comments_by_issue ON comments (issue_id, id);
CREATE INDEX comments_replies ON comments (reply_to) WHERE reply_to IS NOT NULL;

COMMIT;
PRAGMA foreign_keys = ON;
