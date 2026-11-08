//! Searching issues and pull requests with GitHub's qualifiers, in one
//! repository or across all those the viewer can read:
//!
//! `is:open|closed|merged|unmerged|issue|pr`, `state:open|closed`,
//! `label:x` (all of them), `-label:x`, `author:`, `assignee:`,
//! `review-requested:`, `involves:` (author, assignee, commenter or
//! reviewer), `milestone:"v1.0"`, `no:label|assignee|milestone`,
//! `repo:owner/name`, `user:`/`org:` (the owner), `project:owner/name/1`
//! (a repository's board) or `project:org/1` (an organization's),
//! `sort:created-desc`
//! (also `-asc`, `updated-…`, `comments-…`), and words to find in titles
//! and bodies. `@me` is the viewer. Values with spaces go in quotes.

use serde_json::json;

use crate::{Core, Issue, Result, User, invalid};

/// Most words, and most labels, one search can have: each becomes a
/// condition checked for every issue, and anyone can search.
const MAX_TERMS: usize = 10;
/// Longest search, in characters.
const MAX_SEARCH_CHARS: usize = 256;

/// How results are ordered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IssueSort {
    #[default]
    CreatedDesc,
    CreatedAsc,
    UpdatedDesc,
    UpdatedAsc,
    CommentsDesc,
    CommentsAsc,
}

impl IssueSort {
    pub const ALL: [IssueSort; 6] =
        [Self::CreatedDesc, Self::CreatedAsc, Self::UpdatedDesc, Self::UpdatedAsc, Self::CommentsDesc, Self::CommentsAsc];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::CreatedDesc => "created-desc",
            Self::CreatedAsc => "created-asc",
            Self::UpdatedDesc => "updated-desc",
            Self::UpdatedAsc => "updated-asc",
            Self::CommentsDesc => "comments-desc",
            Self::CommentsAsc => "comments-asc",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::CreatedDesc => "Newest",
            Self::CreatedAsc => "Oldest",
            Self::UpdatedDesc => "Recently updated",
            Self::UpdatedAsc => "Least recently updated",
            Self::CommentsDesc => "Most commented",
            Self::CommentsAsc => "Least commented",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        let s = match s {
            "created" => "created-desc",
            "updated" => "updated-desc",
            "comments" => "comments-desc",
            s => s,
        };
        Self::ALL.into_iter().find(|v| v.as_str() == s)
    }
}

/// Which state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateFilter {
    Open,
    /// Closed or merged.
    Closed,
    Merged,
    /// Closed pull requests that weren't merged.
    Unmerged,
}

/// A parsed search.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IssueQuery {
    /// `Some(true)`: pull requests only.
    pub pulls: Option<bool>,
    pub state: Option<StateFilter>,
    pub labels: Vec<String>,
    pub not_labels: Vec<String>,
    pub author: Option<String>,
    pub assignee: Option<String>,
    pub review_requested: Option<String>,
    pub involves: Option<String>,
    pub milestone: Option<String>,
    pub no_label: bool,
    pub no_assignee: bool,
    pub no_milestone: bool,
    /// `Some(true)`: draft pull requests only; `Some(false)`: no drafts.
    pub draft: Option<bool>,
    /// `owner/name`.
    pub repo: Option<String>,
    pub owner: Option<String>,
    /// A board: `owner/name/number`, or `org/number`.
    pub project: Option<String>,
    pub sort: IssueSort,
    /// Words to find in titles and bodies.
    pub text: Vec<String>,
    /// Of the search as written, in characters.
    pub length: usize,
}

impl IssueQuery {
    /// Whether the search is small enough to run.
    fn check(&self) -> Result<()> {
        let labels = self.labels.len() + self.not_labels.len();
        if self.length > MAX_SEARCH_CHARS || self.text.len() > MAX_TERMS || labels > MAX_TERMS {
            return invalid(format!(
                "Searches are at most {MAX_SEARCH_CHARS} characters, with up to {MAX_TERMS} words and {MAX_TERMS} labels."
            ));
        }
        Ok(())
    }
}

/// Splits on whitespace, keeping quoted parts together (and unquoting
/// them): `label:"good first issue" crash`.
fn tokens(q: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut any = false;
    for c in q.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            c if c.is_whitespace() && !quoted => {
                if any || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                any = false;
            }
            c => cur.push(c),
        }
    }
    if any || !cur.is_empty() {
        out.push(cur);
    }
    out.retain(|t| !t.is_empty());
    out
}

