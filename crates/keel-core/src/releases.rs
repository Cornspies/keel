//! Releases: notes and files attached to a tag. A release can start as a
//! draft (visible to people with write access); publishing it creates its
//! tag if needed.

use std::path::{Path, PathBuf};

use crate::activity::Activity;
use crate::{Core, Error, Repo, Result, User, invalid, now};

const MAX_ASSETS: i64 = 100;

#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct Release {
    pub id: i64,
    pub repo_id: i64,
    pub tag: String,
    pub target: String,
    pub title: String,
    pub notes: String,
    pub draft: bool,
    pub prerelease: bool,
    pub author: Option<String>,
    pub created_at: i64,
    pub published_at: Option<i64>,
}

impl Release {
    /// The title, or the tag without one.
    pub fn name(&self) -> &str {
        if self.title.is_empty() { &self.tag } else { &self.title }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct ReleaseAsset {
    pub id: i64,
    pub release_id: i64,
    pub name: String,
    pub size: i64,
    pub content_type: String,
    pub downloads: i64,
    pub created_at: i64,
}

/// What happened to a release, for webhooks and the activity feed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseAction {
    Published,
    Edited,
    Deleted,
}

macro_rules! release_select {
    () => {
        "SELECT r.id, r.repo_id, r.tag, r.target, r.title, r.notes, r.draft, r.prerelease, u.username AS author,
            r.created_at, r.published_at
         FROM releases r LEFT JOIN users u ON u.id = r.author_id "
    };
}

fn validate_asset_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 255
        && !name.contains(['/', '\\'])
        && name != "."
        && name != ".."
        && !name.chars().any(char::is_control);
    if !ok {
        return invalid("File names can't be empty or contain slashes.");
    }
    Ok(())
}

impl Core {
    pub fn release_asset_path(&self, id: i64) -> PathBuf {
        self.config.data_dir.join("releases").join(id.to_string())
    }

