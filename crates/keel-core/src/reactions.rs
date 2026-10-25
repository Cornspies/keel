//! Reactions (👍 ❤️ 🚀 …) to issues, pull requests and their comments, with
//! GitHub's eight kinds.

use std::collections::HashMap;

use serde_json::{Value, json};

use crate::{Core, Error, Issue, Result, User, invalid, now};

/// The kinds of reaction (GitHub's names) and how they're shown.
pub const REACTIONS: [(&str, &str); 8] = [
    ("+1", "👍"),
    ("-1", "👎"),
    ("laugh", "😄"),
    ("hooray", "🎉"),
    ("confused", "😕"),
    ("heart", "❤️"),
    ("rocket", "🚀"),
    ("eyes", "👀"),
];

/// Names shown for who reacted, beyond which it says "and N more".
const NAMES_SHOWN: usize = 10;

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Reaction {
    pub id: i64,
    /// `None`: a reaction to the issue or pull request itself.
    pub comment_id: Option<i64>,
    pub user_id: i64,
    pub username: String,
    pub content: String,
    pub created_at: i64,
}

/// One kind of reaction to one thing: how many, and by whom.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReactionGroup {
    pub content: &'static str,
    pub emoji: &'static str,
    pub count: usize,
    /// The first few people (in the order they reacted).
    pub names: Vec<String>,
    /// Whether the viewer is among them.
    pub mine: bool,
}

impl ReactionGroup {
    /// "ada, bob and 3 more reacted with 👍".
    pub fn describe(&self) -> String {
        let shown = self.names.join(", ");
        let more = self.count.saturating_sub(self.names.len());
        if more > 0 { format!("{shown} and {more} more reacted with {}", self.emoji) } else { format!("{shown} reacted with {}", self.emoji) }
    }
}

/// Whether `content` is a kind of reaction, in its canonical form.
pub fn reaction_kind(content: &str) -> Option<&'static str> {
    REACTIONS.iter().map(|(k, _)| *k).find(|k| *k == content)
}

/// Groups reactions by what they react to (`None`: the issue), each in
/// [`REACTIONS`] order.
pub fn group_reactions(reactions: &[Reaction], viewer: Option<i64>) -> HashMap<Option<i64>, Vec<ReactionGroup>> {
    let mut out: HashMap<Option<i64>, Vec<ReactionGroup>> = HashMap::new();
    for r in reactions {
        let Some(i) = REACTIONS.iter().position(|(k, _)| *k == r.content) else { continue };
        let groups = out.entry(r.comment_id).or_default();
        let group = match groups.iter_mut().find(|g| g.content == REACTIONS[i].0) {
            Some(g) => g,
            None => {
                groups.push(ReactionGroup { content: REACTIONS[i].0, emoji: REACTIONS[i].1, count: 0, names: Vec::new(), mine: false });
                groups.last_mut().expect("just pushed")
            }
        };
        group.count += 1;
        if group.names.len() < NAMES_SHOWN {
            group.names.push(r.username.clone());
        }
        group.mine |= viewer == Some(r.user_id);
    }
    for groups in out.values_mut() {
        groups.sort_by_key(|g| REACTIONS.iter().position(|(k, _)| *k == g.content));
    }
    out
}

impl Core {
    /// Every reaction to the issue and its comments, oldest first.
    pub async fn reactions(&self, issue_id: i64) -> Result<Vec<Reaction>> {
        Ok(sqlx::query_as(
            "SELECT r.id, r.comment_id, r.user_id, u.username, r.content, r.created_at
             FROM reactions r JOIN users u ON u.id = r.user_id
             WHERE r.issue_id = ?1 ORDER BY r.id",
        )
        .bind(issue_id)
        .fetch_all(&self.db)
        .await?)
    }

    /// Checks that `comment_id` is a comment of `issue` that can have
    /// reactions (not an event like "closed").
    async fn check_reactable(&self, issue: &Issue, comment_id: Option<i64>) -> Result<()> {
        let Some(id) = comment_id else { return Ok(()) };
        let (n,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM comments WHERE id = ?1 AND issue_id = ?2
               AND kind IN ('comment', 'inline', 'approved', 'changes_requested')",
        )
        .bind(id)
        .bind(issue.id)
        .fetch_one(&self.db)
        .await?;
        if n == 0 {
            return Err(Error::NotFound);
        }
        Ok(())
    }