impl IssueQuery {
    /// Parses a search; `me` is who `@me` stands for. Unknown qualifiers are
    /// searched for as words.
    pub fn parse(q: &str, me: Option<&str>) -> Self {
        let mut out = Self { length: q.chars().count(), ..Self::default() };
        let user = |v: &str| -> String {
            match (v, me) {
                ("@me", Some(me)) => me.to_owned(),
                _ => v.trim_start_matches('@').to_owned(),
            }
        };
        for token in tokens(q) {
            let Some((key, value)) = token.split_once(':').filter(|(k, v)| !k.is_empty() && !v.is_empty()) else {
                out.text.push(token);
                continue;
            };
            match (key.to_ascii_lowercase().as_str(), value.to_ascii_lowercase().as_str()) {
                ("is", "open") | ("state", "open") => out.state = Some(StateFilter::Open),
                ("is", "closed") | ("state", "closed") => out.state = Some(StateFilter::Closed),
                ("is", "merged") => {
                    out.state = Some(StateFilter::Merged);
                    out.pulls = Some(true);
                }
                ("is", "unmerged") => {
                    out.state = Some(StateFilter::Unmerged);
                    out.pulls = Some(true);
                }
                ("is" | "type", "issue") => out.pulls = Some(false),
                ("is", "draft") | ("draft", "true") => {
                    out.draft = Some(true);
                    out.pulls = Some(true);
                }
                ("draft", "false") => {
                    out.draft = Some(false);
                    out.pulls = Some(true);
                }
                ("is" | "type", "pr" | "pull-request" | "pull") => out.pulls = Some(true),
                ("label", _) => out.labels.push(value.to_owned()),
                ("-label", _) => out.not_labels.push(value.to_owned()),
                ("author", _) => out.author = Some(user(value)),
                ("assignee", _) => out.assignee = Some(user(value)),
                ("review-requested", _) => out.review_requested = Some(user(value)),
                ("involves", _) => out.involves = Some(user(value)),
                ("milestone", _) => out.milestone = Some(value.to_owned()),
                ("no", "label") => out.no_label = true,
                ("no", "assignee") => out.no_assignee = true,
                ("no", "milestone") => out.no_milestone = true,
                ("repo", _) if value.contains('/') => out.repo = Some(value.to_owned()),
                ("user" | "org" | "owner", _) => out.owner = Some(value.to_owned()),
                ("project", _) if value.contains('/') => out.project = Some(value.to_owned()),
                ("sort", v) if IssueSort::parse(v).is_some() => out.sort = IssueSort::parse(v).unwrap_or_default(),
                _ => out.text.push(token),
            }
        }
        out
    }
}

/// An issue or pull request found, with its repository.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct IssueHit {
    #[sqlx(flatten)]
    pub issue: Issue,
    pub repo_owner: String,
    pub repo_name: String,
}

