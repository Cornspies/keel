//! Things that happen in repositories, fanned out to notifications and
//! webhooks.
//!
//! Webhook payloads follow the shape of GitHub's for the same events (a
//! subset of the fields), so receivers written for GitHub mostly work.

use serde_json::{Value, json};

use keel_git::{GitRepo, Oid, RefChange, Refs};

use crate::events::EventTarget;
use crate::releases::{Release, ReleaseAction};
use crate::{Comment, CommitStatus, Core, Issue, Label, Milestone, PullRequest, Repo, Result, User, pull_ref};

/// Commits listed in a push payload.
const PUSH_COMMITS: usize = 20;

#[derive(Clone, Debug)]
pub enum Activity {
    /// Refs changed. `before` is what they were.
    Push { repo_id: i64, pusher: Option<i64>, before: Refs, changes: Vec<RefChange> },
    Issue { issue_id: i64, actor: Option<i64>, action: IssueAction },
    /// A comment, inline comment or review was added.
    Comment { issue_id: i64, comment_id: i64 },
    /// A commit status was set.
    Status { repo_id: i64, status: CommitStatus, actor: i64 },
    Release { release: Release, actor: i64, action: ReleaseAction },
}

#[derive(Clone, Debug)]
pub enum IssueAction {
    Opened,
    Closed,
    Reopened,
    Merged,
    Edited,
    Labeled(Label),
    Unlabeled(Label),
    Assigned(User),
    Unassigned(User),
    ReviewRequested(User),
    ReviewRequestRemoved(User),
    Milestoned(Milestone),
    Demilestoned(Milestone),
    /// A pull request's head moved.
    Synchronized,
    ReadyForReview,
    ConvertedToDraft,
    AutoMergeEnabled,
    AutoMergeDisabled,
}

/// A Unix time as an RFC 3339 string.
pub fn time(t: i64) -> Value {
    time::OffsetDateTime::from_unix_timestamp(t)
        .ok()
        .and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok())
        .map_or(Value::Null, Value::String)
}

impl Core {
    /// Notifies people and queues webhooks for `activity`. Failures are
    /// logged rather than returned: the action itself already happened.
    pub async fn record(&self, activity: Activity) {
        if let Err(e) = self.try_record(activity).await {
            tracing::warn!("recording activity failed: {e}");
        }
    }

    async fn try_record(&self, activity: Activity) -> Result<()> {
        match activity {
            Activity::Push { repo_id, pusher, before, changes } => self.record_push(repo_id, pusher, before, changes).await,
            Activity::Issue { issue_id, actor, action } => self.record_issue(issue_id, actor, action).await,
            Activity::Comment { issue_id, comment_id } => self.record_comment(issue_id, comment_id).await,
            Activity::Status { repo_id, status, actor } => self.record_status(repo_id, status, actor).await,
            Activity::Release { release, actor, action } => self.record_release(release, actor, action).await,
        }
    }

    async fn issue_by_id(&self, issue_id: i64) -> Result<Option<(Issue, Repo)>> {
        let row: Option<(i64, i64)> = sqlx::query_as("SELECT repo_id, number FROM issues WHERE id = ?1")
            .bind(issue_id)
            .fetch_optional(&self.db)
            .await?;
        let Some((repo_id, number)) = row else { return Ok(None) };
        let (Some(issue), Some(repo)) = (self.issue(repo_id, number).await?, self.repo_by_id(repo_id).await?) else {
            return Ok(None);
        };
        Ok(Some((issue, repo)))
    }

