//! Wikis. Like GitHub's, a repository's wiki is a second git repository
//! (`<id>.wiki.git` on disk, cloned as `<repo>.wiki.git`) holding one
//! Markdown file per page: `Getting-Started.md` is the page "Getting
//! Started". People edit pages on the web or clone the wiki and push.
//!
//! Pages can be in subdirectories (they're found by name), `Home` is the
//! front page, and `_Sidebar` and `_Footer` are shown around every page.
//! Anyone who can read the repository can read its wiki; people with write
//! access can change it.

use std::path::PathBuf;

use keel_git::{CommitSummary, GitRepo, Identity, Oid, RefChange, Refs};
use serde_json::{Value, json};

use crate::events::EventTarget;
use crate::{Core, Error, Repo, Result, User, invalid};

/// Branch for new wikis (pushes may use another, e.g. GitHub's `master`).
const WIKI_BRANCH: &str = "main";
const MAX_TITLE: usize = 100;
/// Extensions of page files, the first for new pages.
const EXTENSIONS: [&str; 2] = [".md", ".markdown"];
/// Pages shown around every page.
pub const SPECIAL: [&str; 2] = ["_Sidebar", "_Footer"];
/// Commits of a web edit are retried this often when someone pushed to
/// the wiki at the same moment.
const ATTEMPTS: usize = 3;

/// A page: its name (the file name without extension, e.g.
/// `Getting-Started`) and where it is in the wiki repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WikiPage {
    pub name: String,
    pub path: String,
}

impl WikiPage {
    /// "Getting Started".
    pub fn title(&self) -> String {
        page_title(&self.name)
    }

    /// `_Sidebar` and `_Footer`, which aren't listed.
    pub fn is_special(&self) -> bool {
        self.name.starts_with('_')
    }
}

pub fn page_title(name: &str) -> String {
    name.replace('-', " ")
}

/// The page name for a title: "Getting Started" is `Getting-Started`.
pub fn page_name(title: &str) -> Result<String> {
    let name: String = title.split_whitespace().collect::<Vec<_>>().join("-");
    if name.is_empty() {
        return invalid("Pages need a title.");
    }
    if name.chars().count() > MAX_TITLE {
        return invalid(format!("Page titles are at most {MAX_TITLE} characters."));
    }
    if name.starts_with('.') || name.chars().any(|c| c.is_control() || "/\\:*?\"<>|[]#%".contains(c)) {
        return invalid("Page titles can't start with “.” or contain / \\ : * ? \" < > | [ ] # %.");
    }
    // `_pages`, `_new`, … are the wiki's own addresses.
    if name.starts_with('_') && !SPECIAL.iter().any(|s| s.eq_ignore_ascii_case(&name)) {
        return invalid("Only _Sidebar and _Footer can start with “_”.");
    }
    Ok(name)
}

/// The page name of a file, if it's a page.
fn name_of(path: &str) -> Option<&str> {
    let file = path.rsplit('/').next()?;
    EXTENSIONS.iter().find_map(|ext| file.strip_suffix(ext)).filter(|n| !n.is_empty())
}

/// All pages in the wiki at `commit`, by title (`Home` first).
fn pages_at(g: &GitRepo, commit: Oid) -> keel_git::Result<Vec<WikiPage>> {
    let mut pages: Vec<WikiPage> = g
        .files(commit)?
        .into_iter()
        .filter_map(|f| Some(WikiPage { name: name_of(&f.path)?.to_owned(), path: f.path }))
        .collect();
    // Names ignore case; at the top level wins, then the shorter path.
    pages.sort_by_cached_key(|p| (p.name.to_lowercase(), p.path.matches('/').count(), p.path.clone()));
    pages.dedup_by(|b, a| a.name.eq_ignore_ascii_case(&b.name));
    pages.sort_by_cached_key(|p| (!p.name.eq_ignore_ascii_case("home"), p.name.to_lowercase()));
    Ok(pages)
}

