-- Stars on repositories, and reactions (👍 ❤️ …) on issues, pull requests
-- and comments.

CREATE TABLE stars (
    user_id    INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    repo_id    INTEGER NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (user_id, repo_id)
) WITHOUT ROWID;
CREATE INDEX stars_by_repo ON stars (repo_id, created_at);

-- Kept in step with `stars` (also when deleting an account removes its
-- stars), so lists can show and sort by it cheaply.
ALTER TABLE repositories ADD COLUMN star_count INTEGER NOT NULL DEFAULT 0;
CREATE TRIGGER stars_added AFTER INSERT ON stars BEGIN
    UPDATE repositories SET star_count = star_count + 1 WHERE id = NEW.repo_id;
END;
CREATE TRIGGER stars_removed AFTER DELETE ON stars BEGIN
    UPDATE repositories SET star_count = star_count - 1 WHERE id = OLD.repo_id;
END;

CREATE TABLE reactions (
    id         INTEGER PRIMARY KEY,
    issue_id   INTEGER NOT NULL REFERENCES issues (id) ON DELETE CASCADE,
    -- NULL: a reaction to the issue or pull request itself.
    comment_id INTEGER REFERENCES comments (id) ON DELETE CASCADE,
    user_id    INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    -- GitHub's names.
    content    TEXT    NOT NULL CHECK (content IN (
                   '+1', '-1', 'laugh', 'hooray', 'confused', 'heart', 'rocket', 'eyes')),
    created_at INTEGER NOT NULL
);
CREATE UNIQUE INDEX reactions_unique ON reactions (issue_id, IFNULL(comment_id, 0), user_id, content);
CREATE INDEX reactions_by_comment ON reactions (comment_id) WHERE comment_id IS NOT NULL;