    async fn record_issue(&self, issue_id: i64, actor: Option<i64>, action: IssueAction) -> Result<()> {
        let Some((issue, repo)) = self.issue_by_id(issue_id).await? else { return Ok(()) };
        let notification = match &action {
            IssueAction::Opened => Some(("opened", issue.body.as_str(), None)),
            IssueAction::Closed => Some(("closed", "", None)),
            IssueAction::Reopened => Some(("reopened", "", None)),
            IssueAction::Merged => Some(("merged", "", None)),
            IssueAction::ReadyForReview => Some(("marked ready for review", "", None)),
            IssueAction::Assigned(user) => Some(("assigned you", "", Some((user.id, "assigned")))),
            IssueAction::ReviewRequested(user) => {
                Some(("requested your review", "", Some((user.id, "review_requested"))))
            }
            _ => None,
        };
        if let Some((activity, text, only)) = notification {
            self.notify(&issue, actor, activity, text, only).await?;
        }
        let feed_action = match &action {
            IssueAction::Opened => Some("opened"),
            IssueAction::Closed => Some("closed"),
            IssueAction::Reopened => Some("reopened"),
            IssueAction::Merged => Some("merged"),
            _ => None,
        };
        if let Some(a) = feed_action {
            let target = EventTarget { issue_id: Some(issue.id), ..EventTarget::default() };
            self.add_event(repo.id, actor, "issue", target, json!({ "action": a })).await?;
        }

        let event = if issue.is_pull { "pull_request" } else { "issues" };
        let (name, extra) = match &action {
            IssueAction::Opened => ("opened", None),
            IssueAction::Closed | IssueAction::Merged => ("closed", None),
            IssueAction::Reopened => ("reopened", None),
            IssueAction::Edited => ("edited", None),
            IssueAction::Labeled(l) => ("labeled", Some(("label", label_json(l)))),
            IssueAction::Unlabeled(l) => ("unlabeled", Some(("label", label_json(l)))),
            IssueAction::Assigned(u) => ("assigned", Some(("assignee", self.user_json(u)))),
            IssueAction::Unassigned(u) => ("unassigned", Some(("assignee", self.user_json(u)))),
            IssueAction::ReviewRequested(u) => ("review_requested", Some(("requested_reviewer", self.user_json(u)))),
            IssueAction::ReviewRequestRemoved(u) => {
                ("review_request_removed", Some(("requested_reviewer", self.user_json(u))))
            }
            IssueAction::Milestoned(m) => ("milestoned", Some(("milestone", self.milestone_json(m)))),
            IssueAction::Demilestoned(m) => ("demilestoned", Some(("milestone", self.milestone_json(m)))),
            IssueAction::Synchronized => ("synchronize", None),
            IssueAction::ReadyForReview => ("ready_for_review", None),
            IssueAction::ConvertedToDraft => ("converted_to_draft", None),
            IssueAction::AutoMergeEnabled => ("auto_merge_enabled", None),
            IssueAction::AutoMergeDisabled => ("auto_merge_disabled", None),
        };
        let mut payload = json!({
            "action": name,
            "repository": self.repo_json(&repo),
            "sender": self.sender_json(actor).await?,
        });
        if issue.is_pull {
            payload["number"] = json!(issue.number);
            payload["pull_request"] = self.pull_json(&repo, &issue).await?;
        } else {
            payload["issue"] = self.issue_json(&repo, &issue).await?;
        }
        if let Some((key, value)) = extra {
            payload[key] = value;
        }
        self.enqueue_webhooks(repo.id, event, name, &payload).await
    }

    async fn record_comment(&self, issue_id: i64, comment_id: i64) -> Result<()> {
        let Some((issue, repo)) = self.issue_by_id(issue_id).await? else { return Ok(()) };
        let Some(comment) = self.comment(issue_id, comment_id).await? else { return Ok(()) };
        let activity = match comment.kind.as_str() {
            "approved" => "approved",
            "changes_requested" => "requested changes",
            "inline" => "reviewed",
            _ => "commented",
        };
        self.notify(&issue, comment.author_id, activity, &comment.body, None).await?;
        if comment.kind != "inline" {
            let excerpt: String = comment.body.chars().take(200).collect();
            let target = EventTarget { issue_id: Some(issue.id), ..EventTarget::default() };
            let data = json!({ "kind": comment.kind, "comment_id": comment.id, "excerpt": excerpt });
            self.add_event(repo.id, comment.author_id, "comment", target, data).await?;
        }

        let sender = self.sender_json(comment.author_id).await?;
        let comment_json = self.comment_json(&repo, &issue, &comment);
        let (event, action, payload) = match comment.kind.as_str() {
            "inline" => (
                "pull_request_review_comment",
                "created",
                json!({ "action": "created", "comment": comment_json, "pull_request": self.pull_json(&repo, &issue).await? }),
            ),
            "approved" | "changes_requested" => (
                "pull_request_review",
                "submitted",
                json!({
                    "action": "submitted",
                    "review": {
                        "id": comment.id,
                        "state": comment.kind,
                        "body": comment.body,
                        "user": comment_json["user"],
                        "html_url": comment_json["html_url"],
                        "submitted_at": time(comment.created_at),
                    },
                    "pull_request": self.pull_json(&repo, &issue).await?,
                }),
            ),
            _ => (
                "issue_comment",
                "created",
                json!({ "action": "created", "comment": comment_json, "issue": self.issue_json(&repo, &issue).await? }),
            ),
        };
        let mut payload = payload;
        payload["repository"] = self.repo_json(&repo);
        payload["sender"] = sender;
        self.enqueue_webhooks(repo.id, event, action, &payload).await
    }