fn find_page(pages: &[WikiPage], name: &str) -> Option<WikiPage> {
    pages.iter().find(|p| p.name.eq_ignore_ascii_case(name)).cloned()
}

/// A page's contents.
#[derive(Clone, Debug)]
pub struct WikiContent {
    pub page: WikiPage,
    pub text: String,
    /// The commit it was read from (for edits: what they're based on).
    pub commit: Oid,
    /// The last change to the page, up to `commit`.
    pub last: Option<CommitSummary>,
}

/// What a change did to a page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WikiAction {
    Created,
    Edited,
    Deleted,
}

impl WikiAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Edited => "edited",
            Self::Deleted => "deleted",
        }
    }
}

/// A page changed by an edit or a push, for the activity feed and webhooks.
#[derive(Clone, Debug)]
pub struct WikiChange {
    pub name: String,
    pub action: WikiAction,
    pub commit: Oid,
}

/// A page to save from the web.
pub struct WikiEdit<'a> {
    /// The page being edited; `None` to create one.
    pub page: Option<&'a str>,
    pub title: &'a str,
    pub text: &'a str,
    /// Commit message; a default one when empty.
    pub message: &'a str,
    /// The commit the editor started from, to detect conflicting edits.
    pub base: Option<Oid>,
}

/// What a git client asked for: a repository or its wiki.
#[derive(Clone, Debug)]
pub struct GitTarget {
    pub repo: Repo,
    pub wiki: bool,
}

/// How to run git for a [`GitTarget`].
pub struct GitSession {
    pub path: PathBuf,
    /// Environment for `receive-pack`: push limits and branch protection.
    pub env: Vec<(String, String)>,
    /// Refs before a push, to tell what it changed.
    pub before: Option<Refs>,
}

impl Core {
    pub fn wiki_path(&self, repo_id: i64) -> PathBuf {
        self.config.repos_dir().join(format!("{repo_id}.wiki.git"))
    }

    /// Whether the wiki exists (it's created with the first page or push).
    pub fn has_wiki(&self, repo_id: i64) -> bool {
        self.wiki_path(repo_id).is_dir()
    }

    pub(crate) async fn ensure_wiki(&self, repo_id: i64) -> Result<()> {
        let path = self.wiki_path(repo_id);
        tokio::task::spawn_blocking(move || match keel_git::create_bare(&path, WIKI_BRANCH) {
            // Someone else just created it.
            Err(_) if path.join("HEAD").is_file() => Ok(()),
            r => r,
        })
        .await
        .map_err(anyhow::Error::from)??;
        Ok(())
    }