// Parameters: ?1 repository or NULL, ?2/?3 the viewer (see `visible_to!`),
// ?4 pull requests (NULL: both), ?5 state, ?6 author, ?7 assignee, ?8
// review requested, ?9 involves (user ids; -2 for unknown names), ?10
// milestone title, ?11 labels and ?12 excluded labels (JSON arrays), ?13
// no label, ?14 no assignee, ?15 no milestone, ?16 owner id, ?17 words
// (JSON array of LIKE patterns), ?18 drafts (NULL: either), ?19 the id of
// a board the issue must be on (NULL: any).
macro_rules! issue_search_where {
    () => {
        concat!(
            "FROM issues i JOIN repositories r ON r.id = i.repo_id JOIN users ru ON ru.id = r.owner_id
             LEFT JOIN users u ON u.id = i.author_id
             WHERE (?1 IS NULL OR i.repo_id = ?1)
               AND (?4 IS NULL OR i.is_pull = ?4)
               AND (?5 IS NULL
                    OR (?5 = 'open' AND i.state = 'open')
                    OR (?5 = 'closed' AND i.state != 'open')
                    OR (?5 = 'merged' AND i.state = 'merged')
                    OR (?5 = 'unmerged' AND i.state = 'closed'))
               AND (?6 IS NULL OR i.author_id = ?6)
               AND (?7 IS NULL OR EXISTS (SELECT 1 FROM issue_assignees a WHERE a.issue_id = i.id AND a.user_id = ?7))
               AND (?8 IS NULL OR EXISTS (SELECT 1 FROM review_requests rr WHERE rr.issue_id = i.id AND rr.user_id = ?8))
               AND (?9 IS NULL OR i.author_id = ?9
                    OR EXISTS (SELECT 1 FROM issue_assignees a WHERE a.issue_id = i.id AND a.user_id = ?9)
                    OR EXISTS (SELECT 1 FROM review_requests rr WHERE rr.issue_id = i.id AND rr.user_id = ?9)
                    OR EXISTS (SELECT 1 FROM comments c WHERE c.issue_id = i.id AND c.author_id = ?9))
               AND (?10 IS NULL OR EXISTS (SELECT 1 FROM milestones m WHERE m.id = i.milestone_id AND m.title = ?10))
               AND NOT EXISTS (SELECT 1 FROM json_each(?11) j WHERE NOT EXISTS (
                    SELECT 1 FROM issue_labels il JOIN labels l ON l.id = il.label_id
                    WHERE il.issue_id = i.id AND lower(l.name) = lower(j.value)))
               AND NOT EXISTS (SELECT 1 FROM json_each(?12) j JOIN labels l ON lower(l.name) = lower(j.value)
                    JOIN issue_labels il ON il.label_id = l.id WHERE il.issue_id = i.id)
               AND (NOT ?13 OR NOT EXISTS (SELECT 1 FROM issue_labels il WHERE il.issue_id = i.id))
               AND (NOT ?14 OR NOT EXISTS (SELECT 1 FROM issue_assignees a WHERE a.issue_id = i.id))
               AND (NOT ?15 OR i.milestone_id IS NULL)
               AND (?16 IS NULL OR r.owner_id = ?16)
               AND (?18 IS NULL OR EXISTS (SELECT 1 FROM pull_requests p WHERE p.issue_id = i.id AND p.is_draft = ?18))
               AND (?19 IS NULL OR EXISTS (SELECT 1 FROM project_cards pc WHERE pc.issue_id = i.id AND pc.project_id = ?19))
               AND NOT EXISTS (SELECT 1 FROM json_each(?17) j
                    WHERE i.title NOT LIKE j.value ESCAPE '\\' AND i.body NOT LIKE j.value ESCAPE '\\')
               AND ",
            visible_to!()
        )
    };
}

macro_rules! issue_search_page {
    ($order:literal) => {
        concat!(
            "SELECT i.id, i.repo_id, i.number, i.author_id, u.username AS author, i.title, i.body,
                i.is_pull, i.state, i.comment_count, i.created_at, i.updated_at, i.closed_at, i.original_author,
                i.is_pull AND EXISTS (SELECT 1 FROM pull_requests p WHERE p.issue_id = i.id AND p.is_draft) AS is_draft,
                ru.username AS repo_owner, r.name AS repo_name ",
            issue_search_where!(),
            " ORDER BY ",
            $order,
            " LIMIT ?20 OFFSET ?21"
        )
    };
}

/// The values bound to an issue search.
struct Bound {
    repo: Option<i64>,
    viewer: i64,
    admin: bool,
    pulls: Option<bool>,
    state: Option<&'static str>,
    users: [Option<i64>; 4],
    milestone: Option<String>,
    labels: String,
    not_labels: String,
    owner: Option<i64>,
    text: String,
    project: Option<i64>,
}

/// Unknown names match nobody.
const NOBODY: i64 = -2;

impl Core {
    /// The board `project:` names, if `viewer` can see it: `owner/name/1`
    /// or `org/1`.
    async fn project_id_for(&self, viewer: Option<&User>, project: &Option<String>) -> Result<Option<i64>> {
        let Some(spec) = project else { return Ok(None) };
        let parts: Vec<&str> = spec.split('/').collect();
        let found = match parts[..] {
            [owner, name, number] => match (self.repo_or_redirect(owner, name).await?, number.parse()) {
                (Some(repo), Ok(n)) if self.access(&repo, viewer).await? >= crate::Access::Read => {
                    self.project(crate::ProjectOwner::Repo(repo.id), n).await?
                }
                _ => None,
            },
            [org, number] => match (self.user_by_name(org).await?.filter(User::is_org), number.parse()) {
                (Some(org), Ok(n)) if self.org_board_access(&org, viewer).await? >= crate::Access::Read => {
                    self.project(crate::ProjectOwner::Org(org.id), n).await?
                }
                _ => None,
            },
            _ => None,
        };
        Ok(Some(found.map_or(NOBODY, |p| p.id)))
    }

