-- no-transaction
-- Organizations live in `users` (kind = 'org') so users and organizations
-- share one namespace and repositories keep a single owner column.
-- Organizations have no email, so the column becomes nullable; SQLite needs
-- a table rebuild for that, with foreign key enforcement off meanwhile
-- (https://www.sqlite.org/lang_altertable.html#otheralter).
PRAGMA foreign_keys = OFF;
BEGIN;

CREATE TABLE users_new (
    id            INTEGER PRIMARY KEY,
    username      TEXT    NOT NULL UNIQUE COLLATE NOCASE,
    -- NULL for organizations.
    email         TEXT    UNIQUE COLLATE NOCASE,
    -- Empty for organizations, which can't sign in.
    password_hash TEXT    NOT NULL DEFAULT '',
    is_admin      INTEGER NOT NULL DEFAULT 0,
    kind          TEXT    NOT NULL DEFAULT 'user' CHECK (kind IN ('user', 'org')),
    created_at    INTEGER NOT NULL
);
INSERT INTO users_new (id, username, email, password_hash, is_admin, created_at)
    SELECT id, username, email, password_hash, is_admin, created_at FROM users;
DROP TABLE users;
ALTER TABLE users_new RENAME TO users;

CREATE TABLE org_members (
    org_id     INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    user_id    INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    -- Owners administer the organization and all its repositories; members
    -- can push to and create repositories.
    role       TEXT    NOT NULL CHECK (role IN ('owner', 'member')),
    created_at INTEGER NOT NULL,
    PRIMARY KEY (org_id, user_id)
) WITHOUT ROWID;
CREATE INDEX org_members_user ON org_members (user_id);

CREATE TABLE collaborators (
    repo_id    INTEGER NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    user_id    INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    role       TEXT    NOT NULL CHECK (role IN ('read', 'write', 'admin')),
    created_at INTEGER NOT NULL,
    PRIMARY KEY (repo_id, user_id)
) WITHOUT ROWID;
CREATE INDEX collaborators_user ON collaborators (user_id);

COMMIT;
PRAGMA foreign_keys = ON;
