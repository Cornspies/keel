-- Accounts at an OpenID Connect provider (single sign-on), linked to keel
-- accounts. Accounts created by signing in with one have no password
-- (an empty password_hash) until their owner sets one.
CREATE TABLE external_identities (
    user_id      INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    issuer       TEXT    NOT NULL,
    subject      TEXT    NOT NULL,
    -- As the provider said when linking, to show which account it is.
    email        TEXT    NOT NULL DEFAULT '',
    created_at   INTEGER NOT NULL,
    last_used_at INTEGER,
    PRIMARY KEY (issuer, subject)
) WITHOUT ROWID;
CREATE INDEX external_identities_user ON external_identities (user_id);
