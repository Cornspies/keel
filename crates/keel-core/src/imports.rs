//! Importing a repository from GitHub or Gitea (and Forgejo): its git
//! data and Git LFS files, then optionally its labels, milestones, issues
//! and pull requests (with their numbers, comments and reviews), releases
//! and wiki.
//!
//! keel-mirror's import worker talks to the source's API and hands what it
//! finds to the functions here, which write it directly: imports don't
//! notify anyone or send webhooks. Imported authors have no account here;
//! their profile on the source is kept with what they wrote.

use std::sync::Arc;

use crate::mirrors::{FETCH_LIMIT, MirrorRemote, parse_mirror_url};
use crate::{Core, Release, Repo, Result, User, invalid, now};

/// Where a repository is imported from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportService {
    GitHub,
    /// Gitea, Forgejo (e.g. Codeberg) and their compatible API.
    Gitea,
}

impl ImportService {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::GitHub => "github",
            Self::Gitea => "gitea",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::GitHub => "GitHub",
            Self::Gitea => "Gitea",
        }
    }
}

/// A source repository's addresses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportSource {
    pub service: ImportService,
    /// Its page, e.g. `https://github.com/ada/engine`.
    pub url: String,
    /// Its API, e.g. `https://api.github.com/repos/ada/engine`.
    pub api_url: String,
    /// What git clones, e.g. `https://github.com/ada/engine.git`.
    pub git_url: String,
    /// The suggested name here.
    pub name: String,
}

/// Works out the source from a repository's page or clone URL: GitHub for
/// github.com, Gitea's API otherwise.
pub fn import_source(url: &str) -> Result<ImportSource> {
    let parsed = parse_mirror_url(url)?;
    if parsed.username.is_some() {
        return invalid("Leave credentials out of the URL; give an access token instead.");
    }
    let (scheme, rest) = parsed.url.split_once("://").expect("parsed");
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let parts: Vec<&str> = path.trim_matches('/').split('/').filter(|p| !p.is_empty()).collect();
    let [owner, name] = parts.as_slice() else {
        return invalid("Enter the repository's address, like https://github.com/owner/name.");
    };
    let name = name.strip_suffix(".git").unwrap_or(name);
    let host = parsed.host.to_ascii_lowercase();
    let (service, api_url) = if host == "github.com" || host == "www.github.com" {
        (ImportService::GitHub, format!("https://api.github.com/repos/{owner}/{name}"))
    } else {
        (ImportService::Gitea, format!("{scheme}://{authority}/api/v1/repos/{owner}/{name}"))
    };
    let base = if service == ImportService::GitHub { "https://github.com".to_owned() } else { format!("{scheme}://{authority}") };
    Ok(ImportSource {
        service,
        url: format!("{base}/{owner}/{name}"),
        git_url: format!("{base}/{owner}/{name}.git"),
        api_url,
        name: name.to_owned(),
    })
}

/// What to bring along besides the git data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImportOptions {
    pub issues: bool,
    pub releases: bool,
    pub wiki: bool,
}

/// An import's state, for the repository's page.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct Import {
    pub repo_id: i64,
    pub service: String,
    pub url: String,
    pub api_url: String,
    pub with_issues: bool,
    pub with_releases: bool,
    pub with_wiki: bool,
    /// `queued`, `running`, `done` or `failed`.
    pub status: String,
    pub progress: String,
    pub error: Option<String>,
    pub created_by: Option<i64>,
    pub created_at: i64,
    pub finished_at: Option<i64>,
}

impl Import {
    pub fn service(&self) -> ImportService {
        if self.service == "github" { ImportService::GitHub } else { ImportService::Gitea }
    }

    pub fn in_progress(&self) -> bool {
        matches!(self.status.as_str(), "queued" | "running")
    }