    async fn record_status(&self, repo_id: i64, status: CommitStatus, actor: i64) -> Result<()> {
        let Some(repo) = self.repo_by_id(repo_id).await? else { return Ok(()) };
        let wanted = self.hooks_for_repo(repo_id).await?.iter().any(|h| h.active && h.wants("status"));
        if !wanted {
            return Ok(());
        }
        let sha = status.sha.clone();
        // The branches whose tip it is, as GitHub sends.
        let branches: Vec<String> = self
            .git(repo_id, move |g| {
                let id = Oid::from_str(&sha)?;
                Ok(g.branches()?.into_iter().filter(|b| b.commit.id == id).map(|b| b.name).collect())
            })
            .await?;
        let mut payload = self.status_json(&repo, &status);
        payload["name"] = json!(format!("{}/{}", repo.owner, repo.name));
        payload["commit"] = json!({
            "sha": status.sha,
            "html_url": format!("{}/{}/{}/commit/{}", self.config.base_url, repo.owner, repo.name, status.sha),
        });
        payload["branches"] = branches.iter().map(|b| json!({ "name": b, "commit": { "sha": status.sha } })).collect();
        payload["repository"] = self.repo_json(&repo);
        payload["sender"] = self.sender_json(Some(actor)).await?;
        self.enqueue_webhooks(repo_id, "status", "", &payload).await
    }

    async fn record_release(&self, release: Release, actor: i64, action: ReleaseAction) -> Result<()> {
        let Some(repo) = self.repo_by_id(release.repo_id).await? else { return Ok(()) };
        if action == ReleaseAction::Published {
            let target = EventTarget { release_id: Some(release.id), ..EventTarget::default() };
            self.add_event(repo.id, Some(actor), "release", target, json!({})).await?;
        }
        let name = match action {
            ReleaseAction::Published => "published",
            ReleaseAction::Edited => "edited",
            ReleaseAction::Deleted => "deleted",
        };
        let payload = json!({
            "action": name,
            "release": self.release_json(&repo, &release).await?,
            "repository": self.repo_json(&repo),
            "sender": self.sender_json(Some(actor)).await?,
        });
        self.enqueue_webhooks(repo.id, "release", name, &payload).await
    }

