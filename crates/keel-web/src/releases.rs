//! Release pages: the list, a release, creating and editing releases, and
//! their files.

use std::sync::atomic::{AtomicU64, Ordering};

use askama::Template;
use axum::Form;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Redirect};
use futures_util::StreamExt;
use keel_core::{Access, Release, Repo, User};
use serde::Deserialize;
use tokio::io::AsyncWriteExt;

use crate::error::{AppResult, html};
use crate::issues::Renderer;
use crate::render::{datetime, human_size, relative_time};
use crate::repo::{RepoView, encode_path, load, with_git};
use crate::session::{SignedIn, Viewer};
use crate::{AppError, AppState, Page};

const PER_PAGE: i64 = 10;

pub(crate) struct AssetView {
    pub id: i64,
    pub name: String,
    pub size: String,
    pub downloads: i64,
    pub url: String,
}

pub(crate) struct ReleaseView {
    pub id: i64,
    pub tag: String,
    pub name: String,
    pub notes_html: String,
    pub draft: bool,
    pub prerelease: bool,
    pub latest: bool,
    pub author: String,
    /// The author's profile: here, or where an imported release came from.
    pub author_url: String,
    pub when: String,
    pub when_full: String,
    pub url: String,
    pub edit_url: String,
    pub zip_url: String,
    pub tgz_url: String,
    pub assets: Vec<AssetView>,
}

fn base(repo: &Repo) -> String {
    format!("/{}/{}", repo.owner, repo.name)
}

async fn view(state: &AppState, repo: &Repo, r: &Release, latest: Option<i64>, renderer: &Renderer) -> AppResult<ReleaseView> {
    let b = base(repo);
    let tag = encode_path(&r.tag);
    let assets = state
        .core
        .release_assets(r.id)
        .await?
        .into_iter()
        .map(|a| AssetView {
            url: format!("{b}/releases/download/{tag}/{}", encode_path(&a.name).replace('/', "%2F")),
            id: a.id,
            name: a.name,
            size: human_size(a.size as usize),
            downloads: a.downloads,
        })
        .collect();
    let when = r.published_at.unwrap_or(r.created_at);
    let who = match &r.imported_author {
        Some(_) => crate::issues::person(&None, &r.imported_author),
        None => crate::issues::person(&r.author, &None),
    };
    Ok(ReleaseView {
        id: r.id,
        name: r.name().to_owned(),
        notes_html: if r.notes.trim().is_empty() { String::new() } else { renderer.render(&r.notes) },
        draft: r.draft,
        prerelease: r.prerelease,
        latest: latest == Some(r.id),
        author: who.name,
        author_url: who.url,
        when: relative_time(when),
        when_full: datetime(when),
        url: format!("{b}/releases/tag/{tag}"),
        edit_url: format!("{b}/releases/{}/edit", r.id),
        zip_url: format!("{b}/archive/{tag}.zip"),
        tgz_url: format!("{b}/archive/{tag}.tar.gz"),
        tag: r.tag.clone(),
        assets,
    })
}

// ---------------------------------------------------------------------------
// List and single release
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "releases/list.html")]
struct ListPage {
    page: Page,
    repo: RepoView,
    releases: Vec<ReleaseView>,
    can_edit: bool,
    newer_url: Option<String>,
    older_url: Option<String>,
    /// Showing one release (its page), rather than the list.
    single: bool,
}

#[derive(Deserialize)]
pub struct PageQuery {
    page: Option<i64>,
}

pub async fn list(
    State(state): State<AppState>,
    Viewer(viewer): Viewer,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<PageQuery>,
) -> AppResult {
    let (repo, access) = load(&state, viewer.as_ref(), &owner, &name).await?;
    let can_edit = access >= Access::Write;
    let page_no = q.page.unwrap_or(1).max(1);
    let mut releases = state.core.releases(repo.id, can_edit, (page_no - 1) * PER_PAGE, PER_PAGE + 1).await?;
    let more = releases.len() as i64 > PER_PAGE;
    releases.truncate(PER_PAGE as usize);
    let latest = state.core.latest_release(repo.id).await?.map(|r| r.id);
    let renderer = Renderer::new(&state, &repo).await?;
    let mut views = Vec::new();
    for r in &releases {
        views.push(view(&state, &repo, r, latest, &renderer).await?);
    }
    let b = base(&repo);
    html(ListPage {
        repo: RepoView::new(&state, &repo, access, viewer.as_ref(), "releases", "").await?,
        page: Page::new(&state, viewer, format!("Releases · {}/{}", repo.owner, repo.name)),
        releases: views,
        can_edit,
        newer_url: (page_no > 1).then(|| format!("{b}/releases?page={}", page_no - 1)),
        older_url: more.then(|| format!("{b}/releases?page={}", page_no + 1)),
        single: false,
    })
}

