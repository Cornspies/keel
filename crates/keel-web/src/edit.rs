//! Changing files in the browser: editing, creating, uploading and
//! deleting them, committed to the branch or to a new one for a pull
//! request (see [`keel_core::FileEdit`]).

use askama::Template;
use axum::Form;
use axum::extract::{Multipart, Path, State};
use axum::response::{IntoResponse, Redirect};
use keel_core::{Access, FileEdit, FileWrite, Repo, User};
use keel_git::Oid;
use serde::Deserialize;

use crate::error::{AppResult, html};
use crate::render::human_size;
use crate::repo::{RepoView, encode_path, load, with_git};
use crate::session::SignedIn;
use crate::{AppError, AppState, Page};

/// Bigger files aren't edited in the browser.
const MAX_EDIT: usize = 1024 * 1024;
/// Per uploaded file, and for all files of an upload.
pub(crate) const MAX_UPLOAD_FILE: usize = 25 * 1024 * 1024;
pub(crate) const MAX_UPLOAD: usize = 100 * 1024 * 1024;
/// Uploads read at once; more wait their turn. Their files are held in
/// memory until they're committed (up to [`MAX_UPLOAD`] each), which
/// anyone with a repository can ask for.
pub(crate) const CONCURRENT_UPLOADS: usize = 2;
/// How long receiving one upload may take, so that slow ones don't keep
/// their turn (and others waiting) for good.
const UPLOAD_TIME: std::time::Duration = std::time::Duration::from_secs(600);

/// Where a change happens: a branch, its tip (`None` in an empty
/// repository) and a path in it.
struct Spot {
    branch: String,
    tip: Option<Oid>,
    path: String,
    /// Changes must go through a pull request.
    protected: bool,
    /// A free branch name for the changes, like `ada-patch-1`.
    suggested_branch: String,
}

async fn writer(state: &AppState, user: &User, owner: &str, name: &str) -> AppResult<(Repo, Access)> {
    let (repo, access) = load(state, Some(user), owner, name).await?;
    if access < Access::Write {
        return Err(AppError::Forbidden);
    }
    if state.core.is_mirror(repo.id).await? {
        return Err(AppError::BadRequest("This repository is a mirror; change the repository it mirrors.".into()));
    }
    Ok((repo, access))
}

async fn spot(state: &AppState, repo: &Repo, user: &User, rest: String) -> AppResult<Spot> {
    let username = user.username.clone();
    let (branch, tip, path, suggested_branch) = with_git(state, repo, move |g| {
        let (branch, tip, path) = match g.resolve_branch_path(&rest) {
            Some((b, t, p)) => (b, Some(t), p),
            // An empty repository: the first commit goes to HEAD's branch.
            None if !g.has_branches()? => {
                let head = g.head_branch().unwrap_or_else(|| "main".into());
                let rest = rest.trim_matches('/');
                let path = match rest.strip_prefix(head.as_str()) {
                    Some("") => String::new(),
                    Some(p) if p.starts_with('/') => p[1..].to_owned(),
                    _ => return Err(AppError::NotFound),
                };
                (head, None, path)
            }
            None => return Err(AppError::NotFound),
        };
        let suggested = (1..)
            .map(|n| format!("{username}-patch-{n}"))
            .find(|b| g.branch_tip(b).is_none())
            .expect("some number is free");
        Ok((branch, tip, path, suggested))
    })
    .await?;
    Ok(Spot {
        protected: !state.core.may_update_branch(repo.id, &branch, false, false).await?,
        branch,
        tip,
        path,
        suggested_branch,
    })
}

/// The "commit changes" part of the forms.
pub(crate) struct CommitBox {
    pub branch: String,
    /// The default message, shown as the placeholder.
    pub placeholder: String,
    pub summary: String,
    pub description: String,
    /// Direct commits are refused.
    pub protected: bool,
    /// Commit to a new branch (and start a pull request).
    pub to_new_branch: bool,
    pub new_branch: String,
    /// The commit the changes are based on.
    pub base: String,
}

impl CommitBox {
    fn new(s: &Spot, placeholder: String) -> Self {
        Self {
            branch: s.branch.clone(),
            placeholder,
            summary: String::new(),
            description: String::new(),
            protected: s.protected,
            to_new_branch: s.protected,
            new_branch: s.suggested_branch.clone(),
            base: s.tip.map(|t| t.to_string()).unwrap_or_default(),
        }
    }

