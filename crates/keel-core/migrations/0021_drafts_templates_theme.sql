-- no-transaction
-- Draft pull requests and auto-merge, repository templates, and a theme
-- setting. New timeline event kinds need the comments table rebuilt (see
-- 0011), with foreign key enforcement off meanwhile.
PRAGMA foreign_keys = OFF;
BEGIN;

-- Drafts can't be merged until they're marked ready for review.
ALTER TABLE pull_requests ADD COLUMN is_draft INTEGER NOT NULL DEFAULT 0;
-- Auto-merge: who turned it on (it merges as them once nothing blocks it)
-- and how; NULL when it's off.
ALTER TABLE pull_requests ADD COLUMN auto_merge_by INTEGER REFERENCES users (id) ON DELETE SET NULL;
ALTER TABLE pull_requests ADD COLUMN auto_merge_style TEXT NOT NULL DEFAULT 'merge'
    CHECK (auto_merge_style IN ('merge', 'squash'));
ALTER TABLE pull_requests ADD COLUMN auto_merge_delete INTEGER NOT NULL DEFAULT 0;
CREATE INDEX pull_requests_auto_merge ON pull_requests (auto_merge_by) WHERE auto_merge_by IS NOT NULL;

-- Template repositories can be used to start new ones; `template_id` is
-- the template a repository was generated from.
ALTER TABLE repositories ADD COLUMN is_template INTEGER NOT NULL DEFAULT 0;
ALTER TABLE repositories ADD COLUMN template_id INTEGER REFERENCES repositories (id) ON DELETE SET NULL;

-- `auto` follows the system.
ALTER TABLE users ADD COLUMN theme TEXT NOT NULL DEFAULT 'auto' CHECK (theme IN ('auto', 'light', 'dark'));

CREATE TABLE comments_new (
    id              INTEGER PRIMARY KEY,
    issue_id        INTEGER NOT NULL REFERENCES issues (id) ON DELETE CASCADE,
    author_id       INTEGER REFERENCES users (id) ON DELETE SET NULL,
    -- See 0011; new here: ready_for_review, converted_to_draft,
    -- auto_merge_enabled and auto_merge_disabled (no body).
    kind            TEXT    NOT NULL DEFAULT 'comment' CHECK (kind IN (
                        'comment', 'inline', 'approved', 'changes_requested',
                        'closed', 'reopened', 'merged',
                        'labeled', 'unlabeled', 'assigned', 'unassigned',
                        'review_requested', 'review_request_removed',
                        'milestoned', 'demilestoned',
                        'ready_for_review', 'converted_to_draft',
                        'auto_merge_enabled', 'auto_merge_disabled')),
    body            TEXT    NOT NULL DEFAULT '',
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    path            TEXT,
    side            TEXT    CHECK (side IN ('old', 'new')),
    line            INTEGER,
    commit_id       TEXT,
    blob_id         TEXT,
    hunk            TEXT,
    reply_to        INTEGER REFERENCES comments (id) ON DELETE CASCADE,
    original_author TEXT
);
INSERT INTO comments_new SELECT id, issue_id, author_id, kind, body, created_at, updated_at,
    path, side, line, commit_id, blob_id, hunk, reply_to, original_author FROM comments;
DROP TABLE comments;
ALTER TABLE comments_new RENAME TO comments;
CREATE INDEX comments_by_issue ON comments (issue_id, id);
CREATE INDEX comments_replies ON comments (reply_to) WHERE reply_to IS NOT NULL;

COMMIT;
PRAGMA foreign_keys = ON;
