//! Notifications: an inbox of issues and pull requests with new activity.
//!
//! Someone hears about activity on an issue when they are assigned to it,
//! mentioned in it, wrote it, commented on it, subscribed to it or watch
//! its repository, unless they unsubscribed from it, did it themselves or
//! can no longer read the repository. Each person has at most one
//! notification per issue, which new activity marks unread again.

use std::collections::HashMap;

use crate::{Core, Issue, Result, User, now};

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Notification {
    pub id: i64,
    pub issue_id: i64,
    pub owner: String,
    pub repo: String,
    pub number: i64,
    pub title: String,
    pub is_pull: bool,
    /// The issue's state: open, closed or merged.
    pub state: String,
    /// Why the user is notified: assigned, mentioned, author,
    /// participating, subscribed or watching.
    pub reason: String,
    /// What happened last, e.g. `commented`.
    pub activity: String,
    pub actor: Option<String>,
    pub unread: bool,
    pub updated_at: i64,
}

/// Whether someone follows an issue, and why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Subscription {
    pub subscribed: bool,
    /// `assigned`, `author`, `participating`, `watching`, `subscribed`
    /// (explicitly), `unsubscribed` (explicitly) or `none`.
    pub reason: &'static str,
}

/// Usernames mentioned as `@name` in Markdown, outside code.
pub fn mentions(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut fenced = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        if fenced || line.starts_with("    ") || line.starts_with('\t') {
            continue;
        }
        let mut in_code = false;
        let mut prev: Option<char> = None;
        for (i, c) in line.char_indices() {
            if c == '`' {
                in_code = !in_code;
            } else if c == '@'
                && !in_code
                && !prev.is_some_and(|p| p.is_alphanumeric() || "_.-/`".contains(p))
            {
                let name: String = line[i + 1..]
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || "-_.".contains(*c))
                    .collect();
                let name = name.trim_end_matches('.');
                if !name.is_empty() && !out.iter().any(|n| n.eq_ignore_ascii_case(name)) {
                    out.push(name.to_owned());
                }
            }
            prev = Some(c);
        }
    }
    out
}

impl Core {
    // -- Watching repositories ----------------------------------------------

    pub async fn set_watching(&self, user_id: i64, repo_id: i64, on: bool) -> Result<()> {
        let sql = if on {
            "INSERT OR IGNORE INTO watches (user_id, repo_id, created_at) VALUES (?1, ?2, ?3)"
        } else {
            "DELETE FROM watches WHERE user_id = ?1 AND repo_id = ?2 AND ?3 = ?3"
        };
        sqlx::query(sql).bind(user_id).bind(repo_id).bind(now()).execute(&self.db).await?;
        Ok(())
    }

    pub async fn is_watching(&self, user_id: i64, repo_id: i64) -> Result<bool> {
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM watches WHERE user_id = ?1 AND repo_id = ?2")
            .bind(user_id)
            .bind(repo_id)
            .fetch_one(&self.db)
            .await?;
        Ok(n > 0)
    }

    pub async fn watcher_count(&self, repo_id: i64) -> Result<i64> {
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM watches WHERE repo_id = ?1")
            .bind(repo_id)
            .fetch_one(&self.db)
            .await?;
        Ok(n)
    }

    // -- Subscribing to one issue -------------------------------------------

    pub async fn set_subscription(&self, user_id: i64, issue_id: i64, on: bool) -> Result<()> {
        sqlx::query(
            "INSERT INTO issue_subscriptions (user_id, issue_id, subscribed) VALUES (?1, ?2, ?3)
             ON CONFLICT (user_id, issue_id) DO UPDATE SET subscribed = excluded.subscribed",
        )
        .bind(user_id)
        .bind(issue_id)
        .bind(on)
        .execute(&self.db)
        .await?;
        Ok(())
    }

    pub async fn subscription(&self, user: &User, issue: &Issue) -> Result<Subscription> {
        let (explicit, assigned, commented, watching): (Option<bool>, bool, bool, bool) = sqlx::query_as(
            "SELECT (SELECT subscribed FROM issue_subscriptions WHERE user_id = ?1 AND issue_id = ?2),
                    EXISTS (SELECT 1 FROM issue_assignees WHERE user_id = ?1 AND issue_id = ?2),
                    EXISTS (SELECT 1 FROM comments WHERE author_id = ?1 AND issue_id = ?2
                            AND kind IN ('comment', 'inline', 'approved', 'changes_requested')),
                    EXISTS (SELECT 1 FROM watches WHERE user_id = ?1 AND repo_id = ?3)",
        )
        .bind(user.id)
        .bind(issue.id)
        .bind(issue.repo_id)
        .fetch_one(&self.db)
        .await?;
        let (subscribed, reason) = match explicit {
            Some(true) => (true, "subscribed"),
            Some(false) => (false, "unsubscribed"),
            None if assigned => (true, "assigned"),
            None if issue.is_author(user) => (true, "author"),
            None if commented => (true, "participating"),
            None if watching => (true, "watching"),
            None => (false, "none"),
        };
        Ok(Subscription { subscribed, reason })
    }

