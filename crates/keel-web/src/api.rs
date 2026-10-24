//! The JSON API under `/api/v1`, for scripts and integrations.
//!
//! It follows GitHub's REST API where that's easy (paths, field names,
//! `page`/`per_page` with `Link` headers, `{"message": …}` errors), so
//! existing clients and habits mostly carry over. Callers authenticate
//! with a personal access token (`Authorization: Bearer <token>`, `token
//! <token>` or Basic with the token as password); read-only tokens can only
//! read. Without a token, public repositories are readable. Cookies are
//! ignored, so there is nothing to forge cross-site.

use axum::extract::{FromRequest, FromRequestParts, OriginalUri, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use base64::Engine;
use keel_core::{
    Access, BranchRule, Issue, IssueFilter, Repo, User, Verdict, json_time, label_json, looks_like_token, rule_for,
};
use keel_git::{CommitDetail, EntryKind, MergeStyle};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::session::basic_auth;
use crate::{AppError, AppState};

/// Files larger than this aren't returned by `contents`.
const MAX_CONTENT: usize = 10 * 1024 * 1024;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1", get(index))
        .route("/api/v1/version", get(version))
        .route("/api/v1/user", get(me))
        .route("/api/v1/user/repos", get(my_repos).post(create_my_repo))
        .route("/api/v1/users/{name}", get(user))
        .route("/api/v1/users/{name}/repos", get(user_repos))
        .route("/api/v1/orgs/{org}/repos", get(user_repos).post(create_org_repo))
        .route("/api/v1/repos/{owner}/{repo}", get(repo).patch(update_repo).delete(delete_repo))
        .route("/api/v1/repos/{owner}/{repo}/branches", get(branches))
        .route("/api/v1/repos/{owner}/{repo}/branches/{*branch}", get(branch))
        .route("/api/v1/repos/{owner}/{repo}/tags", get(tags))
        .route("/api/v1/repos/{owner}/{repo}/commits", get(commits))
        .route("/api/v1/repos/{owner}/{repo}/commits/{sha}", get(commit))
        .route("/api/v1/repos/{owner}/{repo}/commits/{sha}/statuses", get(statuses))
        .route("/api/v1/repos/{owner}/{repo}/commits/{sha}/status", get(combined_status))
        .route("/api/v1/repos/{owner}/{repo}/statuses/{sha}", get(statuses).post(create_status))
        .route("/api/v1/repos/{owner}/{repo}/transfer", post(transfer))
        .route("/api/v1/repos/{owner}/{repo}/contents", get(contents_root))
        .route("/api/v1/repos/{owner}/{repo}/contents/{*path}", get(contents))
        .route("/api/v1/repos/{owner}/{repo}/labels", get(labels))
        .route("/api/v1/repos/{owner}/{repo}/milestones", get(milestones))
        .route("/api/v1/repos/{owner}/{repo}/issues", get(list_issues).post(create_issue))
        .route("/api/v1/repos/{owner}/{repo}/issues/{number}", get(issue).patch(update_issue))
        .route("/api/v1/repos/{owner}/{repo}/issues/{number}/comments", get(comments).post(create_comment))
        .route("/api/v1/repos/{owner}/{repo}/pulls", get(list_pulls).post(create_pull))
        .route("/api/v1/repos/{owner}/{repo}/pulls/{number}", get(pull))
        .route("/api/v1/repos/{owner}/{repo}/pulls/{number}/reviews", post(review))
        .route("/api/v1/repos/{owner}/{repo}/pulls/{number}/merge", put(merge))
        .route("/api/v1/{*rest}", get(not_found).post(not_found).patch(not_found).put(not_found).delete(not_found))
}

// ---------------------------------------------------------------------------
// Errors, authentication, access
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct ApiError(StatusCode, String);

type ApiResult<T = Response> = Result<T, ApiError>;

fn error(status: StatusCode, msg: impl Into<String>) -> ApiError {
    ApiError(status, msg.into())
}

fn not_found_error() -> ApiError {
    error(StatusCode::NOT_FOUND, "Not Found")
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "message": self.1 }))).into_response()
    }
}

impl From<keel_core::Error> for ApiError {
    fn from(e: keel_core::Error) -> Self {
        match e {
            keel_core::Error::NotFound => not_found_error(),
            keel_core::Error::Invalid(msg) => error(StatusCode::UNPROCESSABLE_ENTITY, msg),
            keel_core::Error::Internal(e) => AppError::Internal(e).into(),
        }
    }
}

impl From<AppError> for ApiError {
    fn from(e: AppError) -> Self {
        match e {
            AppError::NotFound => not_found_error(),
            AppError::Forbidden => error(StatusCode::FORBIDDEN, "Forbidden"),
            AppError::BadRequest(msg) => error(StatusCode::UNPROCESSABLE_ENTITY, msg),
            AppError::Internal(e) => {
                tracing::error!("api: {e:#}");
                error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error")
            }
        }
    }
}

impl From<keel_git::Error> for ApiError {
    fn from(e: keel_git::Error) -> Self {
        AppError::from(e).into()
    }
}

/// A JSON request body, with errors as JSON. Like GitHub, any content
/// type is accepted, so `curl -d '{…}'` works without a header.
pub struct Body<T>(T);

impl<T: serde::de::DeserializeOwned, S: Send + Sync> FromRequest<S> for Body<T> {
    type Rejection = ApiError;

    async fn from_request(req: axum::extract::Request, state: &S) -> Result<Self, ApiError> {
        let bytes = axum::body::Bytes::from_request(req, state)
            .await
            .map_err(|e| error(e.status(), e.body_text()))?;
        serde_json::from_slice(&bytes)
            .map(Self)
            .map_err(|e| error(StatusCode::UNPROCESSABLE_ENTITY, format!("Invalid JSON: {e}")))
    }
}