    async fn record_push(&self, repo_id: i64, pusher: Option<i64>, before: Refs, changes: Vec<RefChange>) -> Result<()> {
        let Some(repo) = self.repo_by_id(repo_id).await? else { return Ok(()) };
        let wanted = self.hooks_for_repo(repo_id).await?.iter().any(|h| h.active && h.wants("push"));
        // Pushes by people go in the activity feed (not mirror syncs).
        let feed = pusher.is_some();
        if !(wanted || feed) || changes.is_empty() {
            return Ok(());
        }
        let sender = self.sender_json(pusher).await?;
        let pusher_name = sender["login"].clone();
        let repo_url = format!("{}/{}/{}", self.config.base_url, repo.owner, repo.name);
        let path = self.config.repo_path(repo_id);
        let details = tokio::task::spawn_blocking(move || -> keel_git::Result<Vec<_>> {
            let git = GitRepo::open(&path)?;
            changes
                .into_iter()
                .map(|c| {
                    let (commits, total, forced) = git.pushed_commits(&c, &before, PUSH_COMMITS)?;
                    let head = match c.new {
                        Some(new) => git.commit_detail(new).ok(),
                        None => None,
                    };
                    Ok((c, commits, total, forced, head))
                })
                .collect()
        })
        .await
        .map_err(anyhow::Error::from)??;

        if feed {
            for (change, commits, total, _, _) in &details {
                let kind = match (change.old, change.new) {
                    (_, None) => "delete_ref",
                    (None, _) => "create_ref",
                    _ => "push",
                };
                // Publishing a release creates its tag; the release says it.
                if kind == "create_ref"
                    && let Some(tag) = change.name.strip_prefix("refs/tags/")
                    && self.release_by_tag(repo_id, tag).await?.is_some()
                {
                    continue;
                }
                // The newest few commits.
                let shown: Vec<Value> = commits
                    .iter()
                    .rev()
                    .take(3)
                    .map(|c| json!({ "sha": c.commit.id.to_string(), "summary": c.commit.summary }))
                    .collect();
                let target = EventTarget { ref_name: Some(change.name.clone()), ..EventTarget::default() };
                self.add_event(repo_id, pusher, kind, target, json!({ "count": total, "commits": shown })).await?;
            }
        }
        if !wanted {
            return Ok(());
        }

        let zero = Oid::ZERO_SHA1.to_string();
        let commit_json = |c: &keel_git::CommitDetail| {
            let message = if c.body.is_empty() {
                c.commit.summary.clone()
            } else {
                format!("{}\n\n{}", c.commit.summary, c.body)
            };
            json!({
                "id": c.commit.id.to_string(),
                "message": message,
                "timestamp": time(c.commit.author.time),
                "url": format!("{repo_url}/commit/{}", c.commit.id),
                "author": { "name": c.commit.author.name, "email": c.commit.author.email },
                "committer": { "name": c.committer.name, "email": c.committer.email },
            })
        };
        for (change, commits, total, forced, head) in details {
            let short = change
                .name
                .strip_prefix("refs/heads/")
                .or_else(|| change.name.strip_prefix("refs/tags/"))
                .unwrap_or(&change.name);
            let payload = json!({
                "ref": change.name,
                "before": change.old.map_or(zero.clone(), |o| o.to_string()),
                "after": change.new.map_or(zero.clone(), |o| o.to_string()),
                "created": change.old.is_none(),
                "deleted": change.new.is_none(),
                "forced": forced,
                "compare": format!("{repo_url}/commits/{short}"),
                "commits": commits.iter().map(commit_json).collect::<Vec<_>>(),
                "total_commits": total,
                "head_commit": head.as_ref().map(commit_json),
                "pusher": { "name": pusher_name },
                "repository": self.repo_json(&repo),
                "sender": sender,
            });
            self.enqueue_webhooks(repo_id, "push", "", &payload).await?;
        }
        Ok(())
    }

    // -- Payload pieces -----------------------------------------------------
    // GitHub-style JSON, shared by webhook payloads and the API.

    pub fn repo_json(&self, r: &Repo) -> Value {
        let url = format!("{}/{}/{}", self.config.base_url, r.owner, r.name);
        json!({
            "id": r.id,
            "name": r.name,
            "full_name": format!("{}/{}", r.owner, r.name),
            "owner": { "id": r.owner_id, "login": r.owner, "html_url": format!("{}/{}", self.config.base_url, r.owner) },
            "private": r.is_private,
            "fork": r.forked_from.is_some(),
            "is_template": r.is_template,
            "archived": r.is_archived(),
            "description": r.description,
            "topics": r.topics().collect::<Vec<_>>(),
            "stargazers_count": r.stars,
            "html_url": url,
            "clone_url": self.config.http_clone_url(&r.owner, &r.name),
            "ssh_url": self.config.ssh_clone_url(&r.owner, &r.name),
            "created_at": time(r.created_at),
            "pushed_at": time(r.updated_at),
        })
    }

    pub fn user_json(&self, u: &User) -> Value {
        json!({
            "id": u.id,
            "login": u.username,
            "type": if u.is_org() { "Organization" } else { "User" },
            "html_url": format!("{}/{}", self.config.base_url, u.username),
            "avatar_url": format!("{}/avatars/{}?v={}", self.config.base_url, u.username, u.avatar_version),
        })
    }

