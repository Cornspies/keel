-- Importing repositories from GitHub or Gitea, with their issues, pull
-- requests, releases and wiki.

CREATE TABLE imports (
    repo_id       INTEGER PRIMARY KEY REFERENCES repositories (id) ON DELETE CASCADE,
    service       TEXT    NOT NULL CHECK (service IN ('github', 'gitea')),
    -- The source repository's page, e.g. https://github.com/ada/engine.
    url           TEXT    NOT NULL,
    -- Its API, e.g. https://api.github.com/repos/ada/engine.
    api_url       TEXT    NOT NULL,
    -- An access token for private repositories and rate limits; cleared
    -- when the import ends.
    token         TEXT    NOT NULL DEFAULT '',
    with_issues   INTEGER NOT NULL DEFAULT 1,
    with_releases INTEGER NOT NULL DEFAULT 1,
    with_wiki     INTEGER NOT NULL DEFAULT 1,
    status        TEXT    NOT NULL DEFAULT 'queued' CHECK (status IN ('queued', 'running', 'done', 'failed')),
    -- What it's doing, e.g. "Issues: 120 of 340".
    progress      TEXT    NOT NULL DEFAULT '',
    error         TEXT,
    created_by    INTEGER REFERENCES users (id) ON DELETE SET NULL,
    created_at    INTEGER NOT NULL,
    finished_at   INTEGER
);

-- Who wrote imported issues, comments and releases: their profile on the
-- source (they have no account here).
ALTER TABLE issues ADD COLUMN original_author TEXT;
ALTER TABLE comments ADD COLUMN original_author TEXT;
ALTER TABLE releases ADD COLUMN original_author TEXT;