    /// After a failed attempt: what the person entered.
    fn keep(mut self, f: &CommitFields) -> Self {
        self.summary = f.summary.clone();
        self.description = f.description.clone();
        self.to_new_branch = f.target == "branch" || self.protected;
        if !f.new_branch.trim().is_empty() {
            self.new_branch = f.new_branch.clone();
        }
        self.base = f.base.clone();
        self
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct CommitFields {
    summary: String,
    description: String,
    base: String,
    /// `direct` or `branch`.
    target: String,
    new_branch: String,
}

impl CommitFields {
    fn edit(&self, s: &Spot, default_message: &str, files: Vec<FileWrite>) -> FileEdit {
        let summary = self.summary.trim();
        let summary = if summary.is_empty() { default_message } else { summary };
        let description = self.description.trim();
        FileEdit {
            branch: s.branch.clone(),
            base: Oid::from_str(&self.base).ok(),
            new_branch: (self.target == "branch" || s.protected).then(|| self.new_branch.trim().to_owned()),
            message: if description.is_empty() { summary.to_owned() } else { format!("{summary}\n\n{description}") },
            files,
        }
    }
}

/// Where to go after committing: the result, or starting a pull request.
fn after_commit(repo: &Repo, s: &Spot, committed: &str, page: &str) -> axum::response::Response {
    let base = format!("/{}/{}", repo.owner, repo.name);
    let url = if committed != s.branch {
        format!("{base}/compare/{}...{}", encode_path(&s.branch), encode_path(committed))
    } else {
        format!("{base}/{page}")
    };
    Redirect::to(&url).into_response()
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// `tree/<branch>/<dir>`, to go after the repository's URL.
fn tree_page(branch: &str, dir: &str) -> String {
    let base = format!("tree/{}", encode_path(branch));
    if dir.is_empty() { base } else { format!("{base}/{}", encode_path(dir)) }
}

fn tree_url(repo: &Repo, branch: &str, dir: &str) -> String {
    format!("/{}/{}/{}", repo.owner, repo.name, tree_page(branch, dir))
}

// ---------------------------------------------------------------------------
// Editing and creating
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "repo/edit_file.html")]
struct EditFilePage {
    page: Page,
    repo: RepoView,
    /// Editing an existing file rather than creating one.
    editing: bool,
    action: String,
    cancel_url: String,
    path: String,
    text: String,
    /// The file used Windows line endings, which are kept.
    crlf: bool,
    commit: CommitBox,
    error: Option<String>,
}

pub async fn edit_form(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, rest)): Path<(String, String, String)>,
) -> AppResult {
    let (repo, access) = writer(&state, &user, &owner, &name).await?;
    let s = spot(&state, &repo, &user, rest).await?;
    let (Some(tip), false) = (s.tip, s.path.is_empty()) else { return Err(AppError::NotFound) };
    let path = s.path.clone();
    let blob = with_git(&state, &repo, move |g| Ok(g.blob(tip, &path)?)).await?.ok_or(AppError::NotFound)?;
    let text = match String::from_utf8(blob.data) {
        Ok(t) if !blob.binary && t.len() <= MAX_EDIT && keel_core::parse_pointer(t.as_bytes()).is_none() => t,
        _ => {
            return Err(AppError::BadRequest(format!(
                "{} can't be edited in the browser: it's binary, stored with Git LFS or bigger than {}.",
                s.path,
                human_size(MAX_EDIT)
            )));
        }
    };
    let crlf = text.contains("\r\n");
    let commit = CommitBox::new(&s, format!("Update {}", file_name(&s.path)));
    html(EditFilePage {
        repo: RepoView::new(&state, &repo, access, Some(&user), "code", &s.branch).await?,
        page: Page::new(&state, Some(user), format!("Editing {} · {}/{}", s.path, repo.owner, repo.name)),
        editing: true,
        action: format!("/{}/{}/edit/{}/{}", repo.owner, repo.name, encode_path(&s.branch), encode_path(&s.path)),
        cancel_url: format!("/{}/{}/blob/{}/{}", repo.owner, repo.name, encode_path(&s.branch), encode_path(&s.path)),
        text: if crlf { text.replace("\r\n", "\n") } else { text },
        path: s.path,
        crlf,
        commit,
        error: None,
    })
}

#[derive(Deserialize)]
pub struct FileForm {
    #[serde(default)]
    path: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    crlf: Option<String>,
    #[serde(flatten)]
    commit: CommitFields,
}

impl FileForm {
    /// The text as committed: line endings as in the file before (browsers
    /// send `\r\n`).
    fn content(&self) -> Vec<u8> {
        let text = self.text.replace("\r\n", "\n");
        if self.crlf.is_some() { text.replace('\n', "\r\n").into_bytes() } else { text.into_bytes() }
    }
}

