-- no-transaction
-- Branch rules can require verified commits, projects can belong to an
-- organization instead of a repository, and board changes show in issue
-- timelines. `projects.repo_id` becomes nullable and comment kinds are a
-- CHECK constraint, which SQLite needs table rebuilds for, with foreign key
-- enforcement off meanwhile (https://www.sqlite.org/lang_altertable.html#otheralter).
PRAGMA foreign_keys = OFF;
BEGIN;

ALTER TABLE branch_protections ADD COLUMN require_signed INTEGER NOT NULL DEFAULT 0;

CREATE TABLE projects_new (
    id          INTEGER PRIMARY KEY,
    -- A repository's board (/owner/name/projects/1), or…
    repo_id     INTEGER REFERENCES repositories (id) ON DELETE CASCADE,
    -- …an organization's, with issues from its repositories
    -- (/org/name/projects/1).
    owner_id    INTEGER REFERENCES users (id) ON DELETE CASCADE,
    number      INTEGER NOT NULL,
    title       TEXT    NOT NULL,
    description TEXT    NOT NULL DEFAULT '',
    closed_at   INTEGER,
    creator_id  INTEGER REFERENCES users (id) ON DELETE SET NULL,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    CHECK ((repo_id IS NULL) != (owner_id IS NULL)),
    UNIQUE (repo_id, number),
    UNIQUE (owner_id, number)
);
INSERT INTO projects_new (id, repo_id, number, title, description, closed_at, creator_id, created_at, updated_at)
    SELECT id, repo_id, number, title, description, closed_at, creator_id, created_at, updated_at FROM projects;
DROP TABLE projects;
ALTER TABLE projects_new RENAME TO projects;

CREATE TABLE comments_new (
    id              INTEGER PRIMARY KEY,
    issue_id        INTEGER NOT NULL REFERENCES issues (id) ON DELETE CASCADE,
    author_id       INTEGER REFERENCES users (id) ON DELETE SET NULL,
    -- See 0021; new here: added_to_project, moved_columns_in_project and
    -- removed_from_project, whose body is JSON (the board and columns).
    kind            TEXT    NOT NULL DEFAULT 'comment' CHECK (kind IN (
                        'comment', 'inline', 'approved', 'changes_requested',
                        'closed', 'reopened', 'merged',
                        'labeled', 'unlabeled', 'assigned', 'unassigned',
                        'review_requested', 'review_request_removed',
                        'milestoned', 'demilestoned',
                        'ready_for_review', 'converted_to_draft',
                        'auto_merge_enabled', 'auto_merge_disabled',
                        'added_to_project', 'moved_columns_in_project', 'removed_from_project')),
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