    async fn wiki_git<T: Send + 'static>(
        &self,
        repo_id: i64,
        f: impl FnOnce(&GitRepo) -> keel_git::Result<T> + Send + 'static,
    ) -> Result<T> {
        let path = self.wiki_path(repo_id);
        let result = tokio::task::spawn_blocking(move || f(&GitRepo::open(&path)?))
            .await
            .map_err(anyhow::Error::from)?;
        Ok(result?)
    }

    /// The wiki's pages, `Home` first, then by title; empty without a wiki.
    pub async fn wiki_pages(&self, repo_id: i64) -> Result<Vec<WikiPage>> {
        if !self.has_wiki(repo_id) {
            return Ok(Vec::new());
        }
        self.wiki_git(repo_id, |g| match wiki_tip(g)? {
            Some((_, tip)) => pages_at(g, tip),
            None => Ok(Vec::new()),
        })
        .await
    }

    /// A page by name (ignoring case), now or at commit `rev`.
    pub async fn wiki_page(&self, repo_id: i64, name: &str, rev: Option<Oid>) -> Result<Option<WikiContent>> {
        if !self.has_wiki(repo_id) {
            return Ok(None);
        }
        let name = name.to_owned();
        self.wiki_git(repo_id, move |g| {
            let commit = match rev {
                Some(rev) => rev,
                None => match wiki_tip(g)? {
                    Some((_, tip)) => tip,
                    None => return Ok(None),
                },
            };
            let Some(page) = find_page(&pages_at(g, commit)?, &name) else { return Ok(None) };
            let Some(blob) = g.blob(commit, &page.path)? else { return Ok(None) };
            let last = g.log(commit, &page.path, 0, 1)?.into_iter().next();
            Ok(Some(WikiContent { text: String::from_utf8_lossy(&blob.data).into_owned(), page, commit, last }))
        })
        .await
        .or_else(|e| match e {
            // An unknown revision.
            Error::Internal(_) if rev.is_some() => Ok(None),
            e => Err(e),
        })
    }

    /// A file of the wiki (e.g. an image a page shows).
    pub async fn wiki_file(&self, repo_id: i64, path: &str) -> Result<Option<keel_git::Blob>> {
        if !self.has_wiki(repo_id) {
            return Ok(None);
        }
        let path = path.to_owned();
        self.wiki_git(repo_id, move |g| match wiki_tip(g)? {
            Some((_, tip)) => g.blob(tip, &path),
            None => Ok(None),
        })
        .await
    }

    /// Changes to a page, newest first.
    pub async fn wiki_history(&self, repo_id: i64, name: &str, skip: usize, limit: usize) -> Result<Option<(WikiPage, Vec<CommitSummary>)>> {
        if !self.has_wiki(repo_id) {
            return Ok(None);
        }
        let name = name.to_owned();
        self.wiki_git(repo_id, move |g| {
            let Some((_, tip)) = wiki_tip(g)? else { return Ok(None) };
            let Some(page) = find_page(&pages_at(g, tip)?, &name) else { return Ok(None) };
            let log = g.log(tip, &page.path, skip, limit)?;
            Ok(Some((page, log)))
        })
        .await
    }

    /// Creates or changes a page as `user`. Returns its name.
    pub async fn save_wiki_page(&self, repo: &Repo, user: &User, edit: WikiEdit<'_>) -> Result<String> {
        crate::issues::ensure_unarchived(&self.db, repo.id).await?;
        let name = page_name(edit.title)?;
        let text = edit.text.replace("\r\n", "\n");
        // Wikis count towards the owner's storage.
        self.check_repo_storage(repo.id, text.len() as u64 + 1).await?;
        self.ensure_wiki(repo.id).await?;
        let old = edit.page.map(str::to_owned);
        let (message, base) = (edit.message.trim().to_owned(), edit.base);
        let (who_name, who_email, signer) = (user.username.clone(), user.email.clone(), self.signer());
        let new_name = name.clone();
        let saved = self
            .wiki_git(repo.id, move |g| {
                let who = Identity::new(&who_name, &who_email).signed_by(&*signer);
                for _ in 0..ATTEMPTS {
                    let (branch, tip) = match wiki_tip(g)? {
                        Some(t) => t,
                        None => (g.default_branch()?.unwrap_or_else(|| WIKI_BRANCH.into()), Oid::ZERO_SHA1),
                    };
                    let tip = Some(tip).filter(|t| !t.is_zero());
                    let pages = match tip {
                        Some(t) => pages_at(g, t)?,
                        None => Vec::new(),
                    };
                    let existing = find_page(&pages, &new_name);
                    let (changes, default_message, action) = match &old {
                        None => {
                            if existing.is_some() {
                                return Ok(Err(format!("There already is a page called “{}”.", page_title(&new_name))));
                            }
                            (vec![(format!("{new_name}.md"), true)], format!("Create {}", page_title(&new_name)), WikiAction::Created)
                        }
                        Some(old) => {
                            let Some(page) = find_page(&pages, old) else {
                                return Ok(Err("Someone deleted this page while you were editing it.".into()));
                            };
                            // Changed since the editor opened? (Other pages may have.)
                            if let (Some(base), Some(tip)) = (base, tip)
                                && base != tip
                            {
                                let before = g.blob(base, &page.path).ok().flatten().map(|b| b.id);
                                let now = g.blob(tip, &page.path)?.map(|b| b.id);
                                if before != now {
                                    return Ok(Err(
                                        "Someone else changed this page while you were editing it. Copy your text, reload the page and make your changes again."
                                            .into(),
                                    ));
                                }
                            }
                            if page.name == new_name {
                                (vec![(page.path.clone(), true)], format!("Update {}", page_title(&new_name)), WikiAction::Edited)
                            } else {
                                if existing.is_some_and(|e| e.path != page.path) {
                                    return Ok(Err(format!("There already is a page called “{}”.", page_title(&new_name))));
                                }
                                let dir = page.path.rsplit_once('/').map_or("", |(d, _)| d);
                                let ext = EXTENSIONS.iter().find(|e| page.path.ends_with(*e)).copied().unwrap_or(".md");
                                let path = if dir.is_empty() { format!("{new_name}{ext}") } else { format!("{dir}/{new_name}{ext}") };
                                let message = format!("Rename {} to {}", page.title(), page_title(&new_name));
                                (vec![(page.path.clone(), false), (path, true)], message, WikiAction::Edited)
                            }
                        }
                    };
                    let message = if message.is_empty() { default_message } else { message.clone() };
                    let files: Vec<(&str, Option<&[u8]>)> =
                        changes.iter().map(|(p, write)| (p.as_str(), write.then_some(text.as_bytes()))).collect();
                    match g.commit_files(&branch, tip, &files, &message, &who) {
                        Ok(commit) => return Ok(Ok((commit, action))),
                        // A push or another edit moved the branch: try again.
                        Err(_) if wiki_tip(g)?.map(|(_, t)| t) != tip => continue,
                        Err(e) => return Err(e),
                    }
                }
                Ok(Err("The wiki is changing too fast; try again.".into()))
            })
            .await?;
        let (commit, action) = saved.or_else(invalid)?;
        self.after_wiki_change(repo, Some(user.id), vec![WikiChange { name: name.clone(), action, commit }]).await;
        Ok(name)
    }

    pub async fn delete_wiki_page(&self, repo: &Repo, user: &User, name: &str) -> Result<()> {
        if !self.has_wiki(repo.id) {
            return Err(Error::NotFound);
        }
        crate::issues::ensure_unarchived(&self.db, repo.id).await?;
        let name = name.to_owned();
        let (who_name, who_email, signer) = (user.username.clone(), user.email.clone(), self.signer());
        let deleted = self
            .wiki_git(repo.id, move |g| {
                let who = Identity::new(&who_name, &who_email).signed_by(&*signer);
                for _ in 0..ATTEMPTS {
                    let Some((branch, tip)) = wiki_tip(g)? else { return Ok(None) };
                    let Some(page) = find_page(&pages_at(g, tip)?, &name) else { return Ok(None) };
                    match g.commit_files(&branch, Some(tip), &[(&page.path, None)], &format!("Delete {}", page.title()), &who) {
                        Ok(commit) => return Ok(Some((page.name, commit))),
                        Err(_) if wiki_tip(g)?.map(|(_, t)| t) != Some(tip) => continue,
                        Err(e) => return Err(e),
                    }
                }
                Err(keel_git::Error::Other("the wiki is changing too fast".into()))
            })
            .await?;
        let (name, commit) = deleted.ok_or(Error::NotFound)?;
        self.after_wiki_change(repo, Some(user.id), vec![WikiChange { name, action: WikiAction::Deleted, commit }]).await;
        Ok(())
    }

    /// Maintenance, the activity feed and `gollum` webhooks after the wiki
    /// changed. Failures are logged: the change itself happened.
    pub(crate) async fn after_wiki_change(&self, repo: &Repo, actor: Option<i64>, changes: Vec<WikiChange>) {
        let (path, core, repo_id) = (self.wiki_path(repo.id), self.clone(), repo.id);
        tokio::spawn(async move {
            if let Ok(Err(e)) = tokio::task::spawn_blocking(move || keel_git::run_maintenance(&path)).await {
                tracing::warn!("wiki maintenance failed: {e}");
            }
            if let Err(e) = core.measure_repo(repo_id).await {
                tracing::warn!(repo = repo_id, "measuring disk usage failed: {e}");
            }
        });
        if changes.is_empty() {
            return;
        }
        if let Err(e) = self.record_wiki(repo, actor, &changes).await {
            tracing::warn!("recording wiki changes failed: {e}");
        }
    }

    async fn record_wiki(&self, repo: &Repo, actor: Option<i64>, changes: &[WikiChange]) -> Result<()> {
        let pages: Vec<Value> = changes
            .iter()
            .filter(|c| !c.name.starts_with('_'))
            .map(|c| json!({ "name": c.name, "action": c.action.as_str() }))
            .collect();
        if actor.is_some() && !pages.is_empty() {
            self.add_event(repo.id, actor, "wiki", EventTarget::default(), json!({ "pages": pages })).await?;
        }
        // GitHub's `gollum` event, which knows created and edited pages.
        let url = format!("{}/{}/{}/wiki", self.config.base_url, repo.owner, repo.name);
        let pages: Vec<Value> = changes
            .iter()
            .filter(|c| c.action != WikiAction::Deleted)
            .map(|c| {
                json!({
                    "page_name": c.name,
                    "title": page_title(&c.name),
                    "summary": null,
                    "action": c.action.as_str(),
                    "sha": c.commit.to_string(),
                    "html_url": format!("{url}/{}", form_urlencoded::byte_serialize(c.name.as_bytes()).collect::<String>()),
                })
            })
            .collect();
        if pages.is_empty() {
            return Ok(());
        }
        let payload = json!({
            "pages": pages,
            "repository": self.repo_json(repo),
            "sender": self.sender_json(actor).await?,
        });
        self.enqueue_webhooks(repo.id, "gollum", "", &payload).await
    }

    // -- Git access ---------------------------------------------------------

    /// The repository or wiki a clone URL's `owner/name` refers to: `name`
    /// may end in `.git`, and in `.wiki` for the wiki. Follows renames.
    pub async fn git_target(&self, owner: &str, name: &str) -> Result<Option<GitTarget>> {
        let name = name.strip_suffix(".git").unwrap_or(name);
        if let Some(repo) = self.repo_or_redirect(owner, name).await? {
            return Ok(Some(GitTarget { repo, wiki: false }));
        }
        let Some(base) = name.strip_suffix(".wiki").filter(|b| !b.is_empty()) else { return Ok(None) };
        Ok(self.repo_or_redirect(owner, base).await?.map(|repo| GitTarget { repo, wiki: true }))
    }

    /// Prepares running git for `target`, after the caller checked access.
    /// `None` when reading a wiki that doesn't exist yet; pushing creates it.
    pub async fn git_session(&self, target: &GitTarget, write: bool) -> Result<Option<GitSession>> {
        let id = target.repo.id;
        if write {
            self.check_storage(target.repo.owner_id, 1).await?;
        }
        if !target.wiki {
            let (env, before) = if write {
                (self.receive_pack_env(id).await?, self.ref_snapshot(id).await.ok())
            } else {
                (Vec::new(), None)
            };
            return Ok(Some(GitSession { path: self.repo_path(&target.repo), env, before }));
        }
        if !self.has_wiki(id) {
            if !write {
                return Ok(None);
            }
            self.ensure_wiki(id).await?;
        }
        let before = if write { self.wiki_git(id, |g| g.refs_snapshot()).await.ok() } else { None };
        // No branch protection in wikis; the push size limit applies.
        let env = if write { self.push_limits_env(&[]) } else { Vec::new() };
        Ok(Some(GitSession { path: self.wiki_path(id), env, before }))
    }

    /// After a successful push to `target` by `pusher`.
    pub async fn after_receive(&self, target: &GitTarget, pusher: Option<i64>, before: Option<Refs>) -> Result<()> {
        if !target.wiki {
            return self.after_push(target.repo.id, pusher, before).await;
        }
        let id = target.repo.id;
        let changes = self
            .wiki_git(id, move |g| {
                let Some((branch, tip)) = wiki_tip(g)? else { return Ok(Vec::new()) };
                let after = g.refs_snapshot()?;
                let before = before.unwrap_or_default();
                let change = keel_git::ref_changes(&before, &after).into_iter().find(|c| c.name == format!("refs/heads/{branch}"));
                page_changes(g, change, tip)
            })
            .await?;
        self.after_wiki_change(&target.repo, pusher, changes).await;
        Ok(())
    }

    /// Deletes the wiki of a deleted repository.
    pub(crate) async fn delete_wiki(&self, repo_id: i64) {
        let path = self.wiki_path(repo_id);
        if path.exists()
            && let Ok(Err(e)) = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(path)).await
        {
            tracing::warn!(repo = repo_id, "deleting the wiki: {e}");
        }
    }
}

