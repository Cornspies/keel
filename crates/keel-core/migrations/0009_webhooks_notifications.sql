-- Webhooks: HTTP POSTs to other services when something happens in a
-- repository.
CREATE TABLE webhooks (
    id          INTEGER PRIMARY KEY,
    repo_id     INTEGER NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    url         TEXT    NOT NULL,
    -- Signs each delivery (HMAC-SHA256); empty for unsigned deliveries.
    secret      TEXT    NOT NULL DEFAULT '',
    -- Comma-separated event names, or '*' for all.
    events      TEXT    NOT NULL DEFAULT 'push',
    active      INTEGER NOT NULL DEFAULT 1,
    created_at  INTEGER NOT NULL
);
CREATE INDEX webhooks_repo ON webhooks (repo_id);

-- The delivery queue and log. The payload is built when the event happens,
-- so retries and redeliveries send exactly the same thing.
CREATE TABLE webhook_deliveries (
    id              INTEGER PRIMARY KEY,
    hook_id         INTEGER NOT NULL REFERENCES webhooks (id) ON DELETE CASCADE,
    guid            TEXT    NOT NULL,
    event           TEXT    NOT NULL,
    action          TEXT    NOT NULL DEFAULT '',
    payload         TEXT    NOT NULL,
    -- pending (queued or waiting to retry), ok or failed (gave up).
    status          TEXT    NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'ok', 'failed')),
    attempts        INTEGER NOT NULL DEFAULT 0,
    next_attempt_at INTEGER NOT NULL,
    response_status INTEGER,
    response_body   TEXT,
    error           TEXT,
    duration_ms     INTEGER,
    created_at      INTEGER NOT NULL,
    delivered_at    INTEGER
);
CREATE INDEX webhook_deliveries_hook ON webhook_deliveries (hook_id, id DESC);
CREATE INDEX webhook_deliveries_due ON webhook_deliveries (next_attempt_at) WHERE status = 'pending';

-- People watching a repository get notified about all its issues and pull
-- requests.
CREATE TABLE watches (
    user_id    INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    repo_id    INTEGER NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (user_id, repo_id)
) WITHOUT ROWID;
CREATE INDEX watches_repo ON watches (repo_id);

-- An explicit choice to follow (1) or ignore (0) one issue or pull request,
-- overriding watching and participating.
CREATE TABLE issue_subscriptions (
    user_id    INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    issue_id   INTEGER NOT NULL REFERENCES issues (id) ON DELETE CASCADE,
    subscribed INTEGER NOT NULL,
    PRIMARY KEY (user_id, issue_id)
) WITHOUT ROWID;

-- One notification per person and issue, bumped on new activity.
CREATE TABLE notifications (
    id         INTEGER PRIMARY KEY,
    user_id    INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    issue_id   INTEGER NOT NULL REFERENCES issues (id) ON DELETE CASCADE,
    -- Why: assigned, mentioned, author, participating, subscribed or
    -- watching.
    reason     TEXT    NOT NULL,
    -- What happened last, e.g. "commented" or "merged".
    activity   TEXT    NOT NULL,
    actor_id   INTEGER REFERENCES users (id) ON DELETE SET NULL,
    unread     INTEGER NOT NULL DEFAULT 1,
    updated_at INTEGER NOT NULL,
    UNIQUE (user_id, issue_id)
);
CREATE INDEX notifications_inbox ON notifications (user_id, unread, updated_at DESC);

-- Owners watch their repositories; for organizations, their owners do.
INSERT INTO watches (user_id, repo_id, created_at)
    SELECT r.owner_id, r.id, r.created_at FROM repositories r JOIN users u ON u.id = r.owner_id
    WHERE u.kind = 'user';
INSERT OR IGNORE INTO watches (user_id, repo_id, created_at)
    SELECT m.user_id, r.id, r.created_at FROM repositories r JOIN org_members m ON m.org_id = r.owner_id
    WHERE m.role = 'owner';