pub async fn show(
    State(state): State<AppState>,
    Viewer(viewer): Viewer,
    Path((owner, name, tag)): Path<(String, String, String)>,
) -> AppResult {
    let (repo, access) = load(&state, viewer.as_ref(), &owner, &name).await?;
    let can_edit = access >= Access::Write;
    let release = state
        .core
        .release_by_tag(repo.id, &tag)
        .await?
        .filter(|r| can_edit || !r.draft)
        .ok_or(AppError::NotFound)?;
    let latest = state.core.latest_release(repo.id).await?.map(|r| r.id);
    let renderer = Renderer::new(&state, &repo).await?;
    let v = view(&state, &repo, &release, latest, &renderer).await?;
    html(ListPage {
        repo: RepoView::new(&state, &repo, access, viewer.as_ref(), "releases", "").await?,
        page: Page::new(&state, viewer, format!("{} · {}/{}", release.name(), repo.owner, repo.name)),
        releases: vec![v],
        can_edit,
        newer_url: None,
        older_url: None,
        single: true,
    })
}

/// `/releases/download/<tag>/<file>`.
pub async fn download(
    State(state): State<AppState>,
    Viewer(viewer): Viewer,
    Path((owner, name, rest)): Path<(String, String, String)>,
) -> AppResult {
    let (repo, access) = load(&state, viewer.as_ref(), &owner, &name).await?;
    let (tag, file) = rest.rsplit_once('/').ok_or(AppError::NotFound)?;
    let release = state
        .core
        .release_by_tag(repo.id, tag)
        .await?
        .filter(|r| access >= Access::Write || !r.draft)
        .ok_or(AppError::NotFound)?;
    let asset = state.core.release_asset(release.id, file).await?.ok_or(AppError::NotFound)?;
    let f = tokio::fs::File::open(state.core.release_asset_path(asset.id)).await?;
    state.core.count_download(asset.id).await?;
    let mut res = axum::body::Body::from_stream(tokio_util::io::ReaderStream::with_capacity(f, crate::repo::STREAM_CHUNK)).into_response();
    let h = res.headers_mut();
    // Never shown inline: user content mustn't run on our origin.
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(asset.size as u64));
    let disposition = format!("attachment; filename*=UTF-8''{}", encode_path(&asset.name).replace('/', "%2F"));
    if let Ok(v) = HeaderValue::from_str(&disposition) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("sandbox; default-src 'none'"));
    Ok(res)
}

// ---------------------------------------------------------------------------
// Creating and editing
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "releases/form.html")]
struct FormPage {
    page: Page,
    repo: RepoView,
    /// `None` when creating.
    release: Option<ReleaseView>,
    is_draft: bool,
    tags: Vec<String>,
    branches: Vec<String>,
    tag: String,
    target: String,
    title: String,
    notes: String,
    prerelease: bool,
    error: Option<String>,
    max_size: String,
}

async fn writer_repo(state: &AppState, user: &User, owner: &str, name: &str) -> AppResult<(Repo, Access)> {
    let (repo, access) = load(state, Some(user), owner, name).await?;
    if access < Access::Write {
        return Err(AppError::Forbidden);
    }
    Ok((repo, access))
}

#[derive(Default)]
struct Draft {
    tag: String,
    target: String,
    title: String,
    notes: String,
    prerelease: bool,
}

