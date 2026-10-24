//! Repository topics, and the Explore page's listing of repositories.

use crate::{Core, Repo, Result, User, invalid};

pub const MAX_TOPICS: usize = 20;
const MAX_TOPIC_LEN: usize = 50;

/// Parses topics as people type them ("Rust, web cli"): lowercased,
/// separated by spaces or commas, without duplicates, sorted.
pub fn parse_topics(input: &str) -> Result<Vec<String>> {
    let mut topics: Vec<String> =
        input.split([' ', ',', '\t', '\n', '\r']).filter(|t| !t.is_empty()).map(|t| t.to_lowercase()).collect();
    topics.sort();
    topics.dedup();
    for t in &topics {
        let ok = t.len() <= MAX_TOPIC_LEN
            && t.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !t.starts_with('-');
        if !ok {
            return invalid(format!(
                "“{t}” isn't a valid topic: use lowercase letters, digits and hyphens (not first), at most {MAX_TOPIC_LEN} characters."
            ));
        }
    }
    if topics.len() > MAX_TOPICS {
        return invalid(format!("A repository can have at most {MAX_TOPICS} topics."));
    }
    Ok(topics)
}

/// How the Explore page orders repositories.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExploreSort {
    /// Most recently pushed first.
    #[default]
    Updated,
    /// Newest first.
    Created,
    /// Most watched first.
    Watchers,
    Name,
}

impl ExploreSort {
    pub const ALL: [ExploreSort; 4] = [Self::Updated, Self::Created, Self::Watchers, Self::Name];

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.as_str() == s)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Updated => "updated",
            Self::Created => "created",
            Self::Watchers => "watchers",
            Self::Name => "name",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Updated => "Recently updated",
            Self::Created => "Newest",
            Self::Watchers => "Most watched",
            Self::Name => "Name",
        }
    }
}

/// What to list on the Explore page.
#[derive(Clone, Debug, Default)]
pub struct ExploreQuery {
    /// Matches `owner/name` and descriptions.
    pub text: String,
    pub topic: Option<String>,
    pub sort: ExploreSort,
}

// Explore's filter. `?1`: text pattern, `?2`, `?3`: the viewer (see
// `visible_to!`), `?4`: topic or ''.
macro_rules! explore_where {
    () => {
        concat!(
            "WHERE (?1 = '%%' OR u.username || '/' || r.name LIKE ?1 ESCAPE '\\' OR r.description LIKE ?1 ESCAPE '\\')
               AND (?4 = '' OR EXISTS (SELECT 1 FROM repo_topics t WHERE t.topic = ?4 AND t.repo_id = r.id))
               AND ",
            visible_to!()
        )
    };
}

macro_rules! explore_page {
    ($order:literal) => {
        concat!(repo_select!(), explore_where!(), " ORDER BY ", $order, " LIMIT ?5 OFFSET ?6")
    };
}

