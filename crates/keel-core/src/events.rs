//! The activity feed: what happened in repositories, for the home page
//! (repositories you watch, and what you did) and profiles (what someone
//! did). Only repositories the viewer can read are included.

use serde_json::Value;

use crate::{Core, Result, User, now};

/// Events older than this are deleted at startup.
const KEEP_DAYS: i64 = 365;

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Event {
    pub id: i64,
    pub owner: String,
    pub repo: String,
    pub actor: Option<String>,
    /// `push`, `create_ref`, `delete_ref`, `issue`, `comment`, `release`,
    /// `repo_created` or `fork`.
    pub kind: String,
    pub ref_name: Option<String>,
    pub issue_number: Option<i64>,
    pub issue_title: Option<String>,
    pub is_pull: Option<bool>,
    pub release_tag: Option<String>,
    pub release_title: Option<String>,
    /// Details as JSON, see [`details`](Self::details).
    pub data: String,
    pub created_at: i64,
}

impl Event {
    /// Details, e.g. `{"action": "opened"}` or a push's commits.
    pub fn details(&self) -> Value {
        serde_json::from_str(&self.data).unwrap_or(Value::Null)
    }
}

macro_rules! event_select {
    () => {
        "SELECT e.id, o.username AS owner, r.name AS repo, a.username AS actor, e.kind, e.ref_name,
            i.number AS issue_number, i.title AS issue_title, i.is_pull, rl.tag AS release_tag,
            rl.title AS release_title, e.data, e.created_at
         FROM events e
         JOIN repositories r ON r.id = e.repo_id
         JOIN users o ON o.id = r.owner_id
         LEFT JOIN users a ON a.id = e.actor_id
         LEFT JOIN issues i ON i.id = e.issue_id
         LEFT JOIN releases rl ON rl.id = e.release_id "
    };
}

/// SQL condition: repository `r` is readable by user `?1` (-1: nobody
/// signed in), who is a site admin if `?2`.
macro_rules! readable {
    () => {
        "(r.is_private = 0 OR ?2 OR r.owner_id = ?1
          OR EXISTS (SELECT 1 FROM collaborators c WHERE c.repo_id = r.id AND c.user_id = ?1)
          OR EXISTS (SELECT 1 FROM org_members m WHERE m.org_id = r.owner_id AND m.user_id = ?1))"
    };
}

/// What to attach an event to.
#[derive(Default)]
pub(crate) struct EventTarget {
    pub ref_name: Option<String>,
    pub issue_id: Option<i64>,
    pub release_id: Option<i64>,
}

impl Core {
    pub(crate) async fn add_event(&self, repo_id: i64, actor: Option<i64>, kind: &str, target: EventTarget, data: Value) -> Result<()> {
        sqlx::query(
            "INSERT INTO events (repo_id, actor_id, kind, ref_name, issue_id, release_id, data, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )
        .bind(repo_id)
        .bind(actor)
        .bind(kind)
        .bind(target.ref_name)
        .bind(target.issue_id)
        .bind(target.release_id)
        .bind(data.to_string())
        .bind(now())
        .execute(&self.db)
        .await?;
        Ok(())
    }

    /// The home page feed: events in repositories `viewer` watches, and
    /// their own, newest first, older than event `before` (for paging).
    pub async fn feed(&self, viewer: &User, before: Option<i64>, limit: i64) -> Result<Vec<Event>> {
        Ok(sqlx::query_as(concat!(
            event_select!(),
            "WHERE (e.repo_id IN (SELECT repo_id FROM watches WHERE user_id = ?1) OR e.actor_id = ?1)
               AND e.id < ?3 AND ",
            readable!(),
            " ORDER BY e.id DESC LIMIT ?4"
        ))
        .bind(viewer.id)
        .bind(viewer.is_admin)
        .bind(before.unwrap_or(i64::MAX))
        .bind(limit)
        .fetch_all(&self.db)
        .await?)
    }

    /// What `actor` did, in repositories `viewer` can read.
    pub async fn user_events(&self, actor: i64, viewer: Option<&User>, before: Option<i64>, limit: i64) -> Result<Vec<Event>> {
        Ok(sqlx::query_as(concat!(
            event_select!(),
            "WHERE e.actor_id = ?5 AND e.id < ?3 AND ",
            readable!(),
            " ORDER BY e.id DESC LIMIT ?4"
        ))
        .bind(viewer.map_or(-1, |v| v.id))
        .bind(viewer.is_some_and(|v| v.is_admin))
        .bind(before.unwrap_or(i64::MAX))
        .bind(limit)
        .bind(actor)
        .fetch_all(&self.db)
        .await?)
    }

    pub(crate) async fn prune_events(&self) -> Result<()> {
        sqlx::query("DELETE FROM events WHERE created_at < ?1")
            .bind(now() - KEEP_DAYS * 86_400)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    /// Records that `actor` created `repo` (forked from `parent`, if any).
    pub async fn repo_created_event(&self, repo_id: i64, actor: &User, parent: Option<&str>) -> Result<()> {
        let (kind, data) = match parent {
            Some(p) => ("fork", serde_json::json!({ "parent": p })),
            None => ("repo_created", serde_json::json!({})),
        };
        self.add_event(repo_id, Some(actor.id), kind, EventTarget::default(), data).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn feeds() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let bob = core.register("bob", "bob@example.com", "password1").await.unwrap();
        let eve = core.register("eve", "eve@example.com", "password1").await.unwrap();
        let public = core.create_repo(&ada, "engine", "", false).await.unwrap();
        let secret = core.create_repo(&ada, "secret", "", true).await.unwrap();
        core.repo_created_event(public.id, &ada, None).await.unwrap();
        core.repo_created_event(secret.id, &ada, None).await.unwrap();
        let issue = core.create_issue(public.id, &bob, "Crash", "").await.unwrap();
        core.add_comment(&issue, &bob, "Details").await.unwrap();

        // Ada watches her repositories: she sees everything there.
        let kinds = |events: Vec<Event>| events.into_iter().map(|e| e.kind).collect::<Vec<_>>();
        assert_eq!(kinds(core.feed(&ada, None, 10).await.unwrap()), ["comment", "issue", "repo_created", "repo_created"]);
        let feed = core.feed(&ada, None, 10).await.unwrap();
        assert_eq!((feed[1].issue_title.as_deref(), feed[1].details()["action"].as_str()), (Some("Crash"), Some("opened")));
        // Paging.
        assert_eq!(core.feed(&ada, Some(feed[1].id), 10).await.unwrap().len(), 2);
        // Bob sees his own activity; eve sees nothing until she watches.
        assert_eq!(kinds(core.feed(&bob, None, 10).await.unwrap()), ["comment", "issue"]);
        assert!(core.feed(&eve, None, 10).await.unwrap().is_empty());
        core.set_watching(eve.id, public.id, true).await.unwrap();
        assert_eq!(core.feed(&eve, None, 10).await.unwrap().len(), 3);

        // Profiles only show what the viewer may read.
        assert_eq!(core.user_events(ada.id, None, None, 10).await.unwrap().len(), 1);
        assert_eq!(core.user_events(ada.id, Some(&ada), None, 10).await.unwrap().len(), 2);
    }
}
