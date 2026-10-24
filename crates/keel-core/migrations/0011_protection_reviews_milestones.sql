-- no-transaction
-- Branch protection, review requests and milestones. New timeline event
-- kinds need the comments table rebuilt (its kinds are a CHECK
-- constraint), with foreign key enforcement off meanwhile (see 0004).
PRAGMA foreign_keys = OFF;
BEGIN;

-- Rules for branches whose name matches `pattern` (a name, or a glob where
-- `*` matches within one path segment, e.g. `release/*`).
CREATE TABLE branch_protections (
    id                 INTEGER PRIMARY KEY,
    repo_id            INTEGER NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    pattern            TEXT    NOT NULL,
    -- Pushes must go through pull requests (creating the branch is fine).
    require_pull       INTEGER NOT NULL DEFAULT 1,
    -- Approvals from people with write access needed to merge into it.
    required_approvals INTEGER NOT NULL DEFAULT 1 CHECK (required_approvals BETWEEN 0 AND 10),
    -- Reviews only count for the commit they were made on.
    dismiss_stale      INTEGER NOT NULL DEFAULT 1,
    -- Force pushes and deleting the branch.
    allow_force_push   INTEGER NOT NULL DEFAULT 0,
    created_at         INTEGER NOT NULL,
    UNIQUE (repo_id, pattern)
);

CREATE TABLE review_requests (
    issue_id     INTEGER NOT NULL REFERENCES issues (id) ON DELETE CASCADE,
    user_id      INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    requested_by INTEGER REFERENCES users (id) ON DELETE SET NULL,
    created_at   INTEGER NOT NULL,
    PRIMARY KEY (issue_id, user_id)
) WITHOUT ROWID;
CREATE INDEX review_requests_user ON review_requests (user_id);

CREATE TABLE milestones (
    id          INTEGER PRIMARY KEY,
    repo_id     INTEGER NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    title       TEXT    NOT NULL COLLATE NOCASE,
    description TEXT    NOT NULL DEFAULT '',
    -- Unix time of the due date (midnight UTC), if any.
    due_on      INTEGER,
    state       TEXT    NOT NULL DEFAULT 'open' CHECK (state IN ('open', 'closed')),
    created_at  INTEGER NOT NULL,
    closed_at   INTEGER,
    UNIQUE (repo_id, title)
);

ALTER TABLE issues ADD COLUMN milestone_id INTEGER REFERENCES milestones (id) ON DELETE SET NULL;
CREATE INDEX issues_milestone ON issues (milestone_id) WHERE milestone_id IS NOT NULL;

CREATE TABLE comments_new (
    id         INTEGER PRIMARY KEY,
    issue_id   INTEGER NOT NULL REFERENCES issues (id) ON DELETE CASCADE,
    author_id  INTEGER REFERENCES users (id) ON DELETE SET NULL,
    -- comment: a plain comment. inline: a comment on a line of a pull
    -- request's diff. approved, changes_requested: a review verdict (with
    -- an optional body; `commit_id` is the head it was given on). The rest
    -- are events; for labeled/unlabeled, assigned/unassigned,
    -- review_requested/review_request_removed and milestoned/demilestoned
    -- the body is the label, user or milestone name.
    kind       TEXT    NOT NULL DEFAULT 'comment' CHECK (kind IN (
                   'comment', 'inline', 'approved', 'changes_requested',
                   'closed', 'reopened', 'merged',
                   'labeled', 'unlabeled', 'assigned', 'unassigned',
                   'review_requested', 'review_request_removed',
                   'milestoned', 'demilestoned')),
    body       TEXT    NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    -- Inline comments: where they were made (see 0008).
    path       TEXT,
    side       TEXT    CHECK (side IN ('old', 'new')),
    line       INTEGER,
    commit_id  TEXT,
    blob_id    TEXT,
    hunk       TEXT,
    reply_to   INTEGER REFERENCES comments (id) ON DELETE CASCADE
);
INSERT INTO comments_new SELECT id, issue_id, author_id, kind, body, created_at, updated_at,
    path, side, line, commit_id, blob_id, hunk, reply_to FROM comments;
DROP TABLE comments;
ALTER TABLE comments_new RENAME TO comments;
CREATE INDEX comments_by_issue ON comments (issue_id, id);
CREATE INDEX comments_replies ON comments (reply_to) WHERE reply_to IS NOT NULL;

COMMIT;
PRAGMA foreign_keys = ON;