    /// Whether the repository's page mentions it: while it runs, and for a
    /// week after it ended with problems.
    pub fn worth_showing(&self) -> bool {
        self.in_progress() || (self.error.is_some() && self.finished_at.is_some_and(|t| now() - t < 7 * 86400))
    }
}

/// A label from the source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportedLabel {
    pub name: String,
    /// `#rrggbb` or `rrggbb`.
    pub color: String,
    pub description: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportedMilestone {
    pub title: String,
    pub description: String,
    pub due_on: Option<i64>,
    pub closed: bool,
    pub created_at: i64,
    pub closed_at: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportedPull {
    pub base_ref: String,
    pub head_ref: String,
    pub head_commit: Option<String>,
    pub merge_commit: Option<String>,
    pub merged_at: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportedIssue {
    pub number: i64,
    pub title: String,
    pub body: String,
    /// The author's profile on the source.
    pub author: String,
    /// `open`, `closed` or (pull requests) `merged`.
    pub state: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub closed_at: Option<i64>,
    pub labels: Vec<String>,
    pub milestone: Option<String>,
    pub pull: Option<ImportedPull>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportedComment {
    pub issue_number: i64,
    /// `comment`, `approved` or `changes_requested`.
    pub kind: String,
    pub body: String,
    pub author: String,
    pub created_at: i64,
    pub updated_at: i64,
    /// Reviews: the commit reviewed.
    pub commit_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportedRelease {
    pub tag: String,
    pub title: String,
    pub notes: String,
    pub draft: bool,
    pub prerelease: bool,
    pub author: String,
    pub created_at: i64,
    pub published_at: Option<i64>,
}

/// The nearest of keel's label colors to an `rrggbb` color.
pub fn label_color(hex: &str) -> &'static str {
    let hex = hex.trim_start_matches('#');
    let Ok(v) = u32::from_str_radix(hex, 16) else { return "gray" };
    if hex.len() != 6 {
        return "gray";
    }
    let (r, g, b) = (((v >> 16) & 255) as f32 / 255.0, ((v >> 8) & 255) as f32 / 255.0, (v & 255) as f32 / 255.0);
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    let (light, chroma) = ((max + min) / 2.0, max - min);
    if chroma < 0.12 {
        return "gray";
    }
    let hue = if max == r {
        60.0 * ((g - b) / chroma).rem_euclid(6.0)
    } else if max == g {
        60.0 * ((b - r) / chroma + 2.0)
    } else {
        60.0 * ((r - g) / chroma + 4.0)
    };
    match hue {
        h if !(15.0..345.0).contains(&h) => "red",
        h if h < 45.0 && light < 0.4 => "brown",
        h if h < 40.0 => "orange",
        h if h < 70.0 => "yellow",
        h if h < 160.0 => "green",
        h if h < 200.0 => "teal",
        h if h < 245.0 => "blue",
        h if h < 290.0 => "purple",
        _ => "pink",
    }
}

impl Core {
    pub fn import_wakeup(&self) -> Arc<tokio::sync::Notify> {
        self.import_wakeup.clone()
    }

    /// Creates `owner/name` and queues importing `source` into it.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_import(
        &self,
        owner: &User,
        by: &User,
        name: &str,
        description: &str,
        is_private: bool,
        source: &ImportSource,
        token: &str,
        options: ImportOptions,
    ) -> Result<Repo> {
        self.check_mirror_host(&parse_mirror_url(&source.git_url)?).await?;
        let repo = self.create_repo(owner, name, description, is_private).await?;
        sqlx::query(
            "INSERT INTO imports (repo_id, service, url, api_url, token, with_issues, with_releases, with_wiki, created_by, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )
        .bind(repo.id)
        .bind(source.service.as_str())
        .bind(&source.url)
        .bind(&source.api_url)
        .bind(token.trim())
        .bind(options.issues)
        .bind(options.releases)
        .bind(options.wiki)
        .bind(by.id)
        .bind(now())
        .execute(&self.db)
        .await?;
        self.import_wakeup.notify_one();
        Ok(repo)
    }

    pub async fn import(&self, repo_id: i64) -> Result<Option<Import>> {
        Ok(sqlx::query_as(
            "SELECT repo_id, service, url, api_url, with_issues, with_releases, with_wiki, status, progress, error,
                created_by, created_at, finished_at
             FROM imports WHERE repo_id = ?1",
        )
        .bind(repo_id)
        .fetch_optional(&self.db)
        .await?)
    }

    /// The access token of a running import.
    pub async fn import_token(&self, repo_id: i64) -> Result<String> {
        let (token,): (String,) = sqlx::query_as("SELECT token FROM imports WHERE repo_id = ?1")
            .bind(repo_id)
            .fetch_one(&self.db)
            .await?;
        Ok(token)
    }

    /// Takes the next queued import, marking it as running.
    pub async fn claim_import(&self) -> Result<Option<i64>> {
        Ok(sqlx::query_as::<_, (i64,)>(
            "UPDATE imports SET status = 'running', progress = 'Starting'
             WHERE repo_id = (SELECT repo_id FROM imports WHERE status = 'queued' ORDER BY created_at LIMIT 1)
             RETURNING repo_id",
        )
        .fetch_optional(&self.db)
        .await?
        .map(|(id,)| id))
    }

    /// Imports that were running when the server stopped won't finish.
    pub async fn fail_interrupted_imports(&self) -> Result<()> {
        sqlx::query(
            "UPDATE imports SET status = 'failed', error = 'The server restarted during the import.', token = '',
                finished_at = ?1
             WHERE status = 'running'",
        )
        .bind(now())
        .execute(&self.db)
        .await?;
        Ok(())
    }

    pub async fn set_import_progress(&self, repo_id: i64, progress: &str) -> Result<()> {
        sqlx::query("UPDATE imports SET progress = ?2 WHERE repo_id = ?1").bind(repo_id).bind(progress).execute(&self.db).await?;
        Ok(())
    }

    /// Records how the import ended, forgetting its token.
    pub async fn finish_import(&self, repo_id: i64, error: Option<&str>) -> Result<()> {
        sqlx::query(
            "UPDATE imports SET status = ?2, error = ?3, progress = '', token = '', finished_at = ?4 WHERE repo_id = ?1",
        )
        .bind(repo_id)
        .bind(if error.is_some() { "failed" } else { "done" })
        .bind(error)
        .bind(now())
        .execute(&self.db)
        .await?;
        if let Some(e) = error {
            tracing::info!(repo = repo_id, "import failed: {e}");
        }
        Ok(())
    }

    /// Where to fetch git data from, with the token as credentials.
    pub async fn import_remote(&self, repo_id: i64, git_url: &str) -> Result<MirrorRemote> {
        let token = self.import_token(repo_id).await?;
        let username = if token.is_empty() { String::new() } else { "oauth2".to_owned() };
        Ok(MirrorRemote { url: git_url.to_owned(), username, password: token })
    }

    /// Fetches the branches, tags and pull request heads, handled like a
    /// push. Returns the Git LFS files the commits need.
    pub async fn import_git(&self, repo_id: i64, remote: &MirrorRemote) -> Result<Vec<(String, u64)>> {
        let before = self.ref_snapshot(repo_id).await?;
        let path = self.config.repo_path(repo_id);
        self.with_remote(remote, move |r| keel_git::import_fetch(&path, r, FETCH_LIMIT)).await?;
        let after = self.ref_snapshot(repo_id).await?;
        let changes: Vec<_> = keel_git::ref_changes(&before, &after)
            .into_iter()
            .filter_map(|c| Some((c.old, c.new?)))
            .collect();
        let wanted = self.git(repo_id, move |g| g.lfs_pointers(&changes)).await?;
        self.after_push(repo_id, None, Some(before)).await?;
        Ok(wanted)
    }

    /// Fetches the source's wiki, if it has one. Returns whether it did.
    pub async fn import_wiki(&self, repo: &Repo, remote: &MirrorRemote) -> Result<bool> {
        let wiki_url = format!("{}.wiki.git", remote.url.strip_suffix(".git").unwrap_or(&remote.url));
        let wiki = MirrorRemote { url: wiki_url, ..remote.clone() };
        let path = self.config.repo_path(repo.id);
        match self.with_remote(&wiki, move |r| keel_git::ls_remote(&path, r)).await {
            Ok(refs) if !refs.is_empty() => {}
            // No wiki (or an empty one).
            _ => return Ok(false),
        }
        self.ensure_wiki(repo.id).await?;
        let git = self.wiki_path(repo.id);
        self.with_remote(&wiki, move |r| keel_git::mirror_fetch(&git, r, FETCH_LIMIT)).await?;
        // Maintenance (and a HEAD pointing at an existing branch).
        self.after_wiki_change(repo, None, Vec::new()).await;
        Ok(true)
    }

    // -- Data ---------------------------------------------------------------

    /// Adds labels, or updates those with the same name.
    pub async fn import_labels(&self, repo_id: i64, labels: &[ImportedLabel]) -> Result<()> {
        for l in labels {
            let name: String = l.name.trim().chars().take(50).collect();
            if name.is_empty() {
                continue;
            }
            let description: String = l.description.chars().take(200).collect();
            sqlx::query(
                "INSERT INTO labels (repo_id, name, color, description, created_at) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT DO UPDATE SET color = excluded.color, description = excluded.description",
            )
            .bind(repo_id)
            .bind(&name)
            .bind(label_color(&l.color))
            .bind(description)
            .bind(now())
            .execute(&self.db)
            .await?;
        }
        Ok(())
    }

    pub async fn import_milestone(&self, repo_id: i64, m: &ImportedMilestone) -> Result<()> {
        sqlx::query(
            "INSERT INTO milestones (repo_id, title, description, due_on, state, created_at, closed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) ON CONFLICT DO NOTHING",
        )
        .bind(repo_id)
        .bind(m.title.trim())
        .bind(&m.description)
        .bind(m.due_on)
        .bind(if m.closed { "closed" } else { "open" })
        .bind(m.created_at)
        .bind(m.closed_at)
        .execute(&self.db)
        .await?;
        Ok(())
    }

    /// Adds an issue or pull request with its number. `false` if the
    /// number is taken.
    pub async fn import_issue(&self, repo_id: i64, i: &ImportedIssue) -> Result<bool> {
        let mut tx = self.db.begin().await?;
        let state = match i.state.as_str() {
            "merged" if i.pull.is_some() => "merged",
            "closed" | "merged" => "closed",
            _ => "open",
        };
        let inserted: Option<(i64,)> = sqlx::query_as(
            "INSERT INTO issues (repo_id, number, author_id, original_author, title, body, is_pull, state, created_at,
                updated_at, closed_at, milestone_id)
             VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
                (SELECT id FROM milestones WHERE repo_id = ?1 AND title = ?11))
             ON CONFLICT DO NOTHING RETURNING id",
        )
        .bind(repo_id)
        .bind(i.number)
        .bind(&i.author)
        .bind(i.title.chars().take(256).collect::<String>())
        .bind(&i.body)
        .bind(i.pull.is_some())
        .bind(state)
        .bind(i.created_at)
        .bind(i.updated_at.max(i.created_at))
        .bind(i.closed_at.filter(|_| state != "open"))
        .bind(&i.milestone)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((id,)) = inserted else { return Ok(false) };
        for label in &i.labels {
            sqlx::query(
                "INSERT OR IGNORE INTO issue_labels (issue_id, label_id)
                 SELECT ?1, id FROM labels WHERE repo_id = ?2 AND lower(name) = lower(?3)",
            )
            .bind(id)
            .bind(repo_id)
            .bind(label)
            .execute(&mut *tx)
            .await?;
        }
        if let Some(p) = &i.pull {
            sqlx::query(
                "INSERT INTO pull_requests (issue_id, base_ref, head_ref, head_repo_id, head_commit, merge_commit, merged_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )
            .bind(id)
            .bind(&p.base_ref)
            .bind(&p.head_ref)
            .bind(repo_id)
            .bind(&p.head_commit)
            .bind(&p.merge_commit)
            .bind(p.merged_at)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(true)
    }

    /// Adds a comment or review to issue `issue_number`. `false` if there's
    /// no such issue.
    pub async fn import_comment(&self, repo_id: i64, c: &ImportedComment) -> Result<bool> {
        let kind = match c.kind.as_str() {
            "approved" | "changes_requested" => c.kind.as_str(),
            _ => "comment",
        };
        let inserted = sqlx::query(
            "INSERT INTO comments (issue_id, author_id, original_author, kind, body, created_at, updated_at, commit_id)
             SELECT id, NULL, ?3, ?4, ?5, ?6, ?7, ?8 FROM issues WHERE repo_id = ?1 AND number = ?2",
        )
        .bind(repo_id)
        .bind(c.issue_number)
        .bind(&c.author)
        .bind(kind)
        .bind(&c.body)
        .bind(c.created_at)
        .bind(c.updated_at.max(c.created_at))
        .bind(&c.commit_id)
        .execute(&self.db)
        .await?
        .rows_affected()
            > 0;
        if inserted && kind == "comment" {
            sqlx::query("UPDATE issues SET comment_count = comment_count + 1 WHERE repo_id = ?1 AND number = ?2")
                .bind(repo_id)
                .bind(c.issue_number)
                .execute(&self.db)
                .await?;
        }
        Ok(inserted)
    }

    /// Adds a release (its tag must have been imported). `None` if the
    /// repository already has one for the tag.
    pub async fn import_release(&self, repo_id: i64, r: &ImportedRelease) -> Result<Option<Release>> {
        let inserted: Option<(i64,)> = sqlx::query_as(
            "INSERT INTO releases (repo_id, tag, title, notes, draft, prerelease, original_author, created_at, published_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) ON CONFLICT DO NOTHING RETURNING id",
        )
        .bind(repo_id)
        .bind(&r.tag)
        .bind(r.title.chars().take(200).collect::<String>())
        .bind(&r.notes)
        .bind(r.draft)
        .bind(r.prerelease)
        .bind(&r.author)
        .bind(r.created_at)
        .bind(r.published_at.filter(|_| !r.draft))
        .fetch_optional(&self.db)
        .await?;
        match inserted {
            Some((id,)) => self.release(repo_id, id).await,
            None => Ok(None),
        }
    }

    /// After issues and pull requests are in: pull requests get their merge
    /// bases, and those merged meanwhile are marked so.
    pub async fn finish_imported_pulls(&self, repo_id: i64) -> Result<()> {
        self.sync_pulls(repo_id, None).await?;
        sqlx::query("UPDATE repositories SET updated_at = ?2 WHERE id = ?1").bind(repo_id).bind(now()).execute(&self.db).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources() {
        let gh = import_source("https://github.com/ada/engine.git").unwrap();
        assert_eq!(gh.service, ImportService::GitHub);
        assert_eq!(gh.api_url, "https://api.github.com/repos/ada/engine");
        assert_eq!((gh.url.as_str(), gh.name.as_str()), ("https://github.com/ada/engine", "engine"));
        let gitea = import_source("https://codeberg.org/ada/engine/").unwrap();
        assert_eq!(gitea.service, ImportService::Gitea);
        assert_eq!(gitea.api_url, "https://codeberg.org/api/v1/repos/ada/engine");
        assert_eq!(gitea.git_url, "https://codeberg.org/ada/engine.git");
        let port = import_source("http://127.0.0.1:3000/ada/engine").unwrap();
        assert_eq!(port.api_url, "http://127.0.0.1:3000/api/v1/repos/ada/engine");
        for bad in ["https://github.com/ada", "https://github.com/ada/engine/issues", "ftp://x/a/b", "https://u:p@github.com/a/b"] {
            assert!(import_source(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn colors() {
        assert_eq!(label_color("d73a4a"), "red");
        assert_eq!(label_color("#0075ca"), "blue");
        assert_eq!(label_color("a2eeef"), "teal");
        assert_eq!(label_color("7057ff"), "purple");
        assert_eq!(label_color("008672"), "teal");
        assert_eq!(label_color("e4e669"), "yellow");
        assert_eq!(label_color("ffffff"), "gray");
        assert_eq!(label_color("0e8a16"), "green");
        assert_eq!(label_color("d876e3"), "pink");
        assert_eq!(label_color("nonsense"), "gray");
    }

    #[tokio::test]
    async fn importing_data() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let repo = core.create_repo(&ada, "engine", "", false).await.unwrap();
        core.import_labels(repo.id, &[
            ImportedLabel { name: "bug".into(), color: "#0075ca".into(), description: "Broken".into() },
            ImportedLabel { name: "perf".into(), color: "e4e669".into(), description: String::new() },
        ])
        .await
        .unwrap();
        let labels = core.labels(repo.id).await.unwrap();
        assert_eq!(labels.iter().find(|l| l.name == "bug").unwrap().color, "blue", "updated");
        core.import_milestone(repo.id, &ImportedMilestone {
            title: "v1".into(),
            description: String::new(),
            due_on: None,
            closed: false,
            created_at: 1,
            closed_at: None,
        })
        .await
        .unwrap();
        let issue = ImportedIssue {
            number: 7,
            title: "Crash".into(),
            body: "Boom".into(),
            author: "https://github.com/octocat".into(),
            state: "closed".into(),
            created_at: 100,
            updated_at: 200,
            closed_at: Some(200),
            labels: vec!["BUG".into(), "missing".into()],
            milestone: Some("v1".into()),
            pull: None,
        };
        assert!(core.import_issue(repo.id, &issue).await.unwrap());
        assert!(!core.import_issue(repo.id, &issue).await.unwrap(), "the number is taken");
        let comment = ImportedComment {
            issue_number: 7,
            kind: "comment".into(),
            body: "Same".into(),
            author: "https://github.com/hubot".into(),
            created_at: 150,
            updated_at: 150,
            commit_id: None,
        };
        assert!(core.import_comment(repo.id, &comment).await.unwrap());
        assert!(!core.import_comment(repo.id, &ImportedComment { issue_number: 99, ..comment }).await.unwrap());

        let found = core.issue(repo.id, 7).await.unwrap().unwrap();
        assert_eq!((found.state.as_str(), found.author, found.comment_count), ("closed", None, 1));
        assert_eq!(found.original_author.as_deref(), Some("https://github.com/octocat"));
        assert_eq!(core.labels_of(&[found.id]).await.unwrap()[&found.id].len(), 1);
        let comments = core.comments(found.id).await.unwrap();
        assert_eq!(comments[0].original_author.as_deref(), Some("https://github.com/hubot"));
        // New issues continue after the imported numbers.
        assert_eq!(core.create_issue(repo.id, &ada, "Next", "").await.unwrap().number, 8);

        let release = ImportedRelease {
            tag: "v1.0".into(),
            title: "First".into(),
            notes: "Notes".into(),
            draft: false,
            prerelease: false,
            author: "https://github.com/octocat".into(),
            created_at: 300,
            published_at: Some(300),
        };
        let r = core.import_release(repo.id, &release).await.unwrap().unwrap();
        assert_eq!(r.author.as_deref(), Some("octocat"));
        assert!(core.import_release(repo.id, &release).await.unwrap().is_none());
    }
}