    /// Adds `user`'s reaction. Returns it, and whether it is new (rather
    /// than already there).
    pub async fn add_reaction(&self, issue: &Issue, comment_id: Option<i64>, user: &User, content: &str) -> Result<(Reaction, bool)> {
        let Some(content) = reaction_kind(content) else {
            return invalid(format!("Unknown reaction “{content}”."));
        };
        self.check_reactable(issue, comment_id).await?;
        crate::issues::ensure_issue_unarchived(&self.db, issue.id).await?;
        let inserted = sqlx::query(
            "INSERT OR IGNORE INTO reactions (issue_id, comment_id, user_id, content, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(issue.id)
        .bind(comment_id)
        .bind(user.id)
        .bind(content)
        .bind(now())
        .execute(&self.db)
        .await?
        .rows_affected()
            > 0;
        let reaction = sqlx::query_as(
            "SELECT r.id, r.comment_id, r.user_id, u.username, r.content, r.created_at
             FROM reactions r JOIN users u ON u.id = r.user_id
             WHERE r.issue_id = ?1 AND IFNULL(r.comment_id, 0) = IFNULL(?2, 0) AND r.user_id = ?3 AND r.content = ?4",
        )
        .bind(issue.id)
        .bind(comment_id)
        .bind(user.id)
        .bind(content)
        .fetch_one(&self.db)
        .await?;
        Ok((reaction, inserted))
    }

    /// Adds `user`'s reaction, or takes it back if it's there. Returns
    /// whether it's there now.
    pub async fn toggle_reaction(&self, issue: &Issue, comment_id: Option<i64>, user: &User, content: &str) -> Result<bool> {
        crate::issues::ensure_issue_unarchived(&self.db, issue.id).await?;
        let removed = sqlx::query(
            "DELETE FROM reactions WHERE issue_id = ?1 AND IFNULL(comment_id, 0) = IFNULL(?2, 0) AND user_id = ?3 AND content = ?4",
        )
        .bind(issue.id)
        .bind(comment_id)
        .bind(user.id)
        .bind(content)
        .execute(&self.db)
        .await?
        .rows_affected()
            > 0;
        if removed {
            return Ok(false);
        }
        self.add_reaction(issue, comment_id, user, content).await?;
        Ok(true)
    }

    /// Deletes reaction `id` to the issue or comment, if it's `user`'s.
    pub async fn delete_reaction(&self, issue: &Issue, comment_id: Option<i64>, user: &User, id: i64) -> Result<()> {
        crate::issues::ensure_issue_unarchived(&self.db, issue.id).await?;
        let deleted = sqlx::query(
            "DELETE FROM reactions WHERE id = ?1 AND issue_id = ?2 AND IFNULL(comment_id, 0) = IFNULL(?3, 0) AND user_id = ?4",
        )
        .bind(id)
        .bind(issue.id)
        .bind(comment_id)
        .bind(user.id)
        .execute(&self.db)
        .await?
        .rows_affected();
        if deleted == 0 {
            return Err(Error::NotFound);
        }
        Ok(())
    }

    /// The number of the issue or pull request that comment `comment_id` of
    /// the repository belongs to.
    pub async fn comment_issue_number(&self, repo_id: i64, comment_id: i64) -> Result<Option<i64>> {
        Ok(sqlx::query_as::<_, (i64,)>(
            "SELECT i.number FROM comments c JOIN issues i ON i.id = c.issue_id WHERE c.id = ?1 AND i.repo_id = ?2",
        )
        .bind(comment_id)
        .bind(repo_id)
        .fetch_optional(&self.db)
        .await?
        .map(|(n,)| n))
    }

    /// GitHub's `reactions` rollup of the issue or one of its comments.
    pub async fn reactions_json(&self, issue_id: i64, comment_id: Option<i64>) -> Result<Value> {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT content, COUNT(*) FROM reactions WHERE issue_id = ?1 AND IFNULL(comment_id, 0) = IFNULL(?2, 0) GROUP BY content",
        )
        .bind(issue_id)
        .bind(comment_id)
        .fetch_all(&self.db)
        .await?;
        let mut v = json!({ "total_count": rows.iter().map(|(_, n)| n).sum::<i64>() });
        for (kind, _) in REACTIONS {
            v[kind] = json!(rows.iter().find(|(k, _)| k == kind).map_or(0, |(_, n)| *n));
        }
        Ok(v)
    }

    pub fn reaction_json(&self, r: &Reaction) -> Value {
        json!({
            "id": r.id,
            "content": r.content,
            "user": { "id": r.user_id, "login": r.username, "html_url": format!("{}/{}", self.config.base_url, r.username) },
            "created_at": crate::activity::time(r.created_at),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reacting() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let bob = core.register("bob", "bob@example.com", "password1").await.unwrap();
        let repo = core.create_repo(&ada, "engine", "", false).await.unwrap();
        let issue = core.create_issue(repo.id, &ada, "Crash", "On start").await.unwrap();
        let comment = core.add_comment(&issue, &bob, "Same here").await.unwrap();

        assert!(core.toggle_reaction(&issue, None, &bob, "+1").await.unwrap());
        assert!(core.toggle_reaction(&issue, None, &ada, "+1").await.unwrap());
        assert!(core.toggle_reaction(&issue, None, &ada, "rocket").await.unwrap());
        assert!(core.toggle_reaction(&issue, Some(comment), &ada, "heart").await.unwrap());
        let (_, new) = core.add_reaction(&issue, Some(comment), &ada, "heart").await.unwrap();
        assert!(!new, "adding twice keeps one");
        assert!(core.add_reaction(&issue, None, &ada, "party").await.is_err());
        assert!(matches!(core.add_reaction(&issue, Some(comment + 100), &ada, "eyes").await, Err(Error::NotFound)));

        let all = core.reactions(issue.id).await.unwrap();
        let groups = group_reactions(&all, Some(bob.id));
        let on_issue = &groups[&None];
        assert_eq!(on_issue.iter().map(|g| (g.content, g.count, g.mine)).collect::<Vec<_>>(), [("+1", 2, true), ("rocket", 1, false)]);
        assert_eq!(on_issue[0].describe(), "bob, ada reacted with 👍");
        assert_eq!(groups[&Some(comment)][0].content, "heart");
        let rollup = core.reactions_json(issue.id, None).await.unwrap();
        assert_eq!((rollup["total_count"].as_i64(), rollup["+1"].as_i64(), rollup["eyes"].as_i64()), (Some(3), Some(2), Some(0)));

        assert!(!core.toggle_reaction(&issue, None, &bob, "+1").await.unwrap());
        let heart = all.iter().find(|r| r.content == "heart").unwrap();
        assert!(core.delete_reaction(&issue, Some(comment), &bob, heart.id).await.is_err(), "only your own");
        core.delete_reaction(&issue, Some(comment), &ada, heart.id).await.unwrap();
        assert_eq!(core.reactions(issue.id).await.unwrap().len(), 2);
    }
}