    // -- Sending ------------------------------------------------------------

    /// Notifies everyone following `issue` that `actor` did `activity`
    /// (e.g. "commented"), and those mentioned in `text`. With `only`,
    /// notifies just that user (e.g. someone who was assigned).
    pub(crate) async fn notify(
        &self,
        issue: &Issue,
        actor: Option<i64>,
        activity: &str,
        text: &str,
        only: Option<(i64, &'static str)>,
    ) -> Result<()> {
        let Some(repo) = self.repo_by_id(issue.repo_id).await? else {
            return Ok(());
        };
        // Strongest reason first; `or_insert` keeps it.
        let mut recipients: HashMap<i64, &'static str> = HashMap::new();
        if let Some((user_id, reason)) = only {
            recipients.insert(user_id, reason);
        } else {
            let ids = |sql: &'static str| {
                let db = self.db.clone();
                async move {
                    let rows: Vec<(i64,)> = sqlx::query_as(sql).bind(issue.id).bind(issue.repo_id).fetch_all(&db).await?;
                    Ok::<_, crate::Error>(rows.into_iter().map(|(id,)| id))
                }
            };
            for id in ids("SELECT user_id FROM issue_assignees WHERE issue_id = ?1 AND ?2 = ?2").await? {
                recipients.entry(id).or_insert("assigned");
            }
            for name in mentions(text) {
                if let Some(user) = self.user_by_name(&name).await? {
                    recipients.entry(user.id).or_insert("mentioned");
                }
            }
            if let Some(author) = issue.author_id {
                recipients.entry(author).or_insert("author");
            }
            for id in ids(
                "SELECT DISTINCT author_id FROM comments WHERE issue_id = ?1 AND ?2 = ?2 AND author_id IS NOT NULL
                 AND kind IN ('comment', 'inline', 'approved', 'changes_requested')",
            )
            .await?
            {
                recipients.entry(id).or_insert("participating");
            }
            for id in ids("SELECT user_id FROM issue_subscriptions WHERE issue_id = ?1 AND subscribed = 1 AND ?2 = ?2").await? {
                recipients.entry(id).or_insert("subscribed");
            }
            for id in ids("SELECT user_id FROM watches WHERE repo_id = ?2 AND ?1 = ?1").await? {
                recipients.entry(id).or_insert("watching");
            }
            for id in ids("SELECT user_id FROM issue_subscriptions WHERE issue_id = ?1 AND subscribed = 0 AND ?2 = ?2").await? {
                recipients.remove(&id);
            }
        }
        if let Some(actor) = actor {
            recipients.remove(&actor);
        }

        let now = now();
        for (user_id, reason) in recipients {
            let Some(user) = self.user_by_id(user_id).await?.filter(|u| !u.is_org()) else {
                continue;
            };
            if self.access(&repo, Some(&user)).await? == crate::Access::None {
                continue;
            }
            sqlx::query(
                "INSERT INTO notifications (user_id, issue_id, reason, activity, actor_id, unread, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6)
                 ON CONFLICT (user_id, issue_id) DO UPDATE SET reason = excluded.reason,
                    activity = excluded.activity, actor_id = excluded.actor_id, unread = 1,
                    updated_at = excluded.updated_at",
            )
            .bind(user_id)
            .bind(issue.id)
            .bind(reason)
            .bind(activity)
            .bind(actor)
            .bind(now)
            .execute(&self.db)
            .await?;
        }
        Ok(())
    }

    // -- The inbox ----------------------------------------------------------