    async fn user_id_for(&self, name: &Option<String>) -> Result<Option<i64>> {
        Ok(match name {
            Some(n) => Some(self.user_by_name(n).await?.map_or(NOBODY, |u| u.id)),
            None => None,
        })
    }

    /// Searches issues and pull requests `viewer` can read, in `repo_id`
    /// (or everywhere). Returns a page of them and how many there are.
    pub async fn search_issues(
        &self,
        viewer: Option<&User>,
        repo_id: Option<i64>,
        q: &IssueQuery,
        offset: i64,
        limit: i64,
    ) -> Result<(Vec<IssueHit>, i64)> {
        q.check()?;
        let repo = match (repo_id, &q.repo) {
            (Some(id), _) => Some(id),
            (None, Some(full)) => {
                let (owner, name) = full.split_once('/').unwrap_or((full, ""));
                Some(self.repo_or_redirect(owner, name).await?.map_or(NOBODY, |r| r.id))
            }
            (None, None) => None,
        };
        let like = |w: &String| format!("%{}%", w.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
        let b = Bound {
            repo,
            viewer: viewer.map_or(-1, |u| u.id),
            admin: viewer.is_some_and(|u| u.is_admin),
            pulls: q.pulls,
            state: q.state.map(|s| match s {
                StateFilter::Open => "open",
                StateFilter::Closed => "closed",
                StateFilter::Merged => "merged",
                StateFilter::Unmerged => "unmerged",
            }),
            users: [
                self.user_id_for(&q.author).await?,
                self.user_id_for(&q.assignee).await?,
                self.user_id_for(&q.review_requested).await?,
                self.user_id_for(&q.involves).await?,
            ],
            milestone: q.milestone.clone(),
            labels: json!(q.labels).to_string(),
            not_labels: json!(q.not_labels).to_string(),
            owner: self.user_id_for(&q.owner).await?,
            text: json!(q.text.iter().map(like).collect::<Vec<_>>()).to_string(),
            project: self.project_id_for(viewer, &q.project).await?,
        };
        macro_rules! bind_all {
            ($query:expr) => {
                $query
                    .bind(b.repo)
                    .bind(b.viewer)
                    .bind(b.admin)
                    .bind(b.pulls)
                    .bind(b.state)
                    .bind(b.users[0])
                    .bind(b.users[1])
                    .bind(b.users[2])
                    .bind(b.users[3])
                    .bind(&b.milestone)
                    .bind(&b.labels)
                    .bind(&b.not_labels)
                    .bind(q.no_label)
                    .bind(q.no_assignee)
                    .bind(q.no_milestone)
                    .bind(b.owner)
                    .bind(&b.text)
                    .bind(q.draft)
                    .bind(b.project)
            };
        }
        let sql = match q.sort {
            IssueSort::CreatedDesc => issue_search_page!("i.created_at DESC, i.id DESC"),
            IssueSort::CreatedAsc => issue_search_page!("i.created_at, i.id"),
            IssueSort::UpdatedDesc => issue_search_page!("i.updated_at DESC, i.id DESC"),
            IssueSort::UpdatedAsc => issue_search_page!("i.updated_at, i.id"),
            IssueSort::CommentsDesc => issue_search_page!("i.comment_count DESC, i.id DESC"),
            IssueSort::CommentsAsc => issue_search_page!("i.comment_count, i.id"),
        };
        let hits = bind_all!(sqlx::query_as::<_, IssueHit>(sql)).bind(limit).bind(offset).fetch_all(&self.db).await?;
        let (total,): (i64,) = bind_all!(sqlx::query_as(concat!("SELECT COUNT(*) ", issue_search_where!())))
            .fetch_one(&self.db)
            .await?;
        Ok((hits, total))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsing() {
        let q = IssueQuery::parse(r#"is:open is:pr label:"good first issue" -label:wontfix author:@me crash sort:updated foo:bar"#, Some("ada"));
        assert_eq!(q.state, Some(StateFilter::Open));
        assert_eq!(q.pulls, Some(true));
        assert_eq!(q.labels, ["good first issue"]);
        assert_eq!(q.not_labels, ["wontfix"]);
        assert_eq!(q.author.as_deref(), Some("ada"));
        assert_eq!(q.sort, IssueSort::UpdatedDesc);
        assert_eq!(q.text, ["crash", "foo:bar"]);
        let q = IssueQuery::parse("no:assignee repo:ada/engine is:merged milestone:\"v1.0\"", None);
        assert!(q.no_assignee && q.state == Some(StateFilter::Merged) && q.pulls == Some(true));
        assert_eq!((q.repo.as_deref(), q.milestone.as_deref()), (Some("ada/engine"), Some("v1.0")));
        assert_eq!(IssueQuery::parse("  ", None), IssueQuery { length: 2, ..IssueQuery::default() });
        let q = IssueQuery::parse("is:draft", None);
        assert_eq!((q.draft, q.pulls), (Some(true), Some(true)));
        assert_eq!(IssueQuery::parse("draft:false", None).draft, Some(false));
    }

    #[tokio::test]
    async fn searching() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let bob = core.register("bob", "bob@example.com", "password1").await.unwrap();
        let engine = core.create_repo(&ada, "engine", "", false).await.unwrap();
        let secret = core.create_repo(&ada, "secret", "", true).await.unwrap();
        let crash = core.create_issue(engine.id, &bob, "Crash on start", "It crashes 100% of the time").await.unwrap();
        let slow = core.create_issue(engine.id, &ada, "Slow builds", "").await.unwrap();
        let hidden = core.create_issue(secret.id, &ada, "Secret crash", "").await.unwrap();
        let bug = core.labels(engine.id).await.unwrap().into_iter().find(|l| l.name == "bug").unwrap();
        core.set_issue_labels(&crash, &ada, &[bug.id]).await.unwrap();
        core.add_comment(&slow, &bob, "Same").await.unwrap();
        core.set_issue_open(&slow, &ada, false).await.unwrap();

        async fn find_in(core: &Core, q: &str, viewer: Option<&User>) -> (Vec<String>, i64) {
            let q = IssueQuery::parse(q, viewer.map(|v| v.username.as_str()));
            let (hits, total) = core.search_issues(viewer, None, &q, 0, 10).await.unwrap();
            (hits.into_iter().map(|h| h.issue.title).collect(), total)
        }
        assert_eq!(find_in(&core, "crash", None).await, (vec!["Crash on start".into()], 1), "private repositories are hidden");
        assert_eq!(find_in(&core, "crash", Some(&ada)).await.1, 2);
        assert_eq!(find_in(&core, "100%", None).await.1, 1);
        assert_eq!(find_in(&core, "10_%", None).await.1, 0, "LIKE wildcards are literal");
        assert_eq!(find_in(&core, "is:open label:bug", None).await.0, ["Crash on start"]);
        assert_eq!(find_in(&core, "-label:bug", None).await.0, ["Slow builds"]);
        assert_eq!(find_in(&core, "no:label is:closed", None).await.0, ["Slow builds"]);
        assert_eq!(find_in(&core, "involves:@me", Some(&bob)).await.1, 2, "authored one, commented on the other");
        assert_eq!(find_in(&core, "author:nobody", None).await.1, 0);
        assert_eq!(find_in(&core, "repo:ada/secret", Some(&bob)).await.1, 0);
        assert_eq!(find_in(&core, "user:ada sort:created-asc", Some(&ada)).await.0, ["Crash on start", "Slow builds", "Secret crash"]);
        assert_eq!(find_in(&core, "is:pr", Some(&ada)).await.1, 0);
        // Big searches are refused before they reach the database.
        for q in ["crash ".repeat(11), "x".repeat(257), "label:a ".repeat(11)] {
            let err = core.search_issues(None, None, &IssueQuery::parse(&q, None), 0, 10).await.unwrap_err();
            assert!(err.to_string().contains("at most 256 characters"), "{err}");
        }
        assert_eq!(find_in(&core, &"crash ".repeat(10), None).await.1, 1, "ten words are fine");
        let q = IssueQuery::parse("", None);
        let (hits, total) = core.search_issues(Some(&ada), Some(secret.id), &q, 0, 10).await.unwrap();
        assert_eq!((hits[0].issue.id, total, hits[0].repo_name.as_str()), (hidden.id, 1, "secret"));
    }
}
