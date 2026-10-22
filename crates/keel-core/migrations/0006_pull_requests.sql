-- What pull requests add to issues. The head commit is also kept in the
-- repository as refs/pull/<number>/head, so a pull request stays viewable
-- after its branch is deleted.
CREATE TABLE pull_requests (
    issue_id     INTEGER PRIMARY KEY REFERENCES issues (id) ON DELETE CASCADE,
    base_ref     TEXT    NOT NULL,
    head_ref     TEXT    NOT NULL,
    -- Set when merged, so "Files changed" stays what was merged.
    base_commit  TEXT,
    head_commit  TEXT,
    merge_commit TEXT,
    merged_by    INTEGER REFERENCES users (id) ON DELETE SET NULL,
    merged_at    INTEGER
);