    pub(crate) async fn sender_json(&self, user_id: Option<i64>) -> Result<Value> {
        Ok(match user_id {
            Some(id) => self.user_by_id(id).await?.map_or(Value::Null, |u| self.user_json(&u)),
            None => Value::Null,
        })
    }

    fn login_json(&self, name: Option<&str>) -> Value {
        match name {
            Some(name) => json!({
                "login": name,
                "html_url": format!("{}/{name}", self.config.base_url),
                "avatar_url": format!("{}/avatars/{name}", self.config.base_url),
            }),
            None => json!({ "login": "ghost" }),
        }
    }

    fn issue_url(&self, repo: &Repo, issue: &Issue) -> String {
        let kind = if issue.is_pull { "pull" } else { "issues" };
        format!("{}/{}/{}/{kind}/{}", self.config.base_url, repo.owner, repo.name, issue.number)
    }

    async fn labels_and_assignees(&self, issue: &Issue) -> Result<(Value, Value)> {
        let labels = self.labels_of(&[issue.id]).await?.remove(&issue.id).unwrap_or_default();
        let assignees = self.assignees_of(&[issue.id]).await?.remove(&issue.id).unwrap_or_default();
        Ok((
            labels.iter().map(label_json).collect(),
            assignees.iter().map(|a| self.login_json(Some(a))).collect(),
        ))
    }

    pub fn status_json(&self, repo: &Repo, s: &CommitStatus) -> Value {
        json!({
            "id": s.id,
            "sha": s.sha,
            "state": s.state,
            "context": s.context,
            "description": s.description,
            "target_url": if s.target_url.is_empty() { Value::Null } else { json!(s.target_url) },
            "creator": self.login_json(s.creator.as_deref()),
            "url": format!("{}/api/v1/repos/{}/{}/statuses/{}", self.config.base_url, repo.owner, repo.name, s.sha),
            "created_at": time(s.created_at),
            "updated_at": time(s.updated_at),
        })
    }

    pub async fn release_json(&self, repo: &Repo, r: &Release) -> Result<Value> {
        let base = format!("{}/{}/{}", self.config.base_url, repo.owner, repo.name);
        let tag_url = form_urlencoded::byte_serialize(r.tag.as_bytes()).collect::<String>().replace("%2F", "/");
        let assets: Vec<Value> = self
            .release_assets(r.id)
            .await?
            .iter()
            .map(|a| {
                let name = form_urlencoded::byte_serialize(a.name.as_bytes()).collect::<String>();
                json!({
                    "id": a.id,
                    "name": a.name,
                    "size": a.size,
                    "content_type": a.content_type,
                    "download_count": a.downloads,
                    "created_at": time(a.created_at),
                    "browser_download_url": format!("{base}/releases/download/{tag_url}/{name}"),
                })
            })
            .collect();
        Ok(json!({
            "id": r.id,
            "tag_name": r.tag,
            "name": r.title,
            "body": r.notes,
            "draft": r.draft,
            "prerelease": r.prerelease,
            // An imported release's author has no account here (as with
            // imported issues): a login would point to whoever has the name.
            "author": self.login_json(r.author.as_deref().filter(|_| r.imported_author.is_none())),
            "created_at": time(r.created_at),
            "published_at": r.published_at.map_or(Value::Null, time),
            "html_url": format!("{base}/releases/tag/{tag_url}"),
            "tarball_url": format!("{base}/archive/{tag_url}.tar.gz"),
            "zipball_url": format!("{base}/archive/{tag_url}.zip"),
            "assets": assets,
        }))
    }

    pub fn milestone_json(&self, m: &Milestone) -> Value {
        json!({
            "id": m.id,
            "number": m.id,
            "title": m.title,
            "description": m.description,
            "state": m.state,
            "open_issues": m.open_items,
            "closed_issues": m.closed_items,
            "due_on": m.due_on.map_or(Value::Null, time),
            "created_at": time(m.created_at),
            "closed_at": m.closed_at.map_or(Value::Null, time),
        })
    }

