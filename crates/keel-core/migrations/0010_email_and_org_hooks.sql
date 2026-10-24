-- no-transaction
-- Email (verification, password resets, notification emails) and webhooks
-- for whole organizations. Webhooks get a nullable repo_id, which needs a
-- table rebuild with foreign key enforcement off (see 0004).
PRAGMA foreign_keys = OFF;
BEGIN;

-- Set once the user proved they receive mail at `email`; notification
-- emails only go to verified addresses.
ALTER TABLE users ADD COLUMN email_verified_at INTEGER;
-- Which notifications are also emailed: all, participating (everything but
-- activity in watched repositories) or off.
ALTER TABLE users ADD COLUMN email_notifications TEXT NOT NULL DEFAULT 'participating'
    CHECK (email_notifications IN ('all', 'participating', 'off'));

-- Single-use links sent by email. Only the SHA-256 of the token is stored.
CREATE TABLE email_tokens (
    token_hash BLOB    PRIMARY KEY,
    user_id    INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    purpose    TEXT    NOT NULL CHECK (purpose IN ('verify', 'reset')),
    -- The address the link was sent to; a verification only counts if it
    -- is still the account's address.
    email      TEXT    NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX email_tokens_user ON email_tokens (user_id, purpose, created_at);

-- Mail waiting to be sent (and recently sent, for a while).
CREATE TABLE mail_outbox (
    id              INTEGER PRIMARY KEY,
    to_addr         TEXT    NOT NULL,
    subject         TEXT    NOT NULL,
    body            TEXT    NOT NULL,
    -- Message-ID of the conversation this belongs to (an issue), so mail
    -- clients thread the messages; NULL for one-off mail.
    thread          TEXT,
    status          TEXT    NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'sent', 'failed')),
    attempts        INTEGER NOT NULL DEFAULT 0,
    next_attempt_at INTEGER NOT NULL,
    error           TEXT,
    created_at      INTEGER NOT NULL,
    sent_at         INTEGER
);
CREATE INDEX mail_outbox_due ON mail_outbox (next_attempt_at) WHERE status = 'pending';

CREATE TABLE webhooks_new (
    id          INTEGER PRIMARY KEY,
    -- Exactly one of these: a repository's hook, or an organization's,
    -- which gets the events of all its repositories.
    repo_id     INTEGER REFERENCES repositories (id) ON DELETE CASCADE,
    org_id      INTEGER REFERENCES users (id) ON DELETE CASCADE,
    url         TEXT    NOT NULL,
    secret      TEXT    NOT NULL DEFAULT '',
    events      TEXT    NOT NULL DEFAULT 'push',
    active      INTEGER NOT NULL DEFAULT 1,
    created_at  INTEGER NOT NULL,
    CHECK ((repo_id IS NULL) != (org_id IS NULL))
);
INSERT INTO webhooks_new (id, repo_id, url, secret, events, active, created_at)
    SELECT id, repo_id, url, secret, events, active, created_at FROM webhooks;
DROP TABLE webhooks;
ALTER TABLE webhooks_new RENAME TO webhooks;
CREATE INDEX webhooks_repo ON webhooks (repo_id);
CREATE INDEX webhooks_org ON webhooks (org_id);

COMMIT;
PRAGMA foreign_keys = ON;
