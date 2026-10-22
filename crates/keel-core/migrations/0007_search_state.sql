-- Which commit of each repository's default branch the search index holds.
CREATE TABLE search_index_state (
    repo_id    INTEGER PRIMARY KEY REFERENCES repositories (id) ON DELETE CASCADE,
    commit_id  TEXT    NOT NULL,
    indexed_at INTEGER NOT NULL
);