    pub async fn issue_json(&self, repo: &Repo, issue: &Issue) -> Result<Value> {
        let (labels, assignees) = self.labels_and_assignees(issue).await?;
        let milestone = self.issue_milestone(issue).await?;
        let mut v = json!({
            "id": issue.id,
            "number": issue.number,
            "title": issue.title,
            "body": issue.body,
            "state": if issue.is_open() { "open" } else { "closed" },
            "html_url": self.issue_url(repo, issue),
            "user": self.login_json(issue.author.as_deref()),
            "labels": labels,
            "assignees": assignees,
            "milestone": milestone.as_ref().map_or(Value::Null, |m| self.milestone_json(m)),
            "comments": issue.comment_count,
            "created_at": time(issue.created_at),
            "updated_at": time(issue.updated_at),
            "closed_at": issue.closed_at.map_or(Value::Null, time),
            "reactions": self.reactions_json(issue.id, None).await?,
        });
        if issue.is_pull {
            v["pull_request"] = json!({ "html_url": self.issue_url(repo, issue) });
        }
        Ok(v)
    }

    pub async fn pull_json(&self, repo: &Repo, issue: &Issue) -> Result<Value> {
        let Some(pull) = self.pull(issue.id).await? else {
            return self.issue_json(repo, issue).await;
        };
        let mut v = self.issue_json(repo, issue).await?;
        let (base_ref, number) = (pull.base_ref.clone(), issue.number);
        let (base_sha, head_sha) = self
            .git(repo.id, move |g| Ok((g.branch_tip(&base_ref), g.ref_tip(&pull_ref(number)))))
            .await?;
        let head_repo = match pull.head_repo_id {
            Some(id) if id == repo.id => Some(repo.clone()),
            // A private head's name and description stay out of a public
            // base's payloads.
            Some(id) => self.repo_by_id(id).await?.filter(|h| !h.is_private || repo.is_private),
            None => None,
        };
        let side = |r: Option<&Repo>, branch: &str, sha: Option<Oid>, label: String| {
            json!({
                "label": label,
                "ref": branch,
                "sha": sha.map(|s| s.to_string()),
                "repo": r.map(|r| self.repo_json(r)),
            })
        };
        let PullRequest { merge_commit, merged_at, merged_by, .. } = &pull;
        v["merged"] = json!(issue.state == "merged");
        v["merged_at"] = merged_at.map_or(Value::Null, time);
        v["merged_by"] = merged_by.as_deref().map_or(Value::Null, |m| self.login_json(Some(m)));
        v["merge_commit_sha"] = json!(merge_commit);
        v["draft"] = json!(pull.is_draft);
        v["auto_merge"] = match &pull.auto_merge_by {
            Some(by) => json!({
                "enabled_by": self.login_json(Some(by)),
                "merge_method": pull.auto_merge_style,
            }),
            None => Value::Null,
        };
        v["head"] = side(head_repo.as_ref(), &pull.head_ref, head_sha, pull.head_label(issue));
        let reviewers = self.review_requests(issue.id).await?;
        v["requested_reviewers"] = reviewers.iter().map(|(_, name)| self.login_json(Some(name))).collect();
        v["base"] = side(Some(repo), &pull.base_ref, base_sha, format!("{}:{}", repo.owner, pull.base_ref));
        v.as_object_mut().map(|o| o.remove("pull_request"));
        Ok(v)
    }

    pub fn comment_json(&self, repo: &Repo, issue: &Issue, c: &Comment) -> Value {
        let url = if c.kind == "inline" {
            format!("{}/files#comment-{}", self.issue_url(repo, issue), c.id)
        } else {
            format!("{}#comment-{}", self.issue_url(repo, issue), c.id)
        };
        let mut v = json!({
            "id": c.id,
            "body": c.body,
            "user": self.login_json(c.author.as_deref()),
            "html_url": url,
            "created_at": time(c.created_at),
            "updated_at": time(c.updated_at),
        });
        if c.kind == "inline" {
            v["path"] = json!(c.path);
            v["line"] = json!(c.line);
            v["side"] = json!(if c.side.as_deref() == Some("old") { "LEFT" } else { "RIGHT" });
            v["commit_id"] = json!(c.commit_id);
            v["diff_hunk"] = json!(c.hunk);
            v["in_reply_to_id"] = json!(c.reply_to);
        }
        v
    }
}