    /// Releases, newest first (drafts first, if included).
    pub async fn releases(&self, repo_id: i64, drafts: bool, offset: i64, limit: i64) -> Result<Vec<Release>> {
        Ok(sqlx::query_as(concat!(
            release_select!(),
            "WHERE r.repo_id = ?1 AND (?2 OR NOT r.draft)
             ORDER BY r.draft DESC, COALESCE(r.published_at, r.created_at) DESC, r.id DESC LIMIT ?3 OFFSET ?4"
        ))
        .bind(repo_id)
        .bind(drafts)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.db)
        .await?)
    }

    pub async fn release(&self, repo_id: i64, id: i64) -> Result<Option<Release>> {
        Ok(sqlx::query_as(concat!(release_select!(), "WHERE r.repo_id = ?1 AND r.id = ?2"))
            .bind(repo_id)
            .bind(id)
            .fetch_optional(&self.db)
            .await?)
    }

    pub async fn release_by_tag(&self, repo_id: i64, tag: &str) -> Result<Option<Release>> {
        Ok(sqlx::query_as(concat!(release_select!(), "WHERE r.repo_id = ?1 AND r.tag = ?2"))
            .bind(repo_id)
            .bind(tag)
            .fetch_optional(&self.db)
            .await?)
    }

    /// The newest published release that isn't a pre-release.
    pub async fn latest_release(&self, repo_id: i64) -> Result<Option<Release>> {
        Ok(sqlx::query_as(concat!(
            release_select!(),
            "WHERE r.repo_id = ?1 AND NOT r.draft AND NOT r.prerelease ORDER BY r.published_at DESC, r.id DESC LIMIT 1"
        ))
        .bind(repo_id)
        .fetch_optional(&self.db)
        .await?)
    }

    pub async fn release_count(&self, repo_id: i64) -> Result<i64> {
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM releases WHERE repo_id = ?1 AND NOT draft")
            .bind(repo_id)
            .fetch_one(&self.db)
            .await?;
        Ok(n)
    }

    /// Creates a release for `tag`. If the tag doesn't exist it's created
    /// at `target` (a branch or commit) on publishing.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_release(
        &self,
        repo: &Repo,
        author: &User,
        tag: &str,
        target: &str,
        title: &str,
        notes: &str,
        draft: bool,
        prerelease: bool,
    ) -> Result<Release> {
        let tag = tag.trim();
        if !keel_git::valid_ref_name(&format!("refs/tags/{tag}")) || tag.len() > 100 {
            return invalid("Enter a valid tag name, like v1.0.0.");
        }
        Self::validate_release(title, notes)?;
        let target = target.trim();
        let (t, tg) = (tag.to_owned(), target.to_owned());
        let (exists, target_ok) = self
            .git(repo.id, move |g| Ok((g.ref_tip(&format!("refs/tags/{t}")).is_some(), g.resolve_rev(&tg).is_some())))
            .await?;
        if !exists {
            if !target_ok {
                return invalid("The tag doesn't exist yet: choose the branch or commit to create it at.");
            }
            if self.is_mirror(repo.id).await? {
                return invalid("Mirrors can only have releases for tags they already have.");
            }
        }
        let inserted = sqlx::query_as::<_, (i64,)>(
            "INSERT INTO releases (repo_id, tag, target, title, notes, draft, prerelease, author_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?7, ?8) RETURNING id",
        )
        .bind(repo.id)
        .bind(tag)
        .bind(if exists { "" } else { target })
        .bind(title.trim())
        .bind(notes.trim_end())
        .bind(prerelease)
        .bind(author.id)
        .bind(now())
        .fetch_one(&self.db)
        .await;
        let (id,) = match inserted {
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
                return invalid(format!("There already is a release for {tag}."));
            }
            r => r?,
        };
        let release = self.release(repo.id, id).await?.ok_or(Error::NotFound)?;
        if !draft {
            return self.publish_release(repo, &release, author).await;
        }
        Ok(release)
    }

    fn validate_release(title: &str, notes: &str) -> Result<()> {
        if title.trim().chars().count() > 200 {
            return invalid("Titles are at most 200 characters.");
        }
        if notes.len() > 100_000 {
            return invalid("The notes are too long.");
        }
        Ok(())
    }

    /// Publishes a draft, creating its tag if needed.
    pub async fn publish_release(&self, repo: &Repo, release: &Release, actor: &User) -> Result<Release> {
        if !release.draft {
            return Ok(release.clone());
        }
        let (tag, target) = (release.tag.clone(), release.target.clone());
        let name = format!("refs/tags/{tag}");
        let tip = self.git(repo.id, {
            let name = name.clone();
            move |g| Ok(g.ref_tip(&name))
        })
        .await?;
        if tip.is_none() {
            let before = self.ref_snapshot(repo.id).await?;
            let commit = self.git(repo.id, move |g| Ok(g.resolve_rev(&target))).await?;
            let Some(commit) = commit else {
                return invalid(format!("{} no longer exists; edit the release to choose where to tag.", release.target));
            };
            self.git(repo.id, move |g| g.update_ref(&name, Some(commit), None)).await?;
            self.after_push(repo.id, Some(actor.id), Some(before)).await?;
        }
        sqlx::query("UPDATE releases SET draft = 0, target = '', published_at = ?2 WHERE id = ?1")
            .bind(release.id)
            .bind(now())
            .execute(&self.db)
            .await?;
        let published = self.release(repo.id, release.id).await?.ok_or(Error::NotFound)?;
        self.record(Activity::Release { release: published.clone(), actor: actor.id, action: ReleaseAction::Published }).await;
        Ok(published)
    }

    /// Changes a release's title, notes and pre-release flag.
    pub async fn edit_release(&self, release: &Release, actor: &User, title: &str, notes: &str, prerelease: bool) -> Result<Release> {
        Self::validate_release(title, notes)?;
        sqlx::query("UPDATE releases SET title = ?2, notes = ?3, prerelease = ?4 WHERE id = ?1")
            .bind(release.id)
            .bind(title.trim())
            .bind(notes.trim_end())
            .bind(prerelease)
            .execute(&self.db)
            .await?;
        let edited = self.release(release.repo_id, release.id).await?.ok_or(Error::NotFound)?;
        if !edited.draft {
            self.record(Activity::Release { release: edited.clone(), actor: actor.id, action: ReleaseAction::Edited }).await;
        }
        Ok(edited)
    }

    /// Deletes a release and its files (the tag stays).
    pub async fn delete_release(&self, release: &Release, actor: &User) -> Result<()> {
        let assets = self.release_assets(release.id).await?;
        if !release.draft {
            self.record(Activity::Release { release: release.clone(), actor: actor.id, action: ReleaseAction::Deleted }).await;
        }
        sqlx::query("DELETE FROM releases WHERE id = ?1").bind(release.id).execute(&self.db).await?;
        for a in assets {
            let _ = tokio::fs::remove_file(self.release_asset_path(a.id)).await;
        }
        Ok(())
    }

    // -- Files ----------------------------------------------------------------

    pub async fn release_assets(&self, release_id: i64) -> Result<Vec<ReleaseAsset>> {
        Ok(sqlx::query_as(
            "SELECT id, release_id, name, size, content_type, downloads, created_at
             FROM release_assets WHERE release_id = ?1 ORDER BY name",
        )
        .bind(release_id)
        .fetch_all(&self.db)
        .await?)
    }

    pub async fn release_asset(&self, release_id: i64, name: &str) -> Result<Option<ReleaseAsset>> {
        Ok(sqlx::query_as(
            "SELECT id, release_id, name, size, content_type, downloads, created_at
             FROM release_assets WHERE release_id = ?1 AND name = ?2",
        )
        .bind(release_id)
        .bind(name)
        .fetch_optional(&self.db)
        .await?)
    }

    /// Attaches the file at `tmp` (moved into the store) as `name`.
    pub async fn add_release_asset(
        &self,
        release: &Release,
        uploader: &User,
        name: &str,
        content_type: &str,
        tmp: &Path,
        size: u64,
    ) -> Result<ReleaseAsset> {
        let name = name.trim();
        validate_asset_name(name)?;
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM release_assets WHERE release_id = ?1")
            .bind(release.id)
            .fetch_one(&self.db)
            .await?;
        if count >= MAX_ASSETS {
            return invalid(format!("A release can have at most {MAX_ASSETS} files."));
        }
        let content_type = if content_type.is_empty() || content_type.len() > 100 || content_type.contains(['\r', '\n']) {
            "application/octet-stream"
        } else {
            content_type
        };
        let inserted = sqlx::query_as::<_, (i64,)>(
            "INSERT INTO release_assets (release_id, name, size, content_type, uploader_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) RETURNING id",
        )
        .bind(release.id)
        .bind(name)
        .bind(size as i64)
        .bind(content_type)
        .bind(uploader.id)
        .bind(now())
        .fetch_one(&self.db)
        .await;
        let (id,) = match inserted {
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
                return invalid(format!("This release already has a file named {name}."));
            }
            r => r?,
        };
        let path = self.release_asset_path(id);
        let moved = async {
            tokio::fs::create_dir_all(path.parent().expect("has a parent")).await?;
            tokio::fs::rename(tmp, &path).await
        }
        .await;
        if let Err(e) = moved {
            sqlx::query("DELETE FROM release_assets WHERE id = ?1").bind(id).execute(&self.db).await?;
            return Err(anyhow::Error::from(e).into());
        }
        self.release_asset(release.id, name).await?.ok_or(Error::NotFound)
    }

    /// The release a file belongs to.
    pub async fn release_of_asset(&self, asset_id: i64) -> Result<Option<i64>> {
        let row: Option<(i64,)> = sqlx::query_as("SELECT release_id FROM release_assets WHERE id = ?1")
            .bind(asset_id)
            .fetch_optional(&self.db)
            .await?;
        Ok(row.map(|(r,)| r))
    }

    pub async fn delete_release_asset(&self, release: &Release, asset_id: i64) -> Result<()> {
        sqlx::query("DELETE FROM release_assets WHERE release_id = ?1 AND id = ?2")
            .bind(release.id)
            .bind(asset_id)
            .execute(&self.db)
            .await?;
        let _ = tokio::fs::remove_file(self.release_asset_path(asset_id)).await;
        Ok(())
    }

    pub async fn count_download(&self, asset_id: i64) -> Result<()> {
        sqlx::query("UPDATE release_assets SET downloads = downloads + 1 WHERE id = ?1")
            .bind(asset_id)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    /// Deletes files of releases that are gone (e.g. with their
    /// repository).
    pub async fn prune_release_assets(&self) -> Result<usize> {
        let dir = self.config.data_dir.join("releases");
        if !dir.is_dir() {
            return Ok(0);
        }
        let ids: std::collections::HashSet<i64> = sqlx::query_as::<_, (i64,)>("SELECT id FROM release_assets")
            .fetch_all(&self.db)
            .await?
            .into_iter()
            .map(|(i,)| i)
            .collect();
        let mut removed = 0;
        for entry in std::fs::read_dir(&dir).map_err(anyhow::Error::from)? {
            let entry = entry.map_err(anyhow::Error::from)?;
            let keep = entry.file_name().to_str().and_then(|n| n.parse().ok()).is_some_and(|id: i64| ids.contains(&id));
            if !keep {
                // Uploads in progress are named differently and are recent.
                let old = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|e| e.as_secs() > 86_400);
                if entry.file_name().to_string_lossy().parse::<i64>().is_ok() || old {
                    let _ = std::fs::remove_file(entry.path());
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[tokio::test]
    async fn releases_and_files() {
        let (core, dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let repo = core.create_repo(&ada, "engine", "", false).await.unwrap();
        let work = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let out = Command::new("git").args(args).current_dir(work.path()).env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_AUTHOR_NAME", "Ada").env("GIT_AUTHOR_EMAIL", "a@x").env("GIT_COMMITTER_NAME", "Ada")
                .env("GIT_COMMITTER_EMAIL", "a@x").output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(work.path().join("a"), "1").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "first"]);
        git(&["tag", "v0.9"]);
        git(&["push", "-q", core.repo_path(&repo).to_str().unwrap(), "main", "v0.9"]);

        assert!(core.create_release(&repo, &ada, "bad tag", "main", "", "", false, false).await.is_err());
        assert!(core.create_release(&repo, &ada, "v1.0", "nope", "", "", false, false).await.is_err(), "no target");
        let old = core.create_release(&repo, &ada, "v0.9", "", "", "Old notes", false, false).await.unwrap();
        assert!(!old.draft && old.published_at.is_some());
        assert_eq!(old.name(), "v0.9");

        // A draft for a new tag: the tag appears on publishing.
        let draft = core.create_release(&repo, &ada, "v1.0", "main", "One", "Notes", true, false).await.unwrap();
        assert!(draft.draft);
        assert!(core.git(repo.id, |g| Ok(g.ref_tip("refs/tags/v1.0"))).await.unwrap().is_none());
        assert_eq!(core.releases(repo.id, false, 0, 10).await.unwrap().len(), 1, "drafts hidden");
        assert_eq!(core.releases(repo.id, true, 0, 10).await.unwrap()[0].tag, "v1.0", "drafts first");
        assert!(core.create_release(&repo, &ada, "v1.0", "main", "", "", true, false).await.is_err(), "one per tag");
        let one = core.publish_release(&repo, &draft, &ada).await.unwrap();
        assert!(core.git(repo.id, |g| Ok(g.ref_tip("refs/tags/v1.0"))).await.unwrap().is_some());
        assert_eq!(core.latest_release(repo.id).await.unwrap().unwrap().tag, "v1.0");
        let edited = core.edit_release(&one, &ada, "One!", "Better", true).await.unwrap();
        assert!(edited.prerelease);
        assert_eq!(core.latest_release(repo.id).await.unwrap().unwrap().tag, "v0.9", "pre-releases aren't latest");
        assert_eq!(core.release_count(repo.id).await.unwrap(), 2);

        // Files.
        let tmp = dir.path().join("upload");
        std::fs::write(&tmp, "binary").unwrap();
        let asset = core.add_release_asset(&one, &ada, "engine.tar.gz", "application/gzip", &tmp, 6).await.unwrap();
        assert_eq!(std::fs::read(core.release_asset_path(asset.id)).unwrap(), b"binary");
        std::fs::write(&tmp, "again").unwrap();
        assert!(core.add_release_asset(&one, &ada, "engine.tar.gz", "", &tmp, 5).await.is_err(), "same name");
        assert!(core.add_release_asset(&one, &ada, "../x", "", &tmp, 5).await.is_err());
        core.count_download(asset.id).await.unwrap();
        assert_eq!(core.release_assets(one.id).await.unwrap()[0].downloads, 1);

        // Deleting a release deletes its files; deleting a repository
        // leaves files for pruning.
        core.delete_release(&one, &ada).await.unwrap();
        assert!(!core.release_asset_path(asset.id).exists());
        let tmp2 = dir.path().join("upload2");
        std::fs::write(&tmp2, "x").unwrap();
        let a2 = core.add_release_asset(&old, &ada, "x.bin", "", &tmp2, 1).await.unwrap();
        core.delete_repo(&repo).await.unwrap();
        // Deleting prunes in the background too; either way it's gone.
        core.prune_release_assets().await.unwrap();
        assert!(!core.release_asset_path(a2.id).exists());
    }
}