pub async fn save(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, rest)): Path<(String, String, String)>,
    Form(f): Form<FileForm>,
) -> AppResult {
    let (repo, access) = writer(&state, &user, &owner, &name).await?;
    let s = spot(&state, &repo, &user, rest).await?;
    if s.path.is_empty() {
        return Err(AppError::NotFound);
    }
    let new_path = if f.path.trim().is_empty() { s.path.clone() } else { f.path.trim().trim_matches('/').to_owned() };
    let renamed = new_path != s.path;
    let default_message = if renamed {
        format!("Rename {} to {}", s.path, new_path)
    } else {
        format!("Update {}", file_name(&s.path))
    };
    let file = FileWrite { path: new_path.clone(), content: Some(f.content()), from: renamed.then(|| s.path.clone()), new: false };
    let edit = f.commit.edit(&s, &default_message, vec![file]);
    match state.core.commit_file_edit(&repo, &user, edit).await {
        Ok((branch, _)) => {
            let path = keel_core::clean_path(&new_path).unwrap_or(new_path);
            Ok(after_commit(&repo, &s, &branch, &format!("blob/{}/{}", encode_path(&branch), encode_path(&path))))
        }
        Err(keel_core::Error::Invalid(msg)) => {
            let commit = CommitBox::new(&s, format!("Update {}", file_name(&s.path))).keep(&f.commit);
            html(EditFilePage {
                repo: RepoView::new(&state, &repo, access, Some(&user), "code", &s.branch).await?,
                page: Page::new(&state, Some(user), format!("Editing {} · {}/{}", s.path, repo.owner, repo.name)),
                editing: true,
                action: format!("/{}/{}/edit/{}/{}", repo.owner, repo.name, encode_path(&s.branch), encode_path(&s.path)),
                cancel_url: format!("/{}/{}/blob/{}/{}", repo.owner, repo.name, encode_path(&s.branch), encode_path(&s.path)),
                path: f.path,
                text: f.text,
                crlf: f.crlf.is_some(),
                commit,
                error: Some(msg),
            })
        }
        Err(e) => Err(e.into()),
    }
}

/// The folder `s.path` names (it must not be a file).
async fn folder(state: &AppState, repo: &Repo, s: &Spot) -> AppResult<()> {
    let (Some(tip), false) = (s.tip, s.path.is_empty()) else { return Ok(()) };
    let path = s.path.clone();
    let is_file = with_git(state, repo, move |g| Ok(g.blob(tip, &path)?.is_some())).await?;
    if is_file {
        return Err(AppError::NotFound);
    }
    Ok(())
}

pub async fn new_form(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, rest)): Path<(String, String, String)>,
) -> AppResult {
    let (repo, access) = writer(&state, &user, &owner, &name).await?;
    let s = spot(&state, &repo, &user, rest).await?;
    folder(&state, &repo, &s).await?;
    let commit = CommitBox::new(&s, "Create new file".into());
    html(EditFilePage {
        repo: RepoView::new(&state, &repo, access, Some(&user), "code", &s.branch).await?,
        page: Page::new(&state, Some(user), format!("New file · {}/{}", repo.owner, repo.name)),
        editing: false,
        action: format!("/{}/{}/new/{}", repo.owner, repo.name, encode_path(&join(&s.branch, &s.path))),
        cancel_url: tree_url(&repo, &s.branch, &s.path),
        path: if s.path.is_empty() { String::new() } else { format!("{}/", s.path) },
        text: String::new(),
        crlf: false,
        commit,
        error: None,
    })
}

fn join(a: &str, b: &str) -> String {
    if b.is_empty() { a.to_owned() } else { format!("{a}/{b}") }
}

pub async fn create(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, rest)): Path<(String, String, String)>,
    Form(f): Form<FileForm>,
) -> AppResult {
    let (repo, access) = writer(&state, &user, &owner, &name).await?;
    let s = spot(&state, &repo, &user, rest).await?;
    folder(&state, &repo, &s).await?;
    let path = f.path.trim().trim_matches('/').to_owned();
    let file = FileWrite { path: path.clone(), content: Some(f.content()), from: None, new: true };
    let edit = f.commit.edit(&s, &format!("Create {}", file_name(&path)), vec![file]);
    match state.core.commit_file_edit(&repo, &user, edit).await {
        Ok((branch, _)) => {
            let path = keel_core::clean_path(&path).unwrap_or(path);
            Ok(after_commit(&repo, &s, &branch, &format!("blob/{}/{}", encode_path(&branch), encode_path(&path))))
        }
        Err(keel_core::Error::Invalid(msg)) => {
            let commit = CommitBox::new(&s, "Create new file".into()).keep(&f.commit);
            html(EditFilePage {
                repo: RepoView::new(&state, &repo, access, Some(&user), "code", &s.branch).await?,
                page: Page::new(&state, Some(user), format!("New file · {}/{}", repo.owner, repo.name)),
                editing: false,
                action: format!("/{}/{}/new/{}", repo.owner, repo.name, encode_path(&join(&s.branch, &s.path))),
                cancel_url: tree_url(&repo, &s.branch, &s.path),
                path: f.path,
                text: f.text,
                crlf: false,
                commit,
                error: Some(msg),
            })
        }
        Err(e) => Err(e.into()),
    }
}

