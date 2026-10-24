-- Repositories pinned to a profile (a user's or an organization's), and
-- avatars.

CREATE TABLE pinned_repos (
    owner_id INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    repo_id  INTEGER NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    PRIMARY KEY (owner_id, repo_id)
) WITHOUT ROWID;

-- The uploaded avatar's content type (the image is data/avatars/<id>);
-- NULL: a generated one. The version changes with every upload, for
-- caching.
ALTER TABLE users ADD COLUMN avatar_type TEXT;
ALTER TABLE users ADD COLUMN avatar_version INTEGER NOT NULL DEFAULT 0;