/// The branch web edits go to and its tip, if it exists.
fn wiki_tip(g: &GitRepo) -> keel_git::Result<Option<(String, Oid)>> {
    let Some(branch) = g.default_branch()? else { return Ok(None) };
    Ok(g.branch_tip(&branch).map(|tip| (branch, tip)))
}

/// Pages a push to the wiki's branch changed.
fn page_changes(g: &GitRepo, change: Option<RefChange>, tip: Oid) -> keel_git::Result<Vec<WikiChange>> {
    let Some(change) = change else { return Ok(Vec::new()) };
    let mut out = Vec::new();
    match change.old {
        None => {
            for f in g.files(tip)? {
                if let Some(name) = name_of(&f.path) {
                    out.push(WikiChange { name: name.to_owned(), action: WikiAction::Created, commit: tip });
                }
            }
        }
        Some(old) => {
            let old_pages = pages_at(g, old)?;
            let diff = g.changed_files(old, tip)?;
            for f in diff.changed {
                if let Some(name) = name_of(&f.path) {
                    let action = if find_page(&old_pages, name).is_some() { WikiAction::Edited } else { WikiAction::Created };
                    out.push(WikiChange { name: name.to_owned(), action, commit: tip });
                }
            }
            for path in diff.deleted {
                if let Some(name) = name_of(&path) {
                    out.push(WikiChange { name: name.to_owned(), action: WikiAction::Deleted, commit: tip });
                }
            }
        }
    }
    // A rename (delete + create of the same name) is an edit.
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out.dedup_by(|b, a| {
        let same = a.name == b.name;
        if same {
            a.action = WikiAction::Edited;
        }
        same
    });
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_titles() {
        assert_eq!(page_name("  Getting   Started ").unwrap(), "Getting-Started");
        assert_eq!(page_title("Getting-Started"), "Getting Started");
        assert_eq!(page_name("C++ tips & tricks").unwrap(), "C++-tips-&-tricks");
        assert_eq!(page_name("_sidebar").unwrap(), "_sidebar");
        for bad in ["", "  ", "a/b", ".hidden", "what?", "x#y", "_pages", "_new", &"a".repeat(101)] {
            assert!(page_name(bad).is_err(), "{bad}");
        }
        assert_eq!(name_of("docs/Setup.md"), Some("Setup"));
        assert_eq!(name_of("Home.markdown"), Some("Home"));
        assert_eq!(name_of("image.png"), None);
        assert_eq!(name_of(".md"), None);
    }

    #[tokio::test]
    async fn editing_pages() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let repo = core.create_repo(&ada, "engine", "", false).await.unwrap();
        assert!(!core.has_wiki(repo.id));
        assert!(core.wiki_pages(repo.id).await.unwrap().is_empty());
        assert!(core.wiki_page(repo.id, "Home", None).await.unwrap().is_none());

        let edit = |page, title, text| WikiEdit { page, title, text, message: "", base: None };
        let home = core.save_wiki_page(&repo, &ada, edit(None, "Home", "Welcome\r\n")).await.unwrap();
        assert_eq!(home, "Home");
        assert!(core.has_wiki(repo.id));
        core.save_wiki_page(&repo, &ada, edit(None, "Getting Started", "Run it")).await.unwrap();
        let err = core.save_wiki_page(&repo, &ada, edit(None, "getting started", "")).await.unwrap_err();
        assert!(matches!(err, Error::Invalid(_)), "names ignore case");

        let page = core.wiki_page(repo.id, "home", None).await.unwrap().unwrap();
        assert_eq!((page.page.path.as_str(), page.text.as_str()), ("Home.md", "Welcome\n"));
        assert_eq!(page.last.as_ref().unwrap().summary, "Create Home");
        let names = |pages: Vec<WikiPage>| pages.into_iter().map(|p| p.name).collect::<Vec<_>>();
        assert_eq!(names(core.wiki_pages(repo.id).await.unwrap()), ["Home", "Getting-Started"]);

        // Conflicts: the page changed since the editor opened it.
        let base = page.commit;
        let at = |base| WikiEdit { page: Some("Home"), title: "Home", text: "Mine", message: "", base: Some(base) };
        core.save_wiki_page(&repo, &ada, edit(Some("Home"), "Home", "Theirs")).await.unwrap();
        assert!(core.save_wiki_page(&repo, &ada, at(base)).await.is_err());
        // Changes to other pages don't conflict.
        let base = core.wiki_page(repo.id, "Home", None).await.unwrap().unwrap().commit;
        core.save_wiki_page(&repo, &ada, edit(Some("Getting-Started"), "Getting Started", "Run it now")).await.unwrap();
        core.save_wiki_page(&repo, &ada, at(base)).await.unwrap();

        // Renaming, history, old revisions, deleting.
        let renamed = edit(Some("Getting-Started"), "Setup", "Run it now");
        assert_eq!(core.save_wiki_page(&repo, &ada, renamed).await.unwrap(), "Setup");
        assert!(core.wiki_page(repo.id, "Getting-Started", None).await.unwrap().is_none());
        let (_, log) = core.wiki_history(repo.id, "Home", 0, 10).await.unwrap().unwrap();
        assert_eq!(log.iter().map(|c| c.summary.as_str()).collect::<Vec<_>>(), ["Update Home", "Update Home", "Create Home"]);
        let old = core.wiki_page(repo.id, "Home", Some(log[2].id)).await.unwrap().unwrap();
        assert_eq!(old.text, "Welcome\n");
        assert!(core.wiki_page(repo.id, "Home", Some(Oid::from_str(&"1".repeat(40)).unwrap())).await.unwrap().is_none());
        core.delete_wiki_page(&repo, &ada, "setup").await.unwrap();
        assert_eq!(names(core.wiki_pages(repo.id).await.unwrap()), ["Home"]);
        assert!(core.delete_wiki_page(&repo, &ada, "setup").await.is_err());

        // The feed has the changes; deleting the repository deletes the wiki.
        let feed = core.feed(&ada, None, 50).await.unwrap();
        assert!(feed.iter().any(|e| e.kind == "wiki" && e.details()["pages"][0]["action"] == "deleted"));
        core.delete_repo(&repo).await.unwrap();
        assert!(!core.wiki_path(repo.id).exists());
    }

    #[tokio::test]
    async fn git_targets() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let repo = core.create_repo(&ada, "engine", "", false).await.unwrap();
        let t = core.git_target("ada", "engine.git").await.unwrap().unwrap();
        assert!(!t.wiki);
        let w = core.git_target("ada", "engine.wiki.git").await.unwrap().unwrap();
        assert!(w.wiki && w.repo.id == repo.id);
        assert!(core.git_target("ada", ".wiki").await.unwrap().is_none());
        assert!(core.git_session(&w, false).await.unwrap().is_none(), "no wiki yet");
        let session = core.git_session(&w, true).await.unwrap().unwrap();
        assert_eq!(session.path, core.wiki_path(repo.id));
        assert!(core.git_session(&w, false).await.unwrap().is_some());
    }
}