// ---------------------------------------------------------------------------
// Deleting
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "repo/delete_file.html")]
struct DeleteFilePage {
    page: Page,
    repo: RepoView,
    action: String,
    cancel_url: String,
    path: String,
    commit: CommitBox,
    error: Option<String>,
}

pub async fn delete_form(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, rest)): Path<(String, String, String)>,
) -> AppResult {
    let (repo, access) = writer(&state, &user, &owner, &name).await?;
    let s = spot(&state, &repo, &user, rest).await?;
    let (Some(tip), false) = (s.tip, s.path.is_empty()) else { return Err(AppError::NotFound) };
    let path = s.path.clone();
    if !with_git(&state, &repo, move |g| Ok(g.blob(tip, &path)?.is_some())).await? {
        return Err(AppError::NotFound);
    }
    delete_page(&state, user, &repo, access, &s, CommitBox::new(&s, format!("Delete {}", file_name(&s.path))), None).await
}

async fn delete_page(state: &AppState, user: User, repo: &Repo, access: Access, s: &Spot, commit: CommitBox, error: Option<String>) -> AppResult {
    html(DeleteFilePage {
        repo: RepoView::new(state, repo, access, Some(&user), "code", &s.branch).await?,
        page: Page::new(state, Some(user), format!("Delete {} · {}/{}", s.path, repo.owner, repo.name)),
        action: format!("/{}/{}/delete/{}/{}", repo.owner, repo.name, encode_path(&s.branch), encode_path(&s.path)),
        cancel_url: format!("/{}/{}/blob/{}/{}", repo.owner, repo.name, encode_path(&s.branch), encode_path(&s.path)),
        path: s.path.clone(),
        commit,
        error,
    })
}

pub async fn delete(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, rest)): Path<(String, String, String)>,
    Form(f): Form<CommitFields>,
) -> AppResult {
    let (repo, access) = writer(&state, &user, &owner, &name).await?;
    let s = spot(&state, &repo, &user, rest).await?;
    if s.path.is_empty() {
        return Err(AppError::NotFound);
    }
    let default_message = format!("Delete {}", file_name(&s.path));
    let file = FileWrite { path: s.path.clone(), content: None, from: None, new: false };
    match state.core.commit_file_edit(&repo, &user, f.edit(&s, &default_message, vec![file])).await {
        Ok((branch, _)) => {
            let dir = s.path.rsplit_once('/').map_or("", |(d, _)| d);
            Ok(after_commit(&repo, &s, &branch, &tree_page(&branch, dir)))
        }
        Err(keel_core::Error::Invalid(msg)) => {
            let commit = CommitBox::new(&s, default_message).keep(&f);
            delete_page(&state, user, &repo, access, &s, commit, Some(msg)).await
        }
        Err(e) => Err(e.into()),
    }
}

// ---------------------------------------------------------------------------
// Uploading
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "repo/upload.html")]
struct UploadPage {
    page: Page,
    repo: RepoView,
    action: String,
    cancel_url: String,
    dir: String,
    max_file: String,
    commit: CommitBox,
    error: Option<String>,
}

async fn upload_page(state: &AppState, user: User, repo: &Repo, access: Access, s: &Spot, commit: CommitBox, error: Option<String>) -> AppResult {
    html(UploadPage {
        repo: RepoView::new(state, repo, access, Some(&user), "code", &s.branch).await?,
        page: Page::new(state, Some(user), format!("Upload files · {}/{}", repo.owner, repo.name)),
        action: format!("/{}/{}/upload/{}", repo.owner, repo.name, encode_path(&join(&s.branch, &s.path))),
        cancel_url: tree_url(repo, &s.branch, &s.path),
        dir: s.path.clone(),
        max_file: human_size(MAX_UPLOAD_FILE),
        commit,
        error,
    })
}

