//! Site administration: statistics, managing accounts and instance
//! settings. Callers check that the acting user is a site administrator.

use crate::{Core, Repo, Result, User, invalid, now};

/// Instance settings changed on the admin page. Kept in memory, as every
/// page reads them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SiteSettings {
    /// New accounts can't sign up (administrators can still create them).
    pub registration_closed: bool,
    /// A notice shown at the top of every page, e.g. planned maintenance.
    pub banner: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SiteStats {
    pub users: i64,
    pub admins: i64,
    pub disabled: i64,
    pub orgs: i64,
    pub repos: i64,
    pub private_repos: i64,
    pub issues: i64,
    pub pulls: i64,
    pub pending_webhooks: i64,
    pub failed_webhooks: i64,
    pub pending_mail: i64,
    pub failed_mail: i64,
}

/// An account as listed on the admin page.
#[derive(Clone, Debug)]
pub struct AdminUser {
    pub user: User,
    pub repos: i64,
    /// When they last signed in (started a session).
    pub last_seen: Option<i64>,
}

const MAX_BANNER: usize = 500;

impl Core {
    pub(crate) async fn load_settings(&self) -> Result<()> {
        let rows: Vec<(String, String)> = sqlx::query_as("SELECT key, value FROM site_settings")
            .fetch_all(&self.db)
            .await?;
        let mut s = SiteSettings::default();
        for (key, value) in rows {
            match key.as_str() {
                "registration_closed" => s.registration_closed = value == "1",
                "banner" => s.banner = value,
                _ => {}
            }
        }
        *self.settings.write().expect("settings lock") = s;
        Ok(())
    }

    pub fn site_settings(&self) -> SiteSettings {
        self.settings.read().expect("settings lock").clone()
    }

    /// Whether people can sign up: `--closed-registration` closes it for
    /// good, the admin page for now.
    pub fn registration_open(&self) -> bool {
        self.config.open_registration && !self.settings.read().expect("settings lock").registration_closed
    }