/// `Query`, with errors as JSON.
pub struct Q<T>(T);

impl<T: serde::de::DeserializeOwned, S: Send + Sync> FromRequestParts<S> for Q<T> {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        match Query::<T>::from_request_parts(parts, state).await {
            Ok(Query(v)) => Ok(Self(v)),
            Err(e) => Err(error(StatusCode::BAD_REQUEST, e.body_text())),
        }
    }
}

async fn not_found() -> ApiError {
    not_found_error()
}

/// Who is calling: the token's owner (if any) and whether it may write.
pub struct Caller {
    user: Option<User>,
    can_write: bool,
}

impl FromRequestParts<AppState> for Caller {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let token = match parts.headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
            None => return Ok(Self { user: None, can_write: false }),
            Some(value) => match value.split_once(' ') {
                Some((scheme, t)) if scheme.eq_ignore_ascii_case("bearer") || scheme.eq_ignore_ascii_case("token") => {
                    Some(t.trim().to_owned())
                }
                Some((scheme, _)) if scheme.eq_ignore_ascii_case("basic") => basic_auth(&parts.headers)
                    .map(|(login, secret)| if looks_like_token(&secret) { secret } else { login }),
                _ => None,
            },
        };
        let found = match token {
            Some(t) => state.core.authenticate_token(&t).await?,
            None => None,
        };
        match found {
            Some((user, can_write)) => Ok(Self { user: Some(user), can_write }),
            None => Err(error(
                StatusCode::UNAUTHORIZED,
                "Bad credentials: use a personal access token (passwords don't work here)",
            )),
        }
    }
}

impl Caller {
    fn user(&self) -> ApiResult<&User> {
        self.user.as_ref().ok_or_else(|| error(StatusCode::UNAUTHORIZED, "Requires authentication"))
    }

    /// The user, if their token may make changes.
    fn writer(&self) -> ApiResult<&User> {
        let user = self.user()?;
        if !self.can_write {
            return Err(error(StatusCode::FORBIDDEN, "This access token is read-only"));
        }
        Ok(user)
    }
}

/// A repository the caller can see, and their access to it (read-only
/// tokens get at most read access).
async fn find_repo(state: &AppState, caller: &Caller, owner: &str, name: &str) -> ApiResult<(Repo, Access)> {
    // Renamed and transferred repositories are found under their old names.
    let repo = state.core.repo_or_redirect(owner, name).await?.ok_or_else(not_found_error)?;
    let access = state.core.access(&repo, caller.user.as_ref()).await?;
    if access == Access::None {
        return Err(not_found_error());
    }
    Ok((repo, if caller.can_write { access } else { access.min(Access::Read) }))
}

fn need(caller: &Caller, access: Access, level: Access) -> ApiResult<()> {
    if access >= level {
        return Ok(());
    }
    caller.writer()?;
    Err(error(StatusCode::FORBIDDEN, format!("This needs {} access to the repository", level.role_name())))
}

async fn find_issue(state: &AppState, repo: &Repo, number: i64) -> ApiResult<Issue> {
    state.core.issue(repo.id, number).await?.ok_or_else(not_found_error)
}

// ---------------------------------------------------------------------------
// Pagination
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct Paging {
    #[serde(deserialize_with = "number")]
    page: Option<i64>,
    #[serde(deserialize_with = "number")]
    per_page: Option<i64>,
}

/// A number, also written as a string: query strings only have strings,
/// and `#[serde(flatten)]` loses the hint to parse them.
fn number<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum N {
        Int(i64),
        Text(String),
    }
    match Option::<N>::deserialize(d)? {
        None => Ok(None),
        Some(N::Int(n)) => Ok(Some(n)),
        Some(N::Text(t)) if t.trim().is_empty() => Ok(None),
        Some(N::Text(t)) => t.trim().parse().map(Some).map_err(serde::de::Error::custom),
    }
}

impl Paging {
    fn page(&self) -> i64 {
        self.page.unwrap_or(1).max(1)
    }

    fn per_page(&self) -> i64 {
        self.per_page.unwrap_or(30).clamp(1, 100)
    }

    fn offset(&self) -> i64 {
        (self.page() - 1) * self.per_page()
    }
}

/// A list response. `more`: there's a next page, linked in `Link`.
fn paged(state: &AppState, uri: &axum::http::Uri, paging: &Paging, items: Vec<Value>, more: bool) -> Response {
    let mut res = Json(Value::Array(items)).into_response();
    let link = |page: i64, rel: &str| {
        let mut q = form_urlencoded::Serializer::new(String::new());
        for (k, v) in form_urlencoded::parse(uri.query().unwrap_or("").as_bytes()) {
            if k != "page" {
                q.append_pair(&k, &v);
            }
        }
        q.append_pair("page", &page.to_string());
        format!("<{}{}?{}>; rel=\"{rel}\"", state.core.config.base_url, uri.path(), q.finish())
    };
    let mut links = Vec::new();
    if more {
        links.push(link(paging.page() + 1, "next"));
    }
    if paging.page() > 1 {
        links.push(link(paging.page() - 1, "prev"));
    }
    if !links.is_empty()
        && let Ok(v) = HeaderValue::from_str(&links.join(", "))
    {
        res.headers_mut().insert(header::LINK, v);
    }
    res
}

/// Pages a fully loaded list.
fn page_of<T>(items: Vec<T>, paging: &Paging) -> (Vec<T>, bool) {
    let total = items.len() as i64;
    let start = paging.offset().min(total) as usize;
    let end = (paging.offset() + paging.per_page()).min(total) as usize;
    let more = (end as i64) < total;
    let mut items = items;
    (items.drain(start..end).collect(), more)
}

