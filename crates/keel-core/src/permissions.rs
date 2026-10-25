//! Who may do what: access levels, repository collaborators and
//! organizations.
//!
//! A viewer's access to a repository is the highest of:
//! - Admin for the owner, site administrators and organization owners,
//! - Write for organization members,
//! - the role of a collaborator,
//! - Read for anyone if the repository is public.

use crate::{Core, Error, Repo, Result, User, invalid, now, validate_username};

/// What a viewer may do with a repository. Ordered: each level includes the
/// ones below it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Access {
    None,
    /// See, clone, open issues and pull requests, comment.
    Read,
    /// Push, merge pull requests, manage issues.
    Write,
    /// Settings, collaborators, deletion.
    Admin,
}

impl Access {
    fn from_role(role: &str) -> Self {
        match role {
            "admin" | "owner" => Self::Admin,
            "write" | "member" => Self::Write,
            "read" => Self::Read,
            _ => Self::None,
        }
    }

    pub fn role_name(self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::Write => "write",
            Self::Read => "read",
            Self::None => "none",
        }
    }

    /// Parses a collaborator role from a form.
    pub fn parse_role(s: &str) -> Option<Self> {
        match s {
            "read" => Some(Self::Read),
            "write" => Some(Self::Write),
            "admin" => Some(Self::Admin),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrgRole {
    Owner,
    Member,
}

impl OrgRole {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "owner" => Some(Self::Owner),
            "member" => Some(Self::Member),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Member => "member",
        }
    }
}

impl Core {
    /// What `viewer` may do in `repo`. Archived repositories are read-only
    /// for everyone.
    pub async fn access(&self, repo: &Repo, viewer: Option<&User>) -> Result<Access> {
        let access = self.granted_access(repo, viewer).await?;
        Ok(if repo.is_archived() { access.min(Access::Read) } else { access })
    }

    /// [`access`](Self::access) as if `repo` weren't archived: settings
    /// (to unarchive it, say) go by this.
    pub async fn granted_access(&self, repo: &Repo, viewer: Option<&User>) -> Result<Access> {
        let public = if repo.is_private { Access::None } else { Access::Read };
        let Some(user) = viewer else {
            return Ok(public);
        };
        if user.is_admin || user.id == repo.owner_id {
            return Ok(Access::Admin);
        }
        let (collaborator, member): (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT (SELECT role FROM collaborators WHERE repo_id = ?1 AND user_id = ?2),
                    (SELECT role FROM org_members WHERE org_id = ?3 AND user_id = ?2)",
        )
        .bind(repo.id)
        .bind(user.id)
        .bind(repo.owner_id)
        .fetch_one(&self.db)
        .await?;
        Ok([collaborator, member]
            .iter()
            .flatten()
            .map(|r| Access::from_role(r))
            .fold(public, Access::max))
    }

    // -- Collaborators ------------------------------------------------------