async fn form_page(state: &AppState, user: User, repo: &Repo, release: Option<&Release>, d: Draft, error: Option<String>) -> AppResult {
    let st = state.clone();
    let (tags, branches, default) = with_git(state, repo, move |g| {
        let names = crate::repo::ref_names(&st, g)?;
        Ok((names.tags.clone(), names.branches.clone(), g.default_branch()?.unwrap_or_default()))
    })
    .await?;
    let view = match release {
        Some(r) => {
            let renderer = Renderer::new(state, repo).await?;
            Some(view(state, repo, r, None, &renderer).await?)
        }
        None => None,
    };
    let title = match release {
        Some(r) => format!("Edit {} · {}/{}", r.name(), repo.owner, repo.name),
        None => format!("New release · {}/{}", repo.owner, repo.name),
    };
    html(FormPage {
        repo: RepoView::new(state, repo, Access::Write, Some(&user), "releases", "").await?,
        page: Page::new(state, Some(user), title),
        is_draft: release.is_some_and(|r| r.draft),
        release: view,
        tags,
        branches,
        target: if d.target.is_empty() { default } else { d.target },
        tag: d.tag,
        title: d.title,
        notes: d.notes,
        prerelease: d.prerelease,
        error,
        max_size: human_size(state.core.config.lfs_max_bytes as usize),
    })
}

pub async fn new_form(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name)): Path<(String, String)>,
) -> AppResult {
    let (repo, _) = writer_repo(&state, &user, &owner, &name).await?;
    form_page(&state, user, &repo, None, Draft::default(), None).await
}

#[derive(Deserialize)]
pub struct ReleaseForm {
    #[serde(default)]
    tag: String,
    #[serde(default)]
    target: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    notes: String,
    prerelease: Option<String>,
    /// `publish`, or `draft` (save without publishing).
    #[serde(default)]
    action: String,
}

pub async fn create(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name)): Path<(String, String)>,
    Form(f): Form<ReleaseForm>,
) -> AppResult {
    let (repo, _) = writer_repo(&state, &user, &owner, &name).await?;
    let draft = f.action == "draft";
    let result = state
        .core
        .create_release(&repo, &user, &f.tag, &f.target, &f.title, &f.notes, draft, f.prerelease.is_some())
        .await;
    match result {
        Ok(r) if r.draft => Ok(Redirect::to(&format!("{}/releases/{}/edit", base(&repo), r.id)).into_response()),
        Ok(r) => Ok(Redirect::to(&format!("{}/releases/tag/{}", base(&repo), encode_path(&r.tag))).into_response()),
        Err(keel_core::Error::Invalid(msg)) => {
            let d = Draft { tag: f.tag, target: f.target, title: f.title, notes: f.notes, prerelease: f.prerelease.is_some() };
            form_page(&state, user, &repo, None, d, Some(msg)).await
        }
        Err(e) => Err(e.into()),
    }
}

async fn writable_release(state: &AppState, user: &User, owner: &str, name: &str, id: i64) -> AppResult<(Repo, Release)> {
    let (repo, _) = writer_repo(state, user, owner, name).await?;
    let release = state.core.release(repo.id, id).await?.ok_or(AppError::NotFound)?;
    Ok((repo, release))
}

pub async fn edit_form(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> AppResult {
    let (repo, release) = writable_release(&state, &user, &owner, &name, id).await?;
    let d = Draft {
        tag: release.tag.clone(),
        target: release.target.clone(),
        title: release.title.clone(),
        notes: release.notes.clone(),
        prerelease: release.prerelease,
    };
    form_page(&state, user, &repo, Some(&release), d, None).await
}

pub async fn update(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, id)): Path<(String, String, i64)>,
    Form(f): Form<ReleaseForm>,
) -> AppResult {
    let (repo, release) = writable_release(&state, &user, &owner, &name, id).await?;
    let result = async {
        let edited = state.core.edit_release(&release, &user, &f.title, &f.notes, f.prerelease.is_some()).await?;
        if f.action == "publish" {
            return state.core.publish_release(&repo, &edited, &user).await;
        }
        Ok(edited)
    }
    .await;
    match result {
        Ok(r) if r.draft => Ok(Redirect::to(&format!("{}/releases/{}/edit", base(&repo), r.id)).into_response()),
        Ok(r) => Ok(Redirect::to(&format!("{}/releases/tag/{}", base(&repo), encode_path(&r.tag))).into_response()),
        Err(keel_core::Error::Invalid(msg)) => {
            let d = Draft { tag: release.tag.clone(), target: release.target.clone(), title: f.title, notes: f.notes, prerelease: f.prerelease.is_some() };
            form_page(&state, user, &repo, Some(&release), d, Some(msg)).await
        }
        Err(e) => Err(e.into()),
    }
}

