-- Repository topics, for finding repositories on the Explore page.

CREATE TABLE repo_topics (
    repo_id INTEGER NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    topic   TEXT NOT NULL,
    PRIMARY KEY (repo_id, topic)
) WITHOUT ROWID;

CREATE INDEX repo_topics_by_topic ON repo_topics (topic, repo_id);
