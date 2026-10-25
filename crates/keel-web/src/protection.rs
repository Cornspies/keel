//! Branch protection settings.

use askama::Template;
use axum::extract::{Path, RawForm, State};
use axum::response::{IntoResponse, Redirect};
use keel_core::{Access, BranchRule, Repo, User};

use crate::error::{AppResult, html};
use crate::labels::form_values;
use crate::repo::RepoView;
use crate::session::SignedIn;
use crate::{AppState, Page};

#[derive(Template)]
#[template(path = "repo/branch_rules.html")]
struct RulesPage {
    page: Page,
    repo: RepoView,
    rules: Vec<BranchRule>,
    /// The new-rule form, refilled after an error.
    draft: BranchRule,
    error: Option<String>,
    /// The rule an error is about (0: the new-rule form).
    error_for: i64,
    base_url: String,
    /// Status contexts reported lately, to choose required checks from.
    contexts: String,
}

async fn admin_repo(state: &AppState, user: &User, owner: &str, name: &str) -> AppResult<Repo> {
    crate::repo::load_for_admin(state, user, owner, name).await
}

fn default_rule() -> BranchRule {
    BranchRule {
        id: 0,
        pattern: String::new(),
        require_pull: true,
        required_approvals: 1,
        dismiss_stale: true,
        allow_force_push: false,
        required_checks: String::new(),
    }
}

async fn rules_page(
    state: &AppState,
    user: User,
    repo: &Repo,
    draft: BranchRule,
    error: Option<(i64, String)>,
) -> AppResult {
    let mut draft = draft;
    if draft.pattern.is_empty() && error.is_none() {
        // Suggest the default branch for the first rule.
        let rules = state.core.branch_rules(repo.id).await?;
        if rules.is_empty() {
            draft.pattern = crate::repo::with_git(state, repo, |g| Ok(g.default_branch()?.unwrap_or_default())).await?;
        }
    }
    let (error_for, error) = match error {
        Some((id, msg)) => (id, Some(msg)),
        None => (0, None),
    };
    html(RulesPage {
        repo: RepoView::new(state, repo, Access::Admin, Some(&user), "settings", "").await?,
        page: Page::new(state, Some(user), format!("Branch protection · {}/{}", repo.owner, repo.name)),
        rules: state.core.branch_rules(repo.id).await?,
        draft,
        error,
        error_for,
        base_url: format!("/{}/{}/settings/branches", repo.owner, repo.name),
        contexts: state.core.recent_contexts(repo.id).await?.join(", "),
    })
}

pub async fn index(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name)): Path<(String, String)>,
) -> AppResult {
    let repo = admin_repo(&state, &user, &owner, &name).await?;
    rules_page(&state, user, &repo, default_rule(), None).await
}

fn rule_from_form(id: i64, body: &[u8]) -> BranchRule {
    let field = |k: &str| form_values(body, k).into_iter().next();
    BranchRule {
        id,
        pattern: field("pattern").unwrap_or_default(),
        require_pull: field("require_pull").is_some(),
        required_approvals: field("required_approvals").and_then(|v| v.trim().parse().ok()).unwrap_or(-1),
        dismiss_stale: field("dismiss_stale").is_some(),
        allow_force_push: field("allow_force_push").is_some(),
        required_checks: field("required_checks").unwrap_or_default(),
    }
}

async fn save(state: AppState, user: User, owner: String, name: String, id: i64, body: &[u8]) -> AppResult {
    let repo = admin_repo(&state, &user, &owner, &name).await?;
    let rule = rule_from_form(id, body);
    match state.core.save_branch_rule(repo.id, &rule).await {
        Ok(()) => Ok(Redirect::to(&format!("/{}/{}/settings/branches", repo.owner, repo.name)).into_response()),
        Err(keel_core::Error::Invalid(msg)) => {
            let draft = if id == 0 { rule } else { default_rule() };
            rules_page(&state, user, &repo, draft, Some((id, msg))).await
        }
        Err(e) => Err(e.into()),
    }
}

pub async fn create(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name)): Path<(String, String)>,
    RawForm(body): RawForm,
) -> AppResult {
    save(state, user, owner, name, 0, &body).await
}

pub async fn update(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, id)): Path<(String, String, i64)>,
    RawForm(body): RawForm,
) -> AppResult {
    save(state, user, owner, name, id, &body).await
}

pub async fn delete(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> AppResult {
    let repo = admin_repo(&state, &user, &owner, &name).await?;
    state.core.delete_branch_rule(repo.id, id).await?;
    Ok(Redirect::to(&format!("/{}/{}/settings/branches", repo.owner, repo.name)).into_response())
}