pub async fn delete(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> AppResult {
    let (repo, release) = writable_release(&state, &user, &owner, &name, id).await?;
    state.core.delete_release(&release, &user).await?;
    Ok(Redirect::to(&format!("{}/releases", base(&repo))).into_response())
}

/// Streams an upload to a temporary file next to the release files,
/// refusing it past the size limit. Returns its path and size.
pub(crate) async fn receive_upload<S, E>(state: &AppState, room: Option<u64>, mut chunks: S) -> AppResult<Result<(std::path::PathBuf, u64), String>>
where
    S: futures_util::Stream<Item = Result<axum::body::Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = state.core.config.data_dir.join("releases");
    tokio::fs::create_dir_all(&dir).await?;
    let tmp = dir.join(format!("upload-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    let max = state.core.config.lfs_max_bytes;
    let mut file = tokio::fs::File::create(&tmp).await?;
    let mut size = 0u64;
    while let Some(chunk) = chunks.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp).await;
                return Ok(Err(format!("The upload failed: {e}")));
            }
        };
        size += chunk.len() as u64;
        if size > max {
            drop(file);
            let _ = tokio::fs::remove_file(&tmp).await;
            return Ok(Err(format!("Files can be at most {}.", human_size(max as usize))));
        }
        // Stop as soon as it can't fit, rather than after receiving it all.
        if room.is_some_and(|r| size > r) {
            drop(file);
            let _ = tokio::fs::remove_file(&tmp).await;
            return Ok(Err("The repository's owner is out of storage on this server.".into()));
        }
        file.write_all(&chunk).await?;
    }
    file.sync_all().await?;
    Ok(Ok((tmp, size)))
}

/// Attaches the uploaded files (`file` fields of a multipart form).
pub async fn upload(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, id)): Path<(String, String, i64)>,
    mut multipart: Multipart,
) -> AppResult {
    let (repo, release) = writable_release(&state, &user, &owner, &name, id).await?;
    let mut error = None;
    while let Some(field) = multipart.next_field().await.map_err(|e| AppError::BadRequest(e.body_text()))? {
        if field.name() != Some("file") {
            continue;
        }
        let file_name = field.file_name().unwrap_or_default().to_owned();
        if file_name.is_empty() {
            continue; // no file chosen
        }
        let content_type = field.content_type().unwrap_or("application/octet-stream").to_owned();
        let stream = field.map(|c| c.map_err(|e| e.body_text()));
        let room = state.core.storage_room(repo.owner_id).await?;
        let (tmp, size) = match receive_upload(&state, room, Box::pin(stream)).await? {
            Ok(v) => v,
            Err(msg) => {
                error = Some(msg);
                break;
            }
        };
        match state.core.add_release_asset(&release, &user, &file_name, &content_type, &tmp, size).await {
            Ok(_) => {}
            Err(keel_core::Error::Invalid(msg)) => {
                let _ = tokio::fs::remove_file(&tmp).await;
                error = Some(msg);
                break;
            }
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp).await;
                return Err(e.into());
            }
        }
    }
    match error {
        None => Ok(Redirect::to(&format!("{}/releases/{}/edit", base(&repo), release.id)).into_response()),
        Some(msg) => {
            let d = Draft {
                tag: release.tag.clone(),
                target: release.target.clone(),
                title: release.title.clone(),
                notes: release.notes.clone(),
                prerelease: release.prerelease,
            };
            form_page(&state, user, &repo, Some(&release), d, Some(msg)).await
        }
    }
}

pub async fn delete_asset(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, id, asset)): Path<(String, String, i64, i64)>,
) -> AppResult {
    let (repo, release) = writable_release(&state, &user, &owner, &name, id).await?;
    state.core.delete_release_asset(&release, asset).await?;
    Ok(Redirect::to(&format!("{}/releases/{}/edit", base(&repo), release.id)).into_response())
}

