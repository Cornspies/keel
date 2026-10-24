-- Releases (notes and files for a tag) and the activity feed.

CREATE TABLE releases (
    id           INTEGER PRIMARY KEY,
    repo_id      INTEGER NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    tag          TEXT    NOT NULL,
    -- For a draft whose tag doesn't exist yet: the branch or commit to
    -- create it at when it's published.
    target       TEXT    NOT NULL DEFAULT '',
    title        TEXT    NOT NULL DEFAULT '',
    notes        TEXT    NOT NULL DEFAULT '',
    draft        INTEGER NOT NULL DEFAULT 0,
    prerelease   INTEGER NOT NULL DEFAULT 0,
    author_id    INTEGER REFERENCES users (id) ON DELETE SET NULL,
    created_at   INTEGER NOT NULL,
    published_at INTEGER,
    UNIQUE (repo_id, tag)
);

-- Files attached to releases, stored as data/releases/<id>.
CREATE TABLE release_assets (
    id           INTEGER PRIMARY KEY,
    release_id   INTEGER NOT NULL REFERENCES releases (id) ON DELETE CASCADE,
    name         TEXT    NOT NULL,
    size         INTEGER NOT NULL,
    content_type TEXT    NOT NULL,
    downloads    INTEGER NOT NULL DEFAULT 0,
    uploader_id  INTEGER REFERENCES users (id) ON DELETE SET NULL,
    created_at   INTEGER NOT NULL,
    UNIQUE (release_id, name)
);

-- What happened, for activity feeds: pushes, issues and pull requests,
-- comments and reviews, releases, new repositories.
CREATE TABLE events (
    id         INTEGER PRIMARY KEY,
    repo_id    INTEGER NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    actor_id   INTEGER REFERENCES users (id) ON DELETE SET NULL,
    kind       TEXT    NOT NULL,
    ref_name   TEXT,
    issue_id   INTEGER REFERENCES issues (id) ON DELETE CASCADE,
    release_id INTEGER REFERENCES releases (id) ON DELETE CASCADE,
    -- Details, as JSON (e.g. the commits of a push).
    data       TEXT    NOT NULL DEFAULT '{}',
    created_at INTEGER NOT NULL
);
CREATE INDEX events_repo ON events (repo_id, id DESC);
CREATE INDEX events_actor ON events (actor_id, id DESC);