    /// A user's notifications, most recent first.
    pub async fn notifications(&self, user_id: i64, unread_only: bool, offset: i64, limit: i64) -> Result<Vec<Notification>> {
        Ok(sqlx::query_as(
            "SELECT n.id, n.issue_id, o.username AS owner, r.name AS repo, i.number, i.title, i.is_pull, i.state,
                n.reason, n.activity, a.username AS actor, n.unread, n.updated_at
             FROM notifications n
             JOIN issues i ON i.id = n.issue_id
             JOIN repositories r ON r.id = i.repo_id
             JOIN users o ON o.id = r.owner_id
             LEFT JOIN users a ON a.id = n.actor_id
             WHERE n.user_id = ?1 AND (n.unread = 1 OR NOT ?2)
             ORDER BY n.updated_at DESC, n.id DESC LIMIT ?3 OFFSET ?4",
        )
        .bind(user_id)
        .bind(unread_only)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.db)
        .await?)
    }

    pub async fn unread_notifications(&self, user_id: i64) -> Result<i64> {
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM notifications WHERE user_id = ?1 AND unread = 1")
            .bind(user_id)
            .fetch_one(&self.db)
            .await?;
        Ok(n)
    }

    /// Marks the notification about an issue read (when it is viewed).
    /// Returns whether there was an unread one.
    pub async fn mark_issue_read(&self, user_id: i64, issue_id: i64) -> Result<bool> {
        let done = sqlx::query("UPDATE notifications SET unread = 0 WHERE user_id = ?1 AND issue_id = ?2 AND unread = 1")
            .bind(user_id)
            .bind(issue_id)
            .execute(&self.db)
            .await?;
        Ok(done.rows_affected() > 0)
    }

    pub async fn mark_notification_read(&self, user_id: i64, id: i64) -> Result<()> {
        sqlx::query("UPDATE notifications SET unread = 0 WHERE user_id = ?1 AND id = ?2")
            .bind(user_id)
            .bind(id)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    pub async fn mark_all_read(&self, user_id: i64) -> Result<()> {
        sqlx::query("UPDATE notifications SET unread = 0 WHERE user_id = ?1")
            .bind(user_id)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    /// Forgets read notifications older than `days`.
    pub(crate) async fn prune_notifications(&self, days: i64) -> Result<()> {
        sqlx::query("DELETE FROM notifications WHERE unread = 0 AND updated_at < ?1")
            .bind(now() - days * 86_400)
            .execute(&self.db)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_mentions() {
        assert_eq!(mentions("cc @ada and @Bob."), ["ada", "Bob"]);
        assert_eq!(mentions("mail me@example.com or `@code` @ada @ada"), ["ada"]);
        assert_eq!(mentions("```\n@fenced\n```\n    @indented\n@after"), ["after"]);
    }

    #[tokio::test]
    async fn who_hears_about_what() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let bob = core.register("bob", "bob@example.com", "password1").await.unwrap();
        let eve = core.register("eve", "eve@example.com", "password1").await.unwrap();
        let dan = core.register("dan", "dan@example.com", "password1").await.unwrap();
        let repo = core.create_repo(&ada, "engine", "", false).await.unwrap();
        let secret = core.create_repo(&ada, "secret", "", true).await.unwrap();
        core.set_watching(ada.id, repo.id, true).await.unwrap();
        core.set_watching(ada.id, secret.id, true).await.unwrap();
        assert_eq!(core.watcher_count(repo.id).await.unwrap(), 1);

        // Bob opens an issue mentioning eve: ada (watching) and eve hear.
        let issue = core.create_issue(repo.id, &bob, "Crash", "cc @eve").await.unwrap();
        let inbox = |u: i64| {
            let core = core.clone();
            async move { core.notifications(u, true, 0, 50).await.unwrap() }
        };
        assert_eq!(inbox(ada.id).await[0].reason, "watching");
        assert_eq!(inbox(eve.id).await[0].reason, "mentioned");
        assert!(inbox(bob.id).await.is_empty(), "not about your own activity");
        assert!(inbox(dan.id).await.is_empty());

        // Ada comments: bob (author) and eve hear; dan subscribes.
        core.mark_issue_read(bob.id, issue.id).await.unwrap();
        core.add_comment(&issue, &ada, "Thanks").await.unwrap();
        let n = &inbox(bob.id).await[0];
        assert_eq!((n.reason.as_str(), n.activity.as_str(), n.actor.as_deref()), ("author", "commented", Some("ada")));
        core.set_subscription(dan.id, issue.id, true).await.unwrap();
        core.set_subscription(eve.id, issue.id, false).await.unwrap();
        core.mark_all_read(eve.id).await.unwrap();
        core.add_comment(&issue, &bob, "More").await.unwrap();
        assert_eq!(inbox(dan.id).await[0].reason, "subscribed");
        assert!(inbox(eve.id).await.is_empty(), "unsubscribed");
        assert_eq!(core.unread_notifications(ada.id).await.unwrap(), 1, "one per issue");
        assert_eq!(core.subscription(&dan, &issue).await.unwrap().reason, "subscribed");
        assert_eq!(core.subscription(&ada, &issue).await.unwrap().reason, "participating");
        assert_eq!(core.subscription(&eve, &issue).await.unwrap(), Subscription { subscribed: false, reason: "unsubscribed" });

        // Assigning notifies only the assignee.
        core.mark_all_read(ada.id).await.unwrap();
        core.set_collaborator(&repo, "dan", crate::Access::Write).await.unwrap();
        core.mark_all_read(dan.id).await.unwrap();
        core.set_assignees(&repo, &issue, &bob, &[dan.id]).await.unwrap();
        assert_eq!(inbox(dan.id).await[0].reason, "assigned");
        assert!(inbox(ada.id).await.is_empty());

        // Mentions in private repositories reach only those with access.
        core.create_issue(secret.id, &ada, "Plans", "@eve look").await.unwrap();
        assert!(inbox(eve.id).await.is_empty());

        core.mark_issue_read(dan.id, issue.id).await.unwrap();
        assert_eq!(core.unread_notifications(dan.id).await.unwrap(), 0);
        assert_eq!(core.notifications(dan.id, false, 0, 50).await.unwrap().len(), 1);
    }
}