// ---------------------------------------------------------------------------
// Meta, users
// ---------------------------------------------------------------------------

async fn index(State(state): State<AppState>) -> Json<Value> {
    let base = format!("{}/api/v1", state.core.config.base_url);
    Json(json!({
        "current_user_url": format!("{base}/user"),
        "repository_url": format!("{base}/repos/{{owner}}/{{repo}}"),
        "issues_url": format!("{base}/repos/{{owner}}/{{repo}}/issues"),
        "pulls_url": format!("{base}/repos/{{owner}}/{{repo}}/pulls"),
        "user_url": format!("{base}/users/{{user}}"),
    }))
}

async fn version() -> Json<Value> {
    Json(json!({ "version": env!("CARGO_PKG_VERSION") }))
}

fn user_value(state: &AppState, u: &User) -> Value {
    let mut v = state.core.user_json(u);
    v["site_admin"] = json!(u.is_admin);
    v["created_at"] = json_time(u.created_at);
    v
}

async fn me(State(state): State<AppState>, caller: Caller) -> ApiResult<Json<Value>> {
    let user = caller.user()?;
    let mut v = user_value(&state, user);
    v["email"] = json!(user.email);
    Ok(Json(v))
}

async fn user(State(state): State<AppState>, Path(name): Path<String>) -> ApiResult<Json<Value>> {
    let u = state.core.user_by_name(&name).await?.ok_or_else(not_found_error)?;
    Ok(Json(user_value(&state, &u)))
}

// ---------------------------------------------------------------------------
// Repositories
// ---------------------------------------------------------------------------

async fn repo_value(state: &AppState, r: &Repo) -> ApiResult<Value> {
    let mut v = state.core.repo_json(r);
    let default = crate::repo::with_git(state, r, |g| Ok(g.default_branch()?)).await?;
    v["default_branch"] = json!(default);
    let mirror = state.core.mirror(r.id).await?;
    v["mirror_url"] = json!(mirror.as_ref().map(|m| &m.url));
    v["mirror_synced_at"] = mirror.and_then(|m| m.synced_at).map_or(Value::Null, json_time);
    Ok(v)
}

async fn repo_list(state: &AppState, repos: Vec<Repo>, paging: &Paging, uri: &axum::http::Uri) -> ApiResult {
    let (repos, more) = page_of(repos, paging);
    let mut out = Vec::new();
    for r in &repos {
        out.push(repo_value(state, r).await?);
    }
    Ok(paged(state, uri, paging, out, more))
}

async fn my_repos(
    State(state): State<AppState>,
    caller: Caller,
    Q(paging): Q<Paging>,
    OriginalUri(uri): OriginalUri,
) -> ApiResult {
    let repos = state.core.repos_for(caller.user()?).await?;
    repo_list(&state, repos, &paging, &uri).await
}

async fn user_repos(
    State(state): State<AppState>,
    caller: Caller,
    Path(name): Path<String>,
    Q(paging): Q<Paging>,
    OriginalUri(uri): OriginalUri,
) -> ApiResult {
    let owner = state.core.user_by_name(&name).await?.ok_or_else(not_found_error)?;
    let repos = state.core.repos_of(owner.id, caller.user.as_ref()).await?;
    repo_list(&state, repos, &paging, &uri).await
}

#[derive(Deserialize)]
pub struct NewRepo {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    private: bool,
    /// Mirror the repository at this http(s) URL.
    mirror_url: Option<String>,
    #[serde(default)]
    mirror_username: String,
    #[serde(default)]
    mirror_password: String,
    /// Seconds between syncs: 3600, 14400, 28800 (default) or 86400.
    mirror_interval: Option<i64>,
}

async fn create_repo_in(state: &AppState, user: &User, owner: &User, body: NewRepo) -> ApiResult {
    if !state.core.can_create_repo_in(owner, user).await? {
        return Err(error(StatusCode::FORBIDDEN, format!("You can't create repositories in {}", owner.username)));
    }
    let r = match body.mirror_url.as_deref().filter(|u| !u.trim().is_empty()) {
        Some(url) => {
            let interval = body.mirror_interval.unwrap_or(28_800);
            let (user, pass) = (&body.mirror_username, &body.mirror_password);
            state.core.create_mirror(owner, &body.name, &body.description, body.private, url, user, pass, interval).await?
        }
        None => state.core.create_repo(owner, &body.name, &body.description, body.private).await?,
    };
    Ok((StatusCode::CREATED, Json(repo_value(state, &r).await?)).into_response())
}

async fn create_my_repo(State(state): State<AppState>, caller: Caller, Body(body): Body<NewRepo>) -> ApiResult {
    let user = caller.writer()?;
    create_repo_in(&state, user, user, body).await
}

async fn create_org_repo(
    State(state): State<AppState>,
    caller: Caller,
    Path(org): Path<String>,
    Body(body): Body<NewRepo>,
) -> ApiResult {
    let user = caller.writer()?;
    let org = state.core.user_by_name(&org).await?.filter(User::is_org).ok_or_else(not_found_error)?;
    create_repo_in(&state, user, &org, body).await
}