pub async fn upload_form(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, rest)): Path<(String, String, String)>,
) -> AppResult {
    let (repo, access) = writer(&state, &user, &owner, &name).await?;
    let s = spot(&state, &repo, &user, rest).await?;
    folder(&state, &repo, &s).await?;
    upload_page(&state, user, &repo, access, &s, CommitBox::new(&s, "Add files via upload".into()), None).await
}

/// What an upload's form held: the commit fields and the files, or why
/// the files can't be taken.
struct Received {
    fields: CommitFields,
    files: Vec<FileWrite>,
    problem: Option<String>,
}

/// Reads an upload's form: the files (for folder `dir`) and the commit
/// fields. Files are read piece by piece: once one is too big (or they're
/// more than the limit, or than the `room` the repository's `owner` has
/// left), the rest is still received, so the browser gets its answer, but
/// nothing of the upload is kept.
async fn receive(form: &mut Multipart, dir: &str, owner: &str, room: Option<u64>) -> AppResult<Received> {
    let mut received = Received { fields: CommitFields::default(), files: Vec::new(), problem: None };
    let mut total = 0;
    let bad = |e: axum::extract::multipart::MultipartError| AppError::BadRequest(format!("The upload failed: {e}"));
    while let Some(mut field) = form.next_field().await.map_err(bad)? {
        let field_name = field.name().unwrap_or_default().to_owned();
        if field_name == "files" {
            let file = field.file_name().unwrap_or_default().to_owned();
            let mut data = Vec::new();
            let mut size = 0;
            while let Some(chunk) = field.chunk().await.map_err(bad)? {
                size += chunk.len();
                total += chunk.len();
                if received.problem.is_none() {
                    if size > MAX_UPLOAD_FILE {
                        received.problem =
                            Some(format!("{file} is bigger than {}; push big files with git (and Git LFS).", human_size(MAX_UPLOAD_FILE)));
                    } else if total > MAX_UPLOAD {
                        received.problem = Some(format!("Uploads can be at most {} in all.", human_size(MAX_UPLOAD)));
                    } else if room.is_some_and(|r| total as u64 > r) {
                        received.problem = Some(format!("{owner} is out of storage on this server."));
                    }
                }
                if received.problem.is_some() {
                    data = Vec::new();
                    received.files.clear();
                } else {
                    data.extend_from_slice(&chunk);
                }
            }
            if !file.is_empty() && received.problem.is_none() {
                received.files.push(FileWrite { path: join(dir, file_name(&file)), content: Some(data), from: None, new: false });
            }
            continue;
        }
        let value = field.text().await.map_err(bad)?;
        let fields = &mut received.fields;
        match field_name.as_str() {
            "summary" => fields.summary = value,
            "description" => fields.description = value,
            "base" => fields.base = value,
            "target" => fields.target = value,
            "new_branch" => fields.new_branch = value,
            _ => {}
        }
    }
    Ok(received)
}

pub async fn upload(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, rest)): Path<(String, String, String)>,
    mut form: Multipart,
) -> AppResult {
    let (repo, access) = writer(&state, &user, &owner, &name).await?;
    let s = spot(&state, &repo, &user, rest).await?;
    folder(&state, &repo, &s).await?;
    // Nothing is read before it's this upload's turn.
    let _turn = state.upload_slots.acquire().await.map_err(|e| AppError::Internal(e.into()))?;
    // What the owner may still store: more than that isn't kept either.
    let room = state.core.storage_room(repo.owner_id).await?;
    let received = match tokio::time::timeout(UPLOAD_TIME, receive(&mut form, &s.path, &repo.owner, room)).await {
        Ok(received) => received?,
        Err(_) => return Err(AppError::BadRequest("The upload took too long; push the files with git instead.".into())),
    };
    let Received { fields, files, mut problem } = received;
    if files.is_empty() && problem.is_none() {
        problem = Some("Choose one or more files to upload.".into());
    }
    let default_message = "Add files via upload";
    let result = match problem {
        Some(p) => Err(keel_core::Error::Invalid(p)),
        None => state.core.commit_file_edit(&repo, &user, fields.edit(&s, default_message, files)).await,
    };
    match result {
        Ok((branch, _)) => {
            Ok(after_commit(&repo, &s, &branch, &tree_page(&branch, &s.path)))
        }
        Err(keel_core::Error::Invalid(msg)) => {
            let commit = CommitBox::new(&s, default_message.into()).keep(&fields);
            upload_page(&state, user, &repo, access, &s, commit, Some(msg)).await
        }
        Err(e) => Err(e.into()),
    }
}
