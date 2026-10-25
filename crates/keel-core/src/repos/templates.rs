//! Template repositories: new repositories that start with a copy of a
//! template's files (without its history) and labels.

use keel_git::{GitRepo, Identity};

use crate::{Access, Core, Error, Repo, Result, User, now};

impl Core {
    /// Marks `repo_id` as a template, or not.
    pub async fn set_template(&self, repo_id: i64, is_template: bool) -> Result<()> {
        sqlx::query("UPDATE repositories SET is_template = ?2 WHERE id = ?1")
            .bind(repo_id)
            .bind(is_template)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    /// Templates `viewer` can use, by name.
    pub async fn templates(&self, viewer: &User) -> Result<Vec<Repo>> {
        Ok(sqlx::query_as(concat!(
            repo_select!(),
            "WHERE r.is_template AND ",
            visible_to!(),
            " ORDER BY u.username, r.name LIMIT 200"
        ))
        .bind(0) // ?1 is unused; `visible_to!` takes ?2 and ?3.
        .bind(viewer.id)
        .bind(viewer.is_admin)
        .fetch_all(&self.db)
        .await?)
    }

    /// Creates `owner/name` (as `actor`, who must be able to read
    /// `template`) from a template: one commit with the files of its
    /// default branch (or of every branch), and its labels.
    #[allow(clippy::too_many_arguments)]
    pub async fn generate_repo(
        &self,
        template: &Repo,
        actor: &User,
        owner: &User,
        name: &str,
        description: &str,
        is_private: bool,
        all_branches: bool,
    ) -> Result<Repo> {
        if !template.is_template || self.access(template, Some(actor)).await? == Access::None {
            return Err(Error::NotFound);
        }
        let repo = self.create_repo(owner, name, description, is_private).await?;
        if let Err(e) = self.fill_from_template(template, &repo, actor, all_branches).await {
            self.delete_repo(&repo).await?;
            return Err(e);
        }
        Ok(Repo { template_id: Some(template.id), ..repo })
    }

    async fn fill_from_template(&self, template: &Repo, repo: &Repo, actor: &User, all_branches: bool) -> Result<()> {
        let (from, to) = (self.repo_path(template), self.repo_path(repo));
        let (name, email) = (actor.username.clone(), actor.email.clone());
        tokio::task::spawn_blocking(move || {
            let template = GitRepo::open(&from)?;
            let Some(head) = template.default_branch()? else { return Ok(()) };
            let branches = if all_branches {
                template.branches()?.into_iter().map(|b| b.name).collect()
            } else {
                vec![head.clone()]
            };
            let who = Identity { name: &name, email: &email };
            GitRepo::open(&to)?.start_from_template(&template, &branches, &head, "Initial commit", &who)
        })
        .await
        .map_err(anyhow::Error::from)??;

        let now = now();
        let mut tx = self.db.begin().await?;
        sqlx::query("UPDATE repositories SET template_id = ?2 WHERE id = ?1")
            .bind(repo.id)
            .bind(template.id)
            .execute(&mut *tx)
            .await?;
        // The template's labels instead of the default ones.
        let (labels,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM labels WHERE repo_id = ?1")
            .bind(template.id)
            .fetch_one(&mut *tx)
            .await?;
        if labels > 0 {
            sqlx::query("DELETE FROM labels WHERE repo_id = ?1").bind(repo.id).execute(&mut *tx).await?;
            sqlx::query(
                "INSERT INTO labels (repo_id, name, color, description, created_at)
                 SELECT ?1, name, color, description, ?3 FROM labels WHERE repo_id = ?2",
            )
            .bind(repo.id)
            .bind(template.id)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }
        // Large files in the copied files.
        sqlx::query(
            "INSERT OR IGNORE INTO lfs_objects (repo_id, oid, size, created_at)
             SELECT ?1, oid, size, ?3 FROM lfs_objects WHERE repo_id = ?2",
        )
        .bind(repo.id)
        .bind(template.id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        // Index and warm caches like after a push.
        self.after_push(repo.id, Some(actor.id), None).await
    }

    /// The template `repo` was generated from, if `viewer` can see it.
    pub async fn template_of(&self, repo: &Repo, viewer: Option<&User>) -> Result<Option<Repo>> {
        let Some(id) = repo.template_id else { return Ok(None) };
        let Some(template) = self.repo_by_id(id).await? else { return Ok(None) };
        Ok((self.access(&template, viewer).await? != Access::None).then_some(template))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[tokio::test]
    async fn generating() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let bob = core.register("bob", "bob@example.com", "password1").await.unwrap();
        let template = core.create_repo(&ada, "starter", "", false).await.unwrap();
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
            assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        git(&["init", "-q", "-b", "trunk"]);
        std::fs::write(work.path().join("README.md"), "# Starter").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "one"]);
        git(&["commit", "-q", "--allow-empty", "-m", "two"]);
        git(&["branch", "extra"]);
        git(&["push", "-q", core.repo_path(&template).to_str().unwrap(), "trunk", "extra"]);
        let head = Command::new("git").arg("--git-dir").arg(core.repo_path(&template)).args(["symbolic-ref", "HEAD", "refs/heads/trunk"]).status();
        assert!(head.unwrap().success());
        core.save_label(template.id, None, "triage", "blue", "").await.unwrap();

        assert!(core.generate_repo(&template, &bob, &bob, "mine", "", false, false).await.is_err(), "not a template yet");
        core.set_template(template.id, true).await.unwrap();
        let template = core.repo_by_id(template.id).await.unwrap().unwrap();
        assert_eq!(core.templates(&bob).await.unwrap().len(), 1);

        let repo = core.generate_repo(&template, &bob, &bob, "mine", "From a template", true, false).await.unwrap();
        let (branches, head, count) = core
            .git(repo.id, |g| {
                let tip = g.branch_tip("trunk").unwrap();
                Ok((g.branches()?.len(), g.default_branch()?, g.log(tip, "", 0, 10)?.len()))
            })
            .await
            .unwrap();
        assert_eq!((branches, head.as_deref(), count), (1, Some("trunk"), 1));
        let labels: Vec<String> = core.labels(repo.id).await.unwrap().into_iter().map(|l| l.name).collect();
        assert!(labels.contains(&"triage".to_owned()), "{labels:?}");
        let repo = core.repo_by_id(repo.id).await.unwrap().unwrap();
        assert_eq!(repo.template_id, Some(template.id));
        assert_eq!(core.template_of(&repo, Some(&bob)).await.unwrap().map(|t| t.id), Some(template.id));

        let all = core.generate_repo(&template, &bob, &bob, "all", "", false, true).await.unwrap();
        assert_eq!(core.git(all.id, |g| Ok(g.branches()?.len())).await.unwrap(), 2);
        assert!(core.generate_repo(&template, &bob, &bob, "all", "", false, true).await.is_err(), "name taken");

        // Private templates are only for those who can read them.
        core.update_repo(template.id, "", true).await.unwrap();
        let template = core.repo_by_id(template.id).await.unwrap().unwrap();
        assert!(core.generate_repo(&template, &bob, &bob, "again", "", false, false).await.is_err());
        assert!(core.templates(&bob).await.unwrap().is_empty());
        assert_eq!(core.template_of(&repo, Some(&bob)).await.unwrap().map(|t| t.id), None);
    }
}
