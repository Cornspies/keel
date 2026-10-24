-- Site administration: disabled accounts and instance settings.

-- Disabled accounts can't sign in, push, pull or use the API, and get no
-- notifications. Their work stays.
ALTER TABLE users ADD COLUMN disabled_at INTEGER;

-- Settings changed on the admin page (see `SiteSettings`).
CREATE TABLE site_settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) WITHOUT ROWID;