async fn repo(
    State(state): State<AppState>,
    caller: Caller,
    Path((owner, name)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let (r, access) = find_repo(&state, &caller, &owner, &name).await?;
    let mut v = repo_value(&state, &r).await?;
    v["permissions"] = json!({
        "admin": access >= Access::Admin,
        "push": access >= Access::Write,
        "pull": access >= Access::Read,
    });
    Ok(Json(v))
}

#[derive(Deserialize)]
pub struct RepoUpdate {
    /// A new name (renames it; the old one redirects).
    name: Option<String>,
    description: Option<String>,
    private: Option<bool>,
}

async fn update_repo(
    State(state): State<AppState>,
    caller: Caller,
    Path((owner, name)): Path<(String, String)>,
    Body(body): Body<RepoUpdate>,
) -> ApiResult<Json<Value>> {
    let (r, access) = find_repo(&state, &caller, &owner, &name).await?;
    need(&caller, access, Access::Admin)?;
    let description = body.description.unwrap_or_else(|| r.description.clone());
    state.core.update_repo(r.id, &description, body.private.unwrap_or(r.is_private)).await?;
    if let Some(name) = body.name.as_deref().filter(|n| *n != r.name) {
        state.core.rename_repo(&r, name).await?;
    }
    let r = state.core.repo_by_id(r.id).await?.ok_or_else(not_found_error)?;
    Ok(Json(repo_value(&state, &r).await?))
}

#[derive(Deserialize)]
pub struct Transfer {
    /// You, or an organization you're a member of.
    new_owner: String,
}

async fn transfer(
    State(state): State<AppState>,
    caller: Caller,
    Path((owner, name)): Path<(String, String)>,
    Body(body): Body<Transfer>,
) -> ApiResult {
    let user = caller.writer()?;
    let (r, access) = find_repo(&state, &caller, &owner, &name).await?;
    need(&caller, access, Access::Admin)?;
    let new_owner = state
        .core
        .user_by_name(&body.new_owner)
        .await?
        .ok_or_else(|| error(StatusCode::UNPROCESSABLE_ENTITY, format!("No user or organization named “{}”", body.new_owner)))?;
    let moved = state.core.transfer_repo(&r, user, &new_owner).await?;
    Ok((StatusCode::ACCEPTED, Json(repo_value(&state, &moved).await?)).into_response())
}

async fn delete_repo(
    State(state): State<AppState>,
    caller: Caller,
    Path((owner, name)): Path<(String, String)>,
) -> ApiResult {
    let (r, access) = find_repo(&state, &caller, &owner, &name).await?;
    need(&caller, access, Access::Admin)?;
    state.core.delete_repo(&r).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ---------------------------------------------------------------------------
// Git data
// ---------------------------------------------------------------------------

fn ref_value(repo_url: &str, name: &str, sha: &str, rules: Option<&[BranchRule]>) -> Value {
    let mut v = json!({
        "name": name,
        "commit": { "sha": sha, "url": format!("{repo_url}/commit/{sha}") },
    });
    if let Some(rules) = rules {
        v["protected"] = json!(rule_for(rules, name).is_some());
    }
    v
}

fn repo_url(state: &AppState, r: &Repo) -> String {
    format!("{}/{}/{}", state.core.config.base_url, r.owner, r.name)
}

async fn branches(
    State(state): State<AppState>,
    caller: Caller,
    Path((owner, name)): Path<(String, String)>,
    Q(paging): Q<Paging>,
    OriginalUri(uri): OriginalUri,
) -> ApiResult {
    let (r, _) = find_repo(&state, &caller, &owner, &name).await?;
    let rules = state.core.branch_rules(r.id).await?;
    let list = crate::repo::with_git(&state, &r, |g| Ok(g.branches()?)).await?;
    let url = repo_url(&state, &r);
    let (list, more) = page_of(list, &paging);
    let items = list.iter().map(|b| ref_value(&url, &b.name, &b.commit.id.to_string(), Some(&rules))).collect();
    Ok(paged(&state, &uri, &paging, items, more))
}

async fn branch(
    State(state): State<AppState>,
    caller: Caller,
    Path((owner, name, branch)): Path<(String, String, String)>,
) -> ApiResult<Json<Value>> {
    let (r, _) = find_repo(&state, &caller, &owner, &name).await?;
    let rules = state.core.branch_rules(r.id).await?;
    let b = branch.clone();
    let tip = crate::repo::with_git(&state, &r, move |g| Ok(g.branch_tip(&b))).await?.ok_or_else(not_found_error)?;
    Ok(Json(ref_value(&repo_url(&state, &r), &branch, &tip.to_string(), Some(&rules))))
}

async fn tags(
    State(state): State<AppState>,
    caller: Caller,
    Path((owner, name)): Path<(String, String)>,
    Q(paging): Q<Paging>,
    OriginalUri(uri): OriginalUri,
) -> ApiResult {
    let (r, _) = find_repo(&state, &caller, &owner, &name).await?;
    let list = crate::repo::with_git(&state, &r, |g| Ok(g.tags()?)).await?;
    let url = repo_url(&state, &r);
    let (list, more) = page_of(list, &paging);
    let items = list.iter().map(|t| ref_value(&url, &t.name, &t.commit.id.to_string(), None)).collect();
    Ok(paged(&state, &uri, &paging, items, more))
}

fn commit_value(repo_url: &str, c: &CommitDetail) -> Value {
    let message = if c.body.is_empty() { c.commit.summary.clone() } else { format!("{}\n\n{}", c.commit.summary, c.body) };
    let sha = c.commit.id.to_string();
    json!({
        "sha": sha,
        "html_url": format!("{repo_url}/commit/{sha}"),
        "commit": {
            "message": message,
            "author": { "name": c.commit.author.name, "email": c.commit.author.email, "date": json_time(c.commit.author.time) },
            "committer": { "name": c.committer.name, "email": c.committer.email, "date": json_time(c.committer.time) },
        },
        "parents": c.parents.iter().map(|p| json!({ "sha": p.to_string() })).collect::<Vec<_>>(),
    })
}

#[derive(Deserialize)]
pub struct CommitsQuery {
    /// A branch, tag or commit; the default branch if missing.
    sha: Option<String>,
    #[serde(default)]
    path: String,
    #[serde(flatten)]
    paging: Paging,
}

async fn commits(
    State(state): State<AppState>,
    caller: Caller,
    Path((owner, name)): Path<(String, String)>,
    Q(q): Q<CommitsQuery>,
    OriginalUri(uri): OriginalUri,
) -> ApiResult {
    let (r, _) = find_repo(&state, &caller, &owner, &name).await?;
    let (skip, limit) = (q.paging.offset() as usize, q.paging.per_page() as usize);
    let (rev, path) = (q.sha.clone(), q.path.trim_matches('/').to_owned());
    let list = crate::repo::with_git(&state, &r, move |g| {
        let rev = match rev {
            Some(rev) => rev,
            None => match g.default_branch()? {
                Some(b) => b,
                None => return Ok(None),
            },
        };
        let Some(start) = g.resolve_rev(&rev) else { return Ok(None) };
        let found = g.log(start, &path, skip, limit + 1)?;
        Ok(Some(found.iter().map(|c| g.commit_detail(c.id)).collect::<keel_git::Result<Vec<_>>>()?))
    })
    .await?;
    let Some(mut list) = list else {
        return Err(not_found_error());
    };
    let more = list.len() > limit;
    list.truncate(limit);
    let url = repo_url(&state, &r);
    Ok(paged(&state, &uri, &q.paging, list.iter().map(|c| commit_value(&url, c)).collect(), more))
}

async fn commit(
    State(state): State<AppState>,
    caller: Caller,
    Path((owner, name, sha)): Path<(String, String, String)>,
) -> ApiResult<Json<Value>> {
    let (r, _) = find_repo(&state, &caller, &owner, &name).await?;
    let detail = crate::repo::with_git(&state, &r, move |g| match g.resolve_rev(&sha) {
        Some(id) => Ok(Some(g.commit_detail(id)?)),
        None => Ok(None),
    })
    .await?
    .ok_or_else(not_found_error)?;
    Ok(Json(commit_value(&repo_url(&state, &r), &detail)))
}

/// The full id of the commit `rev` names, if it exists.
async fn commit_id(state: &AppState, r: &Repo, rev: String) -> ApiResult<String> {
    crate::repo::with_git(state, r, move |g| Ok(g.resolve_rev(&rev)))
        .await?
        .map(|id| id.to_string())
        .ok_or_else(|| error(StatusCode::UNPROCESSABLE_ENTITY, "No such commit"))
}

#[derive(Deserialize)]
pub struct NewStatus {
    /// `pending`, `success`, `failure` or `error`.
    state: String,
    #[serde(default)]
    target_url: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    context: Option<String>,
}

async fn create_status(
    State(state): State<AppState>,
    caller: Caller,
    Path((owner, name, sha)): Path<(String, String, String)>,
    Body(body): Body<NewStatus>,
) -> ApiResult {
    let user = caller.writer()?;
    let (r, access) = find_repo(&state, &caller, &owner, &name).await?;
    need(&caller, access, Access::Write)?;
    let sha = commit_id(&state, &r, sha).await?;
    let s = state
        .core
        .set_status(
            &r,
            user,
            &sha,
            &body.state,
            body.context.as_deref().unwrap_or("default"),
            body.description.as_deref().unwrap_or(""),
            body.target_url.as_deref().unwrap_or(""),
        )
        .await?;
    Ok((StatusCode::CREATED, Json(state.core.status_json(&r, &s))).into_response())
}

async fn statuses(
    State(state): State<AppState>,
    caller: Caller,
    Path((owner, name, rev)): Path<(String, String, String)>,
    Q(paging): Q<Paging>,
    OriginalUri(uri): OriginalUri,
) -> ApiResult {
    let (r, _) = find_repo(&state, &caller, &owner, &name).await?;
    let sha = commit_id(&state, &r, rev).await?;
    let mut list = state.core.statuses(r.id, &sha).await?;
    list.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
    let (list, more) = page_of(list, &paging);
    Ok(paged(&state, &uri, &paging, list.iter().map(|s| state.core.status_json(&r, s)).collect(), more))
}

/// One state for the commit, and its statuses.
async fn combined_status(
    State(state): State<AppState>,
    caller: Caller,
    Path((owner, name, rev)): Path<(String, String, String)>,
) -> ApiResult<Json<Value>> {
    let (r, _) = find_repo(&state, &caller, &owner, &name).await?;
    let sha = commit_id(&state, &r, rev).await?;
    let list = state.core.statuses(r.id, &sha).await?;
    Ok(Json(json!({
        // GitHub reports a commit without statuses as pending.
        "state": keel_core::combined_state(&list).unwrap_or("pending"),
        "sha": sha,
        "total_count": list.len(),
        "statuses": list.iter().map(|s| state.core.status_json(&r, s)).collect::<Vec<_>>(),
        "repository": state.core.repo_json(&r),
    })))
}

#[derive(Deserialize)]
pub struct ContentsQuery {
    #[serde(rename = "ref")]
    rev: Option<String>,
}

async fn contents_root(
    state: State<AppState>,
    caller: Caller,
    Path((owner, name)): Path<(String, String)>,
    q: Q<ContentsQuery>,
) -> ApiResult<Json<Value>> {
    contents(state, caller, Path((owner, name, String::new())), q).await
}

/// A file (with its content, base64-encoded) or a directory listing.
async fn contents(
    State(state): State<AppState>,
    caller: Caller,
    Path((owner, name, path)): Path<(String, String, String)>,
    Q(q): Q<ContentsQuery>,
) -> ApiResult<Json<Value>> {
    let (r, _) = find_repo(&state, &caller, &owner, &name).await?;
    let path = path.trim_matches('/').to_owned();
    let p = path.clone();
    let found = crate::repo::with_git(&state, &r, move |g| {
        let rev = match q.rev {
            Some(rev) => rev,
            None => match g.default_branch()? {
                Some(b) => b,
                None => return Ok(None),
            },
        };
        let Some(commit) = g.resolve_rev(&rev) else { return Ok(None) };
        if let Some(entries) = g.tree(commit, &p)? {
            let list = entries
                .into_iter()
                .map(|e| {
                    let size = match e.kind {
                        EntryKind::File | EntryKind::Executable | EntryKind::Symlink => g.blob_size(e.id).ok(),
                        _ => None,
                    };
                    (e, size)
                })
                .collect::<Vec<_>>();
            return Ok(Some(Err(list)));
        }
        Ok(g.blob(commit, &p)?.map(Ok))
    })
    .await?
    .ok_or_else(not_found_error)?;
    let join = |n: &str| if path.is_empty() { n.to_owned() } else { format!("{path}/{n}") };
    match found {
        Ok(blob) => {
            if blob.data.len() > MAX_CONTENT {
                return Err(error(StatusCode::FORBIDDEN, "This file is too large for the API; clone the repository"));
            }
            let file = path.rsplit('/').next().unwrap_or(&path).to_owned();
            Ok(Json(json!({
                "type": "file",
                "name": file,
                "path": path,
                "sha": blob.id.to_string(),
                "size": blob.data.len(),
                "encoding": "base64",
                "content": base64::engine::general_purpose::STANDARD.encode(&blob.data),
            })))
        }
        Err(entries) => Ok(Json(Value::Array(
            entries
                .into_iter()
                .map(|(e, size)| {
                    let kind = match e.kind {
                        EntryKind::Dir => "dir",
                        EntryKind::Symlink => "symlink",
                        EntryKind::Submodule => "submodule",
                        EntryKind::File | EntryKind::Executable => "file",
                    };
                    json!({ "type": kind, "name": e.name, "path": join(&e.name), "sha": e.id.to_string(), "size": size })
                })
                .collect(),
        ))),
    }
}

// ---------------------------------------------------------------------------
// Labels and milestones
// ---------------------------------------------------------------------------

async fn labels(
    State(state): State<AppState>,
    caller: Caller,
    Path((owner, name)): Path<(String, String)>,
    Q(paging): Q<Paging>,
    OriginalUri(uri): OriginalUri,
) -> ApiResult {
    let (r, _) = find_repo(&state, &caller, &owner, &name).await?;
    let (list, more) = page_of(state.core.labels(r.id).await?, &paging);
    Ok(paged(&state, &uri, &paging, list.iter().map(label_json).collect(), more))
}

#[derive(Deserialize)]
pub struct StateQuery {
    state: Option<String>,
    #[serde(flatten)]
    paging: Paging,
}

/// `open` (default), `closed` or `all`.
fn parse_state(s: Option<&str>) -> ApiResult<Option<bool>> {
    match s.unwrap_or("open") {
        "open" => Ok(Some(true)),
        "closed" => Ok(Some(false)),
        "all" => Ok(None),
        other => Err(error(StatusCode::UNPROCESSABLE_ENTITY, format!("Unknown state “{other}”: use open, closed or all"))),
    }
}

async fn milestones(
    State(state): State<AppState>,
    caller: Caller,
    Path((owner, name)): Path<(String, String)>,
    Q(q): Q<StateQuery>,
    OriginalUri(uri): OriginalUri,
) -> ApiResult {
    let (r, _) = find_repo(&state, &caller, &owner, &name).await?;
    let mut list = Vec::new();
    match parse_state(q.state.as_deref())? {
        Some(open) => list.extend(state.core.milestones(r.id, open).await?),
        None => {
            list.extend(state.core.milestones(r.id, true).await?);
            list.extend(state.core.milestones(r.id, false).await?);
        }
    }
    let (list, more) = page_of(list, &q.paging);
    let items = list.iter().map(|m| state.core.milestone_json(m)).collect();
    Ok(paged(&state, &uri, &q.paging, items, more))
}

// ---------------------------------------------------------------------------
// Issues and comments
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct IssuesQuery {
    state: Option<String>,
    /// A label name.
    labels: Option<String>,
    /// A username.
    assignee: Option<String>,
    /// A milestone number (id).
    #[serde(default, deserialize_with = "number")]
    milestone: Option<i64>,
    #[serde(flatten)]
    paging: Paging,
}

async fn issue_list(state: &AppState, caller: &Caller, owner: &str, name: &str, q: IssuesQuery, pulls: bool, uri: &axum::http::Uri) -> ApiResult {
    let (r, _) = find_repo(state, caller, owner, name).await?;
    let open = parse_state(q.state.as_deref())?;
    let label = match q.labels.as_deref().filter(|l| !l.is_empty()) {
        Some(l) => Some(state.core.label_by_name(r.id, l).await?.map_or(-1, |l| l.id)),
        None => None,
    };
    let assignee = match q.assignee.as_deref().filter(|a| !a.is_empty()) {
        Some(a) => Some(state.core.user_by_name(a).await?.map_or(-1, |u| u.id)),
        None => None,
    };
    let filter = IssueFilter { label, assignee, milestone: q.milestone, review_requested: None };
    let (offset, limit) = (q.paging.offset(), q.paging.per_page());
    let mut list = state.core.issues(r.id, pulls, open, filter, offset, limit + 1).await?;
    let more = list.len() as i64 > limit;
    list.truncate(limit as usize);
    let mut items = Vec::new();
    for i in &list {
        items.push(if pulls { state.core.pull_json(&r, i).await? } else { state.core.issue_json(&r, i).await? });
    }
    Ok(paged(state, uri, &q.paging, items, more))
}

async fn list_issues(
    State(state): State<AppState>,
    caller: Caller,
    Path((owner, name)): Path<(String, String)>,
    Q(q): Q<IssuesQuery>,
    OriginalUri(uri): OriginalUri,
) -> ApiResult {
    issue_list(&state, &caller, &owner, &name, q, false, &uri).await
}

async fn issue(
    State(state): State<AppState>,
    caller: Caller,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> ApiResult<Json<Value>> {
    let (r, _) = find_repo(&state, &caller, &owner, &name).await?;
    let i = find_issue(&state, &r, number).await?;
    Ok(Json(state.core.issue_json(&r, &i).await?))
}

/// Labels, assignees and milestone, which only people with write access
/// may set (for others they're ignored, as on GitHub).
#[derive(Deserialize, Default)]
pub struct Triage {
    labels: Option<Vec<String>>,
    assignees: Option<Vec<String>>,
    /// A milestone number, or `null` to remove it.
    #[serde(default, deserialize_with = "double_option")]
    milestone: Option<Option<i64>>,
}

/// Tells a missing field (`None`) from an explicit `null` (`Some(None)`).
fn double_option<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Option<i64>>, D::Error> {
    Option::<i64>::deserialize(d).map(Some)
}

async fn apply_triage(state: &AppState, r: &Repo, issue: &Issue, user: &User, access: Access, t: Triage) -> ApiResult<()> {
    if access < Access::Write {
        return Ok(());
    }
    if let Some(names) = t.labels {
        let all = state.core.labels(r.id).await?;
        let mut ids = Vec::new();
        for n in &names {
            match all.iter().find(|l| l.name.eq_ignore_ascii_case(n)) {
                Some(l) => ids.push(l.id),
                None => return Err(error(StatusCode::UNPROCESSABLE_ENTITY, format!("No label named “{n}”"))),
            }
        }
        state.core.set_issue_labels(issue, user, &ids).await?;
    }
    if let Some(names) = t.assignees {
        let mut ids = Vec::new();
        for n in &names {
            match state.core.user_by_name(n).await? {
                Some(u) => ids.push(u.id),
                None => return Err(error(StatusCode::UNPROCESSABLE_ENTITY, format!("No user named “{n}”"))),
            }
        }
        state.core.set_assignees(r, issue, user, &ids).await?;
    }
    if let Some(m) = t.milestone {
        state.core.set_issue_milestone(issue, user, m).await?;
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct NewIssue {
    title: String,
    #[serde(default)]
    body: String,
    #[serde(flatten)]
    triage: Triage,
}

async fn create_issue(
    State(state): State<AppState>,
    caller: Caller,
    Path((owner, name)): Path<(String, String)>,
    Body(body): Body<NewIssue>,
) -> ApiResult {
    let user = caller.writer()?;
    let (r, access) = find_repo(&state, &caller, &owner, &name).await?;
    let i = state.core.create_issue(r.id, user, &body.title, &body.body).await?;
    apply_triage(&state, &r, &i, user, access, body.triage).await?;
    Ok((StatusCode::CREATED, Json(state.core.issue_json(&r, &i).await?)).into_response())
}

#[derive(Deserialize)]
pub struct IssueUpdate {
    title: Option<String>,
    body: Option<String>,
    /// `open` or `closed`.
    state: Option<String>,
    #[serde(flatten)]
    triage: Triage,
}

async fn update_issue(
    State(state): State<AppState>,
    caller: Caller,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Body(body): Body<IssueUpdate>,
) -> ApiResult<Json<Value>> {
    let user = caller.writer()?;
    let (r, access) = find_repo(&state, &caller, &owner, &name).await?;
    let i = find_issue(&state, &r, number).await?;
    let may_edit = i.is_author(user) || access >= Access::Write;
    if (body.title.is_some() || body.body.is_some() || body.state.is_some()) && !may_edit {
        return Err(error(StatusCode::FORBIDDEN, "Only the author and people with write access can change this"));
    }
    if body.title.is_some() || body.body.is_some() {
        let title = body.title.as_deref().unwrap_or(&i.title);
        let text = body.body.as_deref().unwrap_or(&i.body);
        state.core.edit_issue(&i, user, title, text).await?;
    }
    match body.state.as_deref() {
        None => {}
        Some("open") => state.core.set_issue_open(&i, user, true).await?,
        Some("closed") => state.core.set_issue_open(&i, user, false).await?,
        Some(other) => {
            return Err(error(StatusCode::UNPROCESSABLE_ENTITY, format!("Unknown state “{other}”: use open or closed")));
        }
    }
    apply_triage(&state, &r, &i, user, access, body.triage).await?;
    let i = find_issue(&state, &r, number).await?;
    Ok(Json(if i.is_pull { state.core.pull_json(&r, &i).await? } else { state.core.issue_json(&r, &i).await? }))
}

async fn comments(
    State(state): State<AppState>,
    caller: Caller,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Q(paging): Q<Paging>,
    OriginalUri(uri): OriginalUri,
) -> ApiResult {
    let (r, _) = find_repo(&state, &caller, &owner, &name).await?;
    let i = find_issue(&state, &r, number).await?;
    let all: Vec<_> = state.core.comments(i.id).await?.into_iter().filter(|c| c.kind == "comment").collect();
    let (list, more) = page_of(all, &paging);
    let items = list.iter().map(|c| state.core.comment_json(&r, &i, c)).collect();
    Ok(paged(&state, &uri, &paging, items, more))
}

#[derive(Deserialize)]
pub struct NewComment {
    body: String,
}

async fn create_comment(
    State(state): State<AppState>,
    caller: Caller,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Body(body): Body<NewComment>,
) -> ApiResult {
    let user = caller.writer()?;
    let (r, _) = find_repo(&state, &caller, &owner, &name).await?;
    let i = find_issue(&state, &r, number).await?;
    let id = state.core.add_comment(&i, user, &body.body).await?;
    let c = state.core.comment(i.id, id).await?.ok_or_else(not_found_error)?;
    Ok((StatusCode::CREATED, Json(state.core.comment_json(&r, &i, &c))).into_response())
}

// ---------------------------------------------------------------------------
// Pull requests
// ---------------------------------------------------------------------------

async fn list_pulls(
    State(state): State<AppState>,
    caller: Caller,
    Path((owner, name)): Path<(String, String)>,
    Q(q): Q<IssuesQuery>,
    OriginalUri(uri): OriginalUri,
) -> ApiResult {
    issue_list(&state, &caller, &owner, &name, q, true, &uri).await
}

async fn find_pull(state: &AppState, r: &Repo, number: i64) -> ApiResult<Issue> {
    Some(find_issue(state, r, number).await?).filter(|i| i.is_pull).ok_or_else(not_found_error)
}

async fn pull(
    State(state): State<AppState>,
    caller: Caller,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> ApiResult<Json<Value>> {
    let (r, _) = find_repo(&state, &caller, &owner, &name).await?;
    let i = find_pull(&state, &r, number).await?;
    let mut v = state.core.pull_json(&r, &i).await?;
    if i.is_open()
        && let Some(p) = state.core.pull(i.id).await?
    {
        let head = state.core.pull_tips(&i, &p).await?.1.map(|h| h.to_string());
        let check = state.core.merge_check(&r, &i, &p, head.as_deref()).await?;
        v["approvals"] = json!(check.approvals);
        v["required_approvals"] = json!(check.required());
        v["merge_blocked_by"] = json!(check.blocked());
    }
    Ok(Json(v))
}

#[derive(Deserialize)]
pub struct NewPull {
    title: String,
    #[serde(default)]
    body: String,
    /// The branch to merge: `branch`, `owner:branch` or
    /// `owner/repo:branch` (in a fork).
    head: String,
    /// The branch to merge into.
    base: String,
}

async fn create_pull(
    State(state): State<AppState>,
    caller: Caller,
    Path((owner, name)): Path<(String, String)>,
    Body(body): Body<NewPull>,
) -> ApiResult {
    let user = caller.writer()?;
    let (r, _) = find_repo(&state, &caller, &owner, &name).await?;
    let related = crate::pulls::related_repos(&state, &r, Some(user)).await?;
    let (head_repo, head) = crate::pulls::resolve_head(&related, &body.head)
        .ok_or_else(|| error(StatusCode::UNPROCESSABLE_ENTITY, format!("Unknown head “{}”", body.head)))?;
    let i = state.core.create_pull(&r, user, &body.title, &body.body, &body.base, &head_repo, &head).await?;
    Ok((StatusCode::CREATED, Json(state.core.pull_json(&r, &i).await?)).into_response())
}

#[derive(Deserialize)]
pub struct NewReview {
    /// `APPROVE`, `REQUEST_CHANGES` or `COMMENT`.
    event: String,
    #[serde(default)]
    body: String,
}

async fn review(
    State(state): State<AppState>,
    caller: Caller,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Body(body): Body<NewReview>,
) -> ApiResult {
    let user = caller.writer()?;
    let (r, _) = find_repo(&state, &caller, &owner, &name).await?;
    let i = find_pull(&state, &r, number).await?;
    let id = match body.event.to_ascii_uppercase().as_str() {
        "APPROVE" => state.core.add_review(&i, user, Verdict::Approved, &body.body).await?,
        "REQUEST_CHANGES" => state.core.add_review(&i, user, Verdict::ChangesRequested, &body.body).await?,
        "COMMENT" => state.core.add_comment(&i, user, &body.body).await?,
        other => {
            return Err(error(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("Unknown event “{other}”: use APPROVE, REQUEST_CHANGES or COMMENT"),
            ));
        }
    };
    let c = state.core.comment(i.id, id).await?.ok_or_else(not_found_error)?;
    let state_name = match c.kind.as_str() {
        "approved" => "APPROVED",
        "changes_requested" => "CHANGES_REQUESTED",
        _ => "COMMENTED",
    };
    let mut v = state.core.comment_json(&r, &i, &c);
    v["state"] = json!(state_name);
    Ok((StatusCode::CREATED, Json(v)).into_response())
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct MergeRequest {
    /// `merge` (default) or `squash`.
    merge_method: Option<String>,
    /// Delete the head branch afterwards (if the caller may).
    delete_branch: bool,
}

async fn merge(
    State(state): State<AppState>,
    caller: Caller,
    Path((owner, name, number)): Path<(String, String, i64)>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    let user = caller.writer()?;
    let (r, access) = find_repo(&state, &caller, &owner, &name).await?;
    need(&caller, access, Access::Write)?;
    let i = find_pull(&state, &r, number).await?;
    // The body is optional.
    let body: MergeRequest = if body.iter().all(u8::is_ascii_whitespace) {
        MergeRequest::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| error(StatusCode::UNPROCESSABLE_ENTITY, format!("Invalid JSON: {e}")))?
    };
    let style = match body.merge_method.as_deref().unwrap_or("merge") {
        "merge" => MergeStyle::Merge,
        "squash" => MergeStyle::Squash,
        other => {
            return Err(error(StatusCode::UNPROCESSABLE_ENTITY, format!("Unknown merge_method “{other}”: use merge or squash")));
        }
    };
    match state.core.merge_pull(&r, &i, user, style, body.delete_branch).await {
        Ok(sha) => Ok(Json(json!({ "sha": sha.to_string(), "merged": true, "message": "Pull request merged" }))),
        // GitHub answers 405 for pull requests that can't be merged.
        Err(keel_core::Error::Invalid(msg)) => Err(error(StatusCode::METHOD_NOT_ALLOWED, msg)),
        Err(e) => Err(e.into()),
    }
}