    pub async fn collaborators(&self, repo_id: i64) -> Result<Vec<(User, Access)>> {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT role, user_id FROM collaborators WHERE repo_id = ?1 ORDER BY created_at",
        )
        .bind(repo_id)
        .fetch_all(&self.db)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for (role, user_id) in rows {
            if let Some(user) = self.user_by_id(user_id).await? {
                out.push((user, Access::from_role(&role)));
            }
        }
        out.sort_by_key(|(u, _)| u.username.to_lowercase());
        Ok(out)
    }

    /// Adds a collaborator or changes their role.
    pub async fn set_collaborator(&self, repo: &Repo, username: &str, role: Access) -> Result<()> {
        if role == Access::None {
            return invalid("Choose a role.");
        }
        let Some(user) = self.user_by_name(username.trim()).await?.filter(|u| !u.is_org()) else {
            return invalid(format!("There is no user named “{}”.", username.trim()));
        };
        if user.id == repo.owner_id {
            return invalid("The owner already has full access.");
        }
        sqlx::query(
            "INSERT INTO collaborators (repo_id, user_id, role, created_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (repo_id, user_id) DO UPDATE SET role = excluded.role",
        )
        .bind(repo.id)
        .bind(user.id)
        .bind(role.role_name())
        .bind(now())
        .execute(&self.db)
        .await?;
        Ok(())
    }

    pub async fn remove_collaborator(&self, repo_id: i64, user_id: i64) -> Result<()> {
        sqlx::query("DELETE FROM collaborators WHERE repo_id = ?1 AND user_id = ?2")
            .bind(repo_id)
            .bind(user_id)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    // -- Organizations ------------------------------------------------------

    /// Creates an organization with `creator` as its owner.
    pub async fn create_org(&self, creator: &User, name: &str) -> Result<User> {
        let name = name.trim();
        validate_username(name)?;
        let mut tx = self.db.begin().await?;
        let inserted = sqlx::query_as::<_, (i64,)>(
            "INSERT INTO users (username, kind, created_at) VALUES (?1, 'org', ?2) RETURNING id",
        )
        .bind(name)
        .bind(now())
        .fetch_one(&mut *tx)
        .await;
        let (org_id,) = match inserted {
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
                return invalid(format!("The name “{name}” is taken."));
            }
            r => r?,
        };
        sqlx::query("INSERT INTO org_members (org_id, user_id, role, created_at) VALUES (?1, ?2, 'owner', ?3)")
            .bind(org_id)
            .bind(creator.id)
            .bind(now())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.user_by_id(org_id).await?.ok_or(Error::NotFound)
    }

    pub async fn org_role(&self, org_id: i64, user_id: i64) -> Result<Option<OrgRole>> {
        let row: Option<(String,)> = sqlx::query_as("SELECT role FROM org_members WHERE org_id = ?1 AND user_id = ?2")
            .bind(org_id)
            .bind(user_id)
            .fetch_optional(&self.db)
            .await?;
        Ok(row.and_then(|(r,)| OrgRole::parse(&r)))
    }

    pub async fn org_members(&self, org_id: i64) -> Result<Vec<(User, OrgRole)>> {
        let rows: Vec<(String, i64)> =
            sqlx::query_as("SELECT role, user_id FROM org_members WHERE org_id = ?1")
                .bind(org_id)
                .fetch_all(&self.db)
                .await?;
        let mut out = Vec::with_capacity(rows.len());
        for (role, user_id) in rows {
            if let (Some(user), Some(role)) = (self.user_by_id(user_id).await?, OrgRole::parse(&role)) {
                out.push((user, role));
            }
        }
        out.sort_by_key(|(u, r)| (*r != OrgRole::Owner, u.username.to_lowercase()));
        Ok(out)
    }

    /// Organizations `user_id` belongs to.
    pub async fn orgs_of(&self, user_id: i64) -> Result<Vec<(User, OrgRole)>> {
        let rows: Vec<(String, i64)> =
            sqlx::query_as("SELECT role, org_id FROM org_members WHERE user_id = ?1")
                .bind(user_id)
                .fetch_all(&self.db)
                .await?;
        let mut out = Vec::with_capacity(rows.len());
        for (role, org_id) in rows {
            if let (Some(org), Some(role)) = (self.user_by_id(org_id).await?, OrgRole::parse(&role)) {
                out.push((org, role));
            }
        }
        out.sort_by_key(|(o, _)| o.username.to_lowercase());
        Ok(out)
    }

    /// Adds a member or changes their role. An organization always keeps at
    /// least one owner.
    pub async fn set_org_member(&self, org: &User, username: &str, role: OrgRole) -> Result<()> {
        let Some(user) = self.user_by_name(username.trim()).await?.filter(|u| !u.is_org()) else {
            return invalid(format!("There is no user named “{}”.", username.trim()));
        };
        if role == OrgRole::Member && self.is_last_owner(org.id, user.id).await? {
            return invalid("An organization needs at least one owner.");
        }
        sqlx::query(
            "INSERT INTO org_members (org_id, user_id, role, created_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (org_id, user_id) DO UPDATE SET role = excluded.role",
        )
        .bind(org.id)
        .bind(user.id)
        .bind(role.as_str())
        .bind(now())
        .execute(&self.db)
        .await?;
        Ok(())
    }

    pub async fn remove_org_member(&self, org: &User, user_id: i64) -> Result<()> {
        if self.is_last_owner(org.id, user_id).await? {
            return invalid("An organization needs at least one owner.");
        }
        sqlx::query("DELETE FROM org_members WHERE org_id = ?1 AND user_id = ?2")
            .bind(org.id)
            .bind(user_id)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    async fn is_last_owner(&self, org_id: i64, user_id: i64) -> Result<bool> {
        let (owners, is_owner): (i64, bool) = sqlx::query_as(
            "SELECT COUNT(*), COALESCE(MAX(user_id = ?2), 0) FROM org_members
             WHERE org_id = ?1 AND role = 'owner'",
        )
        .bind(org_id)
        .bind(user_id)
        .fetch_one(&self.db)
        .await?;
        Ok(is_owner && owners == 1)
    }

    /// Deletes an organization. Its repositories must be deleted first.
    pub async fn delete_org(&self, org: &User) -> Result<()> {
        if !org.is_org() {
            return Err(Error::NotFound);
        }
        let (repos,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM repositories WHERE owner_id = ?1")
            .bind(org.id)
            .fetch_one(&self.db)
            .await?;
        if repos > 0 {
            return invalid("Delete or move the organization’s repositories first.");
        }
        sqlx::query("DELETE FROM users WHERE id = ?1 AND kind = 'org'")
            .bind(org.id)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    /// Whether `user` may create repositories owned by `owner` (themselves
    /// or an organization they belong to).
    pub async fn can_create_repo_in(&self, owner: &User, user: &User) -> Result<bool> {
        if owner.id == user.id {
            return Ok(true);
        }
        Ok(owner.is_org() && self.org_role(owner.id, user.id).await?.is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn access_levels() {
        let (core, _dir) = Core::for_tests().await;
        let root = core.register("sysop", "sysop@example.com", "password1").await.unwrap();
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let bob = core.register("bob", "bob@example.com", "password1").await.unwrap();
        let eve = core.register("eve", "eve@example.com", "password1").await.unwrap();
        assert!(root.is_admin && !ada.is_admin);

        let public = core.create_repo(&ada, "engine", "", false).await.unwrap();
        let secret = core.create_repo(&ada, "notes", "", true).await.unwrap();
        let a = |repo, user| {
            let core = core.clone();
            async move { core.access(repo, user).await.unwrap() }
        };
        assert_eq!(a(&public, None).await, Access::Read);
        assert_eq!(a(&public, Some(&bob)).await, Access::Read);
        assert_eq!(a(&public, Some(&ada)).await, Access::Admin);
        assert_eq!(a(&secret, None).await, Access::None);
        assert_eq!(a(&secret, Some(&bob)).await, Access::None);
        assert_eq!(a(&secret, Some(&root)).await, Access::Admin, "site admin");

        // Collaborators.
        assert!(core.set_collaborator(&secret, "nobody", Access::Read).await.is_err());
        assert!(core.set_collaborator(&secret, "ada", Access::Read).await.is_err(), "owner");
        core.set_collaborator(&secret, "bob", Access::Read).await.unwrap();
        assert_eq!(a(&secret, Some(&bob)).await, Access::Read);
        core.set_collaborator(&secret, "BOB", Access::Write).await.unwrap();
        assert_eq!(a(&secret, Some(&bob)).await, Access::Write);
        assert_eq!(core.collaborators(secret.id).await.unwrap().len(), 1);
        assert_eq!(core.repos_of(ada.id, Some(&bob)).await.unwrap().len(), 2);
        assert_eq!(core.repos_of(ada.id, Some(&eve)).await.unwrap().len(), 1);
        assert_eq!(core.repos_for(&bob).await.unwrap().len(), 1);
        core.remove_collaborator(secret.id, bob.id).await.unwrap();
        assert_eq!(a(&secret, Some(&bob)).await, Access::None);

        // Organizations.
        let org = core.create_org(&ada, "acme").await.unwrap();
        assert!(org.is_org());
        assert!(core.create_org(&bob, "Acme").await.is_err(), "name taken");
        assert!(core.create_org(&bob, "ada").await.is_err(), "user names too");
        assert!(core.authenticate("acme", "").await.unwrap().is_none());
        let tool = core.create_repo(&org, "tool", "", true).await.unwrap();
        assert_eq!(a(&tool, Some(&ada)).await, Access::Admin);
        assert_eq!(a(&tool, Some(&bob)).await, Access::None);
        core.set_org_member(&org, "bob", OrgRole::Member).await.unwrap();
        assert_eq!(a(&tool, Some(&bob)).await, Access::Write);
        assert!(core.can_create_repo_in(&org, &bob).await.unwrap());
        assert!(!core.can_create_repo_in(&org, &eve).await.unwrap());
        assert!(!core.can_create_repo_in(&ada, &bob).await.unwrap());
        assert_eq!(core.orgs_of(bob.id).await.unwrap()[0].0.username, "acme");

        // The last owner can't leave or be demoted.
        assert!(core.remove_org_member(&org, ada.id).await.is_err());
        assert!(core.set_org_member(&org, "ada", OrgRole::Member).await.is_err());
        core.set_org_member(&org, "bob", OrgRole::Owner).await.unwrap();
        core.set_org_member(&org, "ada", OrgRole::Member).await.unwrap();
        assert_eq!(core.org_members(org.id).await.unwrap()[0].0.username, "bob");

        assert!(core.delete_org(&org).await.is_err(), "has repositories");
        core.delete_repo(&tool).await.unwrap();
        core.delete_org(&org).await.unwrap();
        assert!(core.user_by_name("acme").await.unwrap().is_none());
        assert!(core.orgs_of(bob.id).await.unwrap().is_empty());
    }
}
