-- Commit statuses, repository redirects after renames and transfers, and
-- two-factor authentication.

-- Results that CI and other services report for commits: the latest state
-- per context (e.g. `ci/build`).
CREATE TABLE commit_statuses (
    id          INTEGER PRIMARY KEY,
    repo_id     INTEGER NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    sha         TEXT    NOT NULL,
    context     TEXT    NOT NULL,
    state       TEXT    NOT NULL CHECK (state IN ('pending', 'success', 'failure', 'error')),
    description TEXT    NOT NULL DEFAULT '',
    target_url  TEXT    NOT NULL DEFAULT '',
    creator_id  INTEGER REFERENCES users (id) ON DELETE SET NULL,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    UNIQUE (repo_id, sha, context)
);
CREATE INDEX commit_statuses_recent ON commit_statuses (repo_id, updated_at DESC);

-- Contexts that must report success before merging, comma-separated.
ALTER TABLE branch_protections ADD COLUMN required_checks TEXT NOT NULL DEFAULT '';

-- Old names of renamed or transferred repositories, so links, clones and
-- remotes keep working. Taken over when a new repository gets that name.
CREATE TABLE repo_redirects (
    owner_id   INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    name       TEXT    NOT NULL COLLATE NOCASE,
    repo_id    INTEGER NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (owner_id, name)
) WITHOUT ROWID;
CREATE INDEX repo_redirects_repo ON repo_redirects (repo_id);

-- TOTP (RFC 6238): the base32 secret once confirmed, a secret being set
-- up, and the last time step used, so a code can't be used twice.
ALTER TABLE users ADD COLUMN totp_secret TEXT;
ALTER TABLE users ADD COLUMN totp_pending TEXT;
ALTER TABLE users ADD COLUMN totp_last_step INTEGER NOT NULL DEFAULT 0;

-- Single-use codes for when the authenticator is lost, stored hashed.
CREATE TABLE recovery_codes (
    user_id   INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    code_hash BLOB    NOT NULL,
    used_at   INTEGER,
    PRIMARY KEY (user_id, code_hash)
) WITHOUT ROWID;

-- Sign-ins that passed the password and wait for the second factor.
CREATE TABLE login_challenges (
    token_hash BLOB    PRIMARY KEY,
    user_id    INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    attempts   INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX login_challenges_user ON login_challenges (user_id, created_at);