pub fn label_json(l: &Label) -> Value {
    json!({ "id": l.id, "name": l.name, "color": l.color, "description": l.description })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn payloads(deliveries: &[crate::Delivery]) -> Vec<(String, String, Value)> {
        let mut out: Vec<_> = deliveries
            .iter()
            .map(|d| (d.event.clone(), d.action.clone(), serde_json::from_str(&d.payload).unwrap()))
            .collect();
        out.reverse();
        out
    }

    #[tokio::test]
    async fn webhook_payloads() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let bob = core.register("bob", "bob@example.com", "password1").await.unwrap();
        let repo = core.create_repo(&ada, "engine", "", false).await.unwrap();
        let all: Vec<String> = crate::WEBHOOK_EVENTS.iter().map(|(e, _)| (*e).to_owned()).collect();
        let hook = core.create_webhook(crate::HookOwner::Repo(repo.id), "https://ci.example.com/x", "", &all, true).await.unwrap();
        assert_eq!(hook.events, "*");

        let issue = core.create_issue(repo.id, &bob, "Crash", "Steps").await.unwrap();
        core.add_comment(&issue, &ada, "Thanks").await.unwrap();
        core.set_issue_open(&issue, &ada, false).await.unwrap();

        // A push creating main with two commits.
        let work = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(work.path())
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_AUTHOR_NAME", "Ada")
                .env("GIT_AUTHOR_EMAIL", "ada@example.com")
                .env("GIT_COMMITTER_NAME", "Ada")
                .env("GIT_COMMITTER_EMAIL", "ada@example.com")
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}");
        };
        git(&["init", "-q", "-b", "main"]);
        for n in ["one", "two"] {
            std::fs::write(work.path().join(n), n).unwrap();
            git(&["add", "-A"]);
            git(&["commit", "-qm", n]);
        }
        let before = core.ref_snapshot(repo.id).await.unwrap();
        git(&["push", "-q", core.repo_path(&repo).to_str().unwrap(), "main"]);
        let after = core.ref_snapshot(repo.id).await.unwrap();
        let changes = keel_git::ref_changes(&before, &after);
        core.record(Activity::Push { repo_id: repo.id, pusher: Some(ada.id), before, changes }).await;

        let got = payloads(&core.deliveries(hook.id, 50).await.unwrap());
        let events: Vec<(&str, &str)> = got.iter().map(|(e, a, _)| (e.as_str(), a.as_str())).collect();
        assert_eq!(events, [
            ("ping", ""),
            ("issues", "opened"),
            ("issue_comment", "created"),
            ("issues", "closed"),
            ("push", ""),
        ]);
        let opened = &got[1].2;
        assert_eq!(opened["issue"]["number"], 1);
        assert_eq!(opened["issue"]["user"]["login"], "bob");
        assert_eq!(opened["repository"]["full_name"], "ada/engine");
        assert_eq!(opened["sender"]["login"], "bob");
        assert_eq!(got[2].2["comment"]["body"], "Thanks");
        assert_eq!(got[3].2["issue"]["state"], "closed");
        let push = &got[4].2;
        assert_eq!(push["ref"], "refs/heads/main");
        assert_eq!(push["created"], true);
        assert_eq!(push["before"], "0000000000000000000000000000000000000000");
        assert_eq!(push["commits"].as_array().unwrap().len(), 2);
        assert_eq!(push["commits"][0]["message"], "one");
        assert_eq!(push["head_commit"]["message"], "two");
        assert_eq!(push["pusher"]["name"], "ada");
        assert!(push["head_commit"]["timestamp"].as_str().unwrap().ends_with('Z'));

        // Inactive hooks get nothing.
        core.update_webhook(&hook, &hook.url, None, &["issues".into()], false).await.unwrap();
        core.create_issue(repo.id, &bob, "Another", "").await.unwrap();
        assert_eq!(core.deliveries(hook.id, 50).await.unwrap().len(), 5);
    }
}