impl Core {
    /// Replaces a repository's topics.
    pub async fn set_topics(&self, repo_id: i64, topics: &[String]) -> Result<()> {
        let mut tx = self.db.begin().await?;
        sqlx::query("DELETE FROM repo_topics WHERE repo_id = ?1").bind(repo_id).execute(&mut *tx).await?;
        for t in topics {
            sqlx::query("INSERT OR IGNORE INTO repo_topics (repo_id, topic) VALUES (?1, ?2)")
                .bind(repo_id)
                .bind(t)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Repositories `viewer` can read matching `q`, and how many match.
    pub async fn explore(&self, viewer: Option<&User>, q: &ExploreQuery, offset: i64, limit: i64) -> Result<(Vec<Repo>, i64)> {
        let escaped = q.text.trim().replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
        let pattern = format!("%{escaped}%");
        let topic = q.topic.as_deref().unwrap_or_default();
        let (viewer_id, admin) = (viewer.map_or(-1, |u| u.id), viewer.is_some_and(|u| u.is_admin));
        let sql = match q.sort {
            ExploreSort::Updated => explore_page!("r.updated_at DESC, r.id DESC"),
            ExploreSort::Created => explore_page!("r.created_at DESC, r.id DESC"),
            ExploreSort::Watchers => {
                explore_page!("(SELECT COUNT(*) FROM watches w WHERE w.repo_id = r.id) DESC, r.updated_at DESC, r.id DESC")
            }
            ExploreSort::Name => explore_page!("lower(r.name), lower(u.username), r.id"),
        };
        let repos = sqlx::query_as(sql)
            .bind(&pattern)
            .bind(viewer_id)
            .bind(admin)
            .bind(topic)
            .bind(limit)
            .bind(offset)
            .fetch_all(&self.db)
            .await?;
        let (total,): (i64,) = sqlx::query_as(concat!(
            "SELECT COUNT(*) FROM repositories r JOIN users u ON u.id = r.owner_id ",
            explore_where!()
        ))
        .bind(&pattern)
        .bind(viewer_id)
        .bind(admin)
        .bind(topic)
        .fetch_one(&self.db)
        .await?;
        Ok((repos, total))
    }

    /// The most used topics among repositories `viewer` can read, with how
    /// many use each.
    pub async fn popular_topics(&self, viewer: Option<&User>, limit: i64) -> Result<Vec<(String, i64)>> {
        Ok(sqlx::query_as(concat!(
            "SELECT t.topic, COUNT(*) AS n FROM repo_topics t JOIN repositories r ON r.id = t.repo_id
             WHERE ?1 = ?1 AND ",
            visible_to!(),
            " GROUP BY t.topic ORDER BY n DESC, t.topic LIMIT ?4"
        ))
        .bind(0)
        .bind(viewer.map_or(-1, |u| u.id))
        .bind(viewer.is_some_and(|u| u.is_admin))
        .bind(limit)
        .fetch_all(&self.db)
        .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsing_topics() {
        assert_eq!(parse_topics(" Rust, web  cli,rust ").unwrap(), ["cli", "rust", "web"]);
        assert!(parse_topics("").unwrap().is_empty());
        for bad in ["c++", "-x", "dots.no", "ünï", &"a".repeat(51)] {
            assert!(parse_topics(bad).is_err(), "{bad}");
        }
        let many: Vec<String> = (0..21).map(|i| format!("t{i}")).collect();
        assert!(parse_topics(&many.join(" ")).is_err());
    }

    #[tokio::test]
    async fn exploring() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let bob = core.register("bob", "bob@example.com", "password1").await.unwrap();
        let engine = core.create_repo(&ada, "engine", "Analytical engine", false).await.unwrap();
        let notes = core.create_repo(&ada, "notes", "100% private", true).await.unwrap();
        let web = core.create_repo(&bob, "web_site", "", false).await.unwrap();
        core.set_topics(engine.id, &parse_topics("rust math").unwrap()).await.unwrap();
        core.set_topics(notes.id, &parse_topics("rust").unwrap()).await.unwrap();
        core.set_topics(web.id, &parse_topics("web rust").unwrap()).await.unwrap();
        assert_eq!(core.repo_by_id(engine.id).await.unwrap().unwrap().topics().collect::<Vec<_>>(), ["math", "rust"]);

        let names = |(repos, total): (Vec<Repo>, i64)| (repos.into_iter().map(|r| r.name).collect::<Vec<_>>(), total);
        let q = |text: &str, topic: Option<&str>, sort| ExploreQuery { text: text.into(), topic: topic.map(Into::into), sort };
        let all = q("", None, ExploreSort::Name);
        assert_eq!(names(core.explore(None, &all, 0, 10).await.unwrap()), (vec!["engine".into(), "web_site".into()], 2));
        assert_eq!(core.explore(Some(&ada), &all, 0, 10).await.unwrap().1, 3);
        assert_eq!(names(core.explore(None, &all, 1, 1).await.unwrap()).0, ["web_site"]);
        // Text matches owner/name and descriptions; % and _ are literal.
        assert_eq!(names(core.explore(None, &q("ANALYTICAL", None, ExploreSort::Updated), 0, 10).await.unwrap()).0, ["engine"]);
        assert_eq!(names(core.explore(None, &q("bob/", None, ExploreSort::Updated), 0, 10).await.unwrap()).0, ["web_site"]);
        assert_eq!(core.explore(Some(&ada), &q("100%", None, ExploreSort::Updated), 0, 10).await.unwrap().1, 1);
        assert_eq!(core.explore(Some(&ada), &q("_", None, ExploreSort::Updated), 0, 10).await.unwrap().1, 1);
        assert_eq!(names(core.explore(None, &q("", Some("web"), ExploreSort::Updated), 0, 10).await.unwrap()).0, ["web_site"]);
        core.set_watching(bob.id, engine.id, true).await.unwrap();
        assert_eq!(names(core.explore(None, &q("", None, ExploreSort::Watchers), 0, 10).await.unwrap()).0[0], "engine");

        // Private repositories' topics only count for those who see them.
        assert_eq!(core.popular_topics(None, 10).await.unwrap()[0], ("rust".into(), 2));
        assert_eq!(core.popular_topics(Some(&ada), 10).await.unwrap()[0], ("rust".into(), 3));
        core.set_topics(engine.id, &[]).await.unwrap();
        assert_eq!(core.popular_topics(None, 10).await.unwrap().len(), 2);
    }
}
