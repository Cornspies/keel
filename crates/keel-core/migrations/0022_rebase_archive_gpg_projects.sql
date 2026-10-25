-- no-transaction
-- Rebase merging (auto-merge can rebase: the pull_requests table is
-- rebuilt for its CHECK, with foreign key enforcement off meanwhile, see
-- 0011), archived repositories, GPG keys for verifying commit signatures,
-- and project boards.
PRAGMA foreign_keys = OFF;
BEGIN;

CREATE TABLE pull_requests_new (
    issue_id          INTEGER PRIMARY KEY REFERENCES issues (id) ON DELETE CASCADE,
    base_ref          TEXT    NOT NULL,
    head_ref          TEXT    NOT NULL,
    -- Set when merged, so "Files changed" stays what was merged.
    base_commit       TEXT,
    head_commit       TEXT,
    merge_commit      TEXT,
    merged_by         INTEGER REFERENCES users (id) ON DELETE SET NULL,
    merged_at         INTEGER,
    head_repo_id      INTEGER REFERENCES repositories (id) ON DELETE SET NULL,
    is_draft          INTEGER NOT NULL DEFAULT 0,
    -- See 0021.
    auto_merge_by     INTEGER REFERENCES users (id) ON DELETE SET NULL,
    auto_merge_style  TEXT    NOT NULL DEFAULT 'merge'
                          CHECK (auto_merge_style IN ('merge', 'squash', 'rebase')),
    auto_merge_delete INTEGER NOT NULL DEFAULT 0
);
INSERT INTO pull_requests_new SELECT issue_id, base_ref, head_ref, base_commit, head_commit,
    merge_commit, merged_by, merged_at, head_repo_id, is_draft, auto_merge_by, auto_merge_style,
    auto_merge_delete FROM pull_requests;
DROP TABLE pull_requests;
ALTER TABLE pull_requests_new RENAME TO pull_requests;
CREATE INDEX pull_requests_head_repo ON pull_requests (head_repo_id);
CREATE INDEX pull_requests_auto_merge ON pull_requests (auto_merge_by) WHERE auto_merge_by IS NOT NULL;

-- Archived repositories are read-only.
ALTER TABLE repositories ADD COLUMN archived_at INTEGER;

CREATE TABLE gpg_keys (
    id          INTEGER PRIMARY KEY,
    user_id     INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    -- Of the primary key, upper-case hex.
    fingerprint TEXT    NOT NULL UNIQUE,
    -- The key's user ID emails, one per line.
    emails      TEXT    NOT NULL,
    -- As uploaded (ASCII-armored).
    public_key  TEXT    NOT NULL,
    expires_at  INTEGER,
    created_at  INTEGER NOT NULL
);
CREATE INDEX gpg_keys_user ON gpg_keys (user_id);

-- Long key IDs (upper-case hex) of the primary key and its subkeys, which
-- signatures name as their issuer.
CREATE TABLE gpg_key_ids (
    key_id     TEXT    PRIMARY KEY,
    gpg_key_id INTEGER NOT NULL REFERENCES gpg_keys (id) ON DELETE CASCADE
) WITHOUT ROWID;
CREATE INDEX gpg_key_ids_key ON gpg_key_ids (gpg_key_id);

CREATE TABLE projects (
    id          INTEGER PRIMARY KEY,
    repo_id     INTEGER NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    -- Per repository: /owner/name/projects/1.
    number      INTEGER NOT NULL,
    title       TEXT    NOT NULL,
    description TEXT    NOT NULL DEFAULT '',
    closed_at   INTEGER,
    creator_id  INTEGER REFERENCES users (id) ON DELETE SET NULL,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    UNIQUE (repo_id, number)
);

CREATE TABLE project_columns (
    id         INTEGER PRIMARY KEY,
    project_id INTEGER NOT NULL REFERENCES projects (id) ON DELETE CASCADE,
    name       TEXT    NOT NULL,
    position   INTEGER NOT NULL,
    -- Closing an issue moves its card here; reopening it, to the first
    -- column.
    is_done    INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX project_columns_by_project ON project_columns (project_id, position);

-- A card is an issue (or pull request) or a note.
CREATE TABLE project_cards (
    id         INTEGER PRIMARY KEY,
    project_id INTEGER NOT NULL REFERENCES projects (id) ON DELETE CASCADE,
    column_id  INTEGER NOT NULL REFERENCES project_columns (id) ON DELETE CASCADE,
    position   INTEGER NOT NULL,
    issue_id   INTEGER REFERENCES issues (id) ON DELETE CASCADE,
    note       TEXT,
    creator_id INTEGER REFERENCES users (id) ON DELETE SET NULL,
    created_at INTEGER NOT NULL,
    CHECK ((issue_id IS NULL) != (note IS NULL)),
    UNIQUE (project_id, issue_id)
);
CREATE INDEX project_cards_by_column ON project_cards (column_id, position);
CREATE INDEX project_cards_by_issue ON project_cards (issue_id) WHERE issue_id IS NOT NULL;

COMMIT;
PRAGMA foreign_keys = ON;