    pub async fn save_site_settings(&self, s: &SiteSettings) -> Result<()> {
        let banner = s.banner.trim();
        if banner.chars().count() > MAX_BANNER {
            return invalid(format!("The notice can be at most {MAX_BANNER} characters."));
        }
        let mut tx = self.db.begin().await?;
        for (key, value) in [
            ("registration_closed", if s.registration_closed { "1" } else { "0" }),
            ("banner", banner),
        ] {
            sqlx::query(
                "INSERT INTO site_settings (key, value) VALUES (?1, ?2)
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            )
            .bind(key)
            .bind(value)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        self.load_settings().await
    }

    /// Whether the database answers (for health checks).
    pub async fn database_ok(&self) -> bool {
        sqlx::query("SELECT 1").execute(&self.db).await.is_ok()
    }

    pub async fn site_stats(&self) -> Result<SiteStats> {
        let (users, admins, disabled, orgs): (i64, i64, i64, i64) = sqlx::query_as(
            "SELECT COALESCE(SUM(kind = 'user'), 0), COALESCE(SUM(kind = 'user' AND is_admin), 0),
                COALESCE(SUM(kind = 'user' AND disabled_at IS NOT NULL), 0), COALESCE(SUM(kind = 'org'), 0)
             FROM users",
        )
        .fetch_one(&self.db)
        .await?;
        let (repos, private_repos): (i64, i64) =
            sqlx::query_as("SELECT COUNT(*), COALESCE(SUM(is_private), 0) FROM repositories")
                .fetch_one(&self.db)
                .await?;
        let (issues, pulls): (i64, i64) =
            sqlx::query_as("SELECT COALESCE(SUM(NOT is_pull), 0), COALESCE(SUM(is_pull), 0) FROM issues")
                .fetch_one(&self.db)
                .await?;
        let (pending_webhooks, failed_webhooks): (i64, i64) = sqlx::query_as(
            "SELECT COALESCE(SUM(status = 'pending'), 0), COALESCE(SUM(status = 'failed'), 0) FROM webhook_deliveries",
        )
        .fetch_one(&self.db)
        .await?;
        let (pending_mail, failed_mail): (i64, i64) = sqlx::query_as(
            "SELECT COALESCE(SUM(status = 'pending'), 0), COALESCE(SUM(status = 'failed'), 0) FROM mail_outbox",
        )
        .fetch_one(&self.db)
        .await?;
        Ok(SiteStats {
            users,
            admins,
            disabled,
            orgs,
            repos,
            private_repos,
            issues,
            pulls,
            pending_webhooks,
            failed_webhooks,
            pending_mail,
            failed_mail,
        })
    }

    /// Accounts (not organizations) whose name or email contains `query`,
    /// newest first, and whether there are more.
    pub async fn admin_users(&self, query: &str, offset: i64, limit: i64) -> Result<(Vec<AdminUser>, bool)> {
        let pattern = format!("%{}%", query.trim().replace(['%', '_', '\\'], ""));
        let users: Vec<User> = sqlx::query_as(concat!(
            "SELECT ",
            user_columns!(),
            " FROM users WHERE kind = 'user' AND (username LIKE ?1 OR email LIKE ?1)
             ORDER BY id DESC LIMIT ?2 OFFSET ?3"
        ))
        .bind(&pattern)
        .bind(limit + 1)
        .bind(offset)
        .fetch_all(&self.db)
        .await?;
        let more = users.len() as i64 > limit;
        let mut out = Vec::new();
        for user in users.into_iter().take(limit as usize) {
            let (repos, last_seen): (i64, Option<i64>) = sqlx::query_as(
                "SELECT (SELECT COUNT(*) FROM repositories WHERE owner_id = ?1),
                        (SELECT MAX(created_at) FROM sessions WHERE user_id = ?1)",
            )
            .bind(user.id)
            .fetch_one(&self.db)
            .await?;
            out.push(AdminUser { user, repos, last_seen });
        }
        Ok((out, more))
    }

    /// All repositories whose `owner/name` contains `query`, most recently
    /// pushed first, and whether there are more.
    pub async fn admin_repos(&self, query: &str, offset: i64, limit: i64) -> Result<(Vec<Repo>, bool)> {
        let pattern = format!("%{}%", query.trim().replace(['%', '_', '\\'], ""));
        let mut repos: Vec<Repo> = sqlx::query_as(
            "SELECT r.id, r.owner_id, u.username AS owner, r.name, r.description,
                r.is_private, r.created_at, r.updated_at, r.forked_from,
                COALESCE((SELECT group_concat(t.topic, ' ' ORDER BY t.topic) FROM repo_topics t WHERE t.repo_id = r.id), '')
                  AS topic_names,
                r.star_count AS stars
             FROM repositories r JOIN users u ON u.id = r.owner_id
             WHERE u.username || '/' || r.name LIKE ?1 ORDER BY r.updated_at DESC, r.id DESC LIMIT ?2 OFFSET ?3",
        )
        .bind(&pattern)
        .bind(limit + 1)
        .bind(offset)
        .fetch_all(&self.db)
        .await?;
        let more = repos.len() as i64 > limit;
        repos.truncate(limit as usize);
        Ok((repos, more))
    }

    /// Creates an account, also when registration is closed.
    pub async fn admin_create_user(&self, username: &str, email: &str, password: &str) -> Result<User> {
        self.insert_user(username, email, password, true).await
    }

    /// Enabled administrators other than `user_id`.
    async fn other_admins(&self, user_id: i64) -> Result<i64> {
        let (n,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM users WHERE kind = 'user' AND is_admin AND disabled_at IS NULL AND id != ?1",
        )
        .bind(user_id)
        .fetch_one(&self.db)
        .await?;
        Ok(n)
    }

    /// Grants or revokes site administration. There must always be an
    /// administrator left.
    pub async fn set_site_admin(&self, user: &User, admin: bool) -> Result<()> {
        if user.is_org() {
            return invalid("Organizations can't be administrators.");
        }
        if !admin && self.other_admins(user.id).await? == 0 {
            return invalid("The site needs at least one administrator.");
        }
        sqlx::query("UPDATE users SET is_admin = ?2 WHERE id = ?1")
            .bind(user.id)
            .bind(admin)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    /// Disables an account (signing it out everywhere) or enables it again.
    pub async fn set_disabled(&self, actor: &User, user: &User, disabled: bool) -> Result<()> {
        if disabled && actor.id == user.id {
            return invalid("You can't disable your own account.");
        }
        if disabled && self.other_admins(user.id).await? == 0 {
            return invalid("The site needs at least one administrator.");
        }
        if user.is_org() {
            return invalid("Organizations can't be disabled.");
        }
        let mut tx = self.db.begin().await?;
        sqlx::query("UPDATE users SET disabled_at = ?2 WHERE id = ?1")
            .bind(user.id)
            .bind(disabled.then(now))
            .execute(&mut *tx)
            .await?;
        if disabled {
            sqlx::query("DELETE FROM sessions WHERE user_id = ?1").bind(user.id).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Deletes an account. Their repositories must be deleted (or the
    /// account must give up organizations it is the only owner of) first;
    /// issues and comments they wrote stay, without an author.
    pub async fn delete_user(&self, actor: &User, user: &User) -> Result<()> {
        if actor.id == user.id {
            return invalid("You can't delete your own account here.");
        }
        if user.is_org() {
            return invalid("Delete organizations from their settings.");
        }
        let (repos, sole_owner): (i64, i64) = sqlx::query_as(
            "SELECT (SELECT COUNT(*) FROM repositories WHERE owner_id = ?1),
                    (SELECT COUNT(*) FROM org_members m WHERE m.user_id = ?1 AND m.role = 'owner'
                       AND NOT EXISTS (SELECT 1 FROM org_members o WHERE o.org_id = m.org_id
                                       AND o.role = 'owner' AND o.user_id != ?1))",
        )
        .bind(user.id)
        .fetch_one(&self.db)
        .await?;
        if repos > 0 {
            return invalid(format!("{} owns {repos} repositories; delete those first.", user.username));
        }
        if sole_owner > 0 {
            return invalid(format!("{} is the only owner of an organization; add another owner first.", user.username));
        }
        if self.other_admins(user.id).await? == 0 {
            return invalid("The site needs at least one administrator.");
        }
        sqlx::query("DELETE FROM users WHERE id = ?1").bind(user.id).execute(&self.db).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn administration() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let bob = core.register("bob", "bob@example.com", "password1").await.unwrap();
        assert!(ada.is_admin && !bob.is_admin);

        // Registration can be closed; admins can still create accounts.
        assert!(core.registration_open());
        core.save_site_settings(&SiteSettings { registration_closed: true, banner: " Maintenance at 10 ".into() })
            .await
            .unwrap();
        assert!(!core.registration_open());
        assert_eq!(core.site_settings().banner, "Maintenance at 10");
        assert!(core.register("eve", "eve@example.com", "password1").await.is_err());
        let eve = core.admin_create_user("eve", "eve@example.com", "password1").await.unwrap();
        core.load_settings().await.unwrap();
        assert!(!core.registration_open(), "settings are stored");

        // Disabling locks people out.
        let session = core.create_session(bob.id).await.unwrap();
        let (_, token) = core.create_token(bob.id, "ci", true, None).await.unwrap();
        assert!(core.set_disabled(&ada, &ada, true).await.is_err(), "not yourself");
        core.set_disabled(&ada, &bob, true).await.unwrap();
        assert!(core.session_user(&session).await.unwrap().is_none());
        assert!(core.authenticate("bob", "password1").await.unwrap().is_none());
        assert!(core.authenticate_token(&token).await.unwrap().is_none());
        assert!(core.user_by_name("bob").await.unwrap().unwrap().disabled);
        core.set_disabled(&ada, &bob, false).await.unwrap();
        assert!(core.authenticate_token(&token).await.unwrap().is_some());

        // Administrators.
        assert!(core.set_site_admin(&ada, false).await.is_err(), "the last one");
        core.set_site_admin(&bob, true).await.unwrap();
        core.set_site_admin(&ada, false).await.unwrap();
        let stats = core.site_stats().await.unwrap();
        assert_eq!((stats.users, stats.admins, stats.disabled), (3, 1, 0));

        // Listing and deleting.
        let (users, more) = core.admin_users("", 0, 2).await.unwrap();
        assert!(more);
        assert_eq!(users.iter().map(|u| u.user.username.as_str()).collect::<Vec<_>>(), ["eve", "bob"]);
        assert_eq!(core.admin_users("EVE@", 0, 10).await.unwrap().0.len(), 1);
        let notes = core.create_repo(&eve, "notes", "", false).await.unwrap();
        assert!(core.delete_user(&ada, &eve).await.unwrap_err().to_string().contains("repositories"));
        let issue = core.create_issue(notes.id, &bob, "Bug", "").await.unwrap();
        assert!(core.delete_user(&ada, &bob).await.is_err(), "the last admin");
        core.set_site_admin(&ada, true).await.unwrap();
        core.delete_user(&ada, &bob).await.unwrap();
        assert!(core.user_by_name("bob").await.unwrap().is_none());
        assert_eq!(core.issue(issue.repo_id, issue.number).await.unwrap().unwrap().author, None);
    }
}
