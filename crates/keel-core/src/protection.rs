//! Branch protection: rules for branches matching a pattern, enforced when
//! merging pull requests (required approvals) and on pushes (by a
//! `pre-receive` hook, see [`pre_receive`]).

use std::io::BufRead;
use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::reviews::Verdict;
use crate::{Access, Core, Issue, PullRequest, Repo, Result, invalid, now, pull_ref};

const MAX_RULES: usize = 50;

/// Environment variables the hook gets from the server.
const RULES_ENV: &str = "KEEL_PROTECTION";
const EXE_ENV: &str = "KEEL_EXE";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct BranchRule {
    pub id: i64,
    /// A branch name, or a glob: `*` matches anything but `/`.
    pub pattern: String,
    /// Changes must go through pull requests; pushing to the branch is
    /// refused (creating it isn't).
    pub require_pull: bool,
    /// Approvals from people with write access needed to merge.
    pub required_approvals: i64,
    /// Reviews only count for the commit they were given on.
    pub dismiss_stale: bool,
    /// Allow force pushes and deleting the branch.
    pub allow_force_push: bool,
}

impl BranchRule {
    pub fn matches(&self, branch: &str) -> bool {
        glob_match(&self.pattern, branch)
    }
}

/// `*` matches any run of characters other than `/`.
fn glob_match(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == name,
        Some((prefix, rest)) => {
            let Some(name) = name.strip_prefix(prefix) else { return false };
            // Try every split of `name` at which the star's match ends.
            let stop = name.find('/').unwrap_or(name.len());
            (0..=stop).any(|i| glob_match(rest, &name[i..]))
        }
    }
}

/// The rule for `branch`: an exact match, otherwise the longest matching
/// pattern.
pub fn rule_for<'a>(rules: &'a [BranchRule], branch: &str) -> Option<&'a BranchRule> {
    rules.iter().find(|r| r.pattern == branch).or_else(|| {
        rules
            .iter()
            .filter(|r| r.matches(branch))
            .max_by_key(|r| r.pattern.len())
    })
}

fn validate_pattern(p: &str) -> Result<()> {
    let ok = !p.is_empty()
        && p.len() <= 200
        && !p.starts_with('/')
        && !p.ends_with('/')
        && !p.contains("//")
        && !p.contains("..")
        && !p.starts_with("refs/")
        && p.chars().all(|c| !c.is_control() && !" ~^:?[\\".contains(c));
    if !ok {
        return invalid("Enter a branch name like main, or a pattern like release/*.");
    }
    Ok(())
}

/// Whether a pull request may be merged under its base branch's rule, and
/// if not, why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeCheck {
    pub rule: Option<BranchRule>,
    /// Approvals that count.
    pub approvals: i64,
    /// People with write access whose latest review requests changes.
    pub changes_requested: Vec<String>,
}

impl MergeCheck {
    pub fn required(&self) -> i64 {
        self.rule.as_ref().map_or(0, |r| r.required_approvals)
    }

    /// Why merging is blocked, if it is.
    pub fn blocked(&self) -> Option<String> {
        if !self.changes_requested.is_empty() {
            return Some(format!("{} requested changes.", self.changes_requested.join(", ")));
        }
        let (need, have) = (self.required(), self.approvals);
        if have < need {
            let s = if need == 1 { "" } else { "s" };
            return Some(format!("{need} approving review{s} from people with write access required ({have} so far)."));
        }
        None
    }
}

impl Core {
    pub async fn branch_rules(&self, repo_id: i64) -> Result<Vec<BranchRule>> {
        Ok(sqlx::query_as(
            "SELECT id, pattern, require_pull, required_approvals, dismiss_stale, allow_force_push
             FROM branch_protections WHERE repo_id = ?1 ORDER BY pattern",
        )
        .bind(repo_id)
        .fetch_all(&self.db)
        .await?)
    }

    pub async fn branch_rule(&self, repo_id: i64, branch: &str) -> Result<Option<BranchRule>> {
        Ok(rule_for(&self.branch_rules(repo_id).await?, branch).cloned())
    }

    /// Creates a rule (`rule.id` 0) or updates rule `rule.id`.
    pub async fn save_branch_rule(&self, repo_id: i64, rule: &BranchRule) -> Result<()> {
        let pattern = rule.pattern.trim();
        validate_pattern(pattern)?;
        if !(0..=10).contains(&rule.required_approvals) {
            return invalid("Required approvals are 0–10.");
        }
        if rule.id == 0 && self.branch_rules(repo_id).await?.len() >= MAX_RULES {
            return invalid(format!("A repository can have at most {MAX_RULES} rules."));
        }
        let result = if rule.id == 0 {
            sqlx::query(
                "INSERT INTO branch_protections
                    (repo_id, pattern, require_pull, required_approvals, dismiss_stale, allow_force_push, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )
            .bind(repo_id)
            .bind(pattern)
            .bind(rule.require_pull)
            .bind(rule.required_approvals)
            .bind(rule.dismiss_stale)
            .bind(rule.allow_force_push)
            .bind(now())
            .execute(&self.db)
            .await
        } else {
            sqlx::query(
                "UPDATE branch_protections SET pattern = ?3, require_pull = ?4, required_approvals = ?5,
                    dismiss_stale = ?6, allow_force_push = ?7
                 WHERE repo_id = ?1 AND id = ?2",
            )
            .bind(repo_id)
            .bind(rule.id)
            .bind(pattern)
            .bind(rule.require_pull)
            .bind(rule.required_approvals)
            .bind(rule.dismiss_stale)
            .bind(rule.allow_force_push)
            .execute(&self.db)
            .await
        };
        match result {
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
                invalid(format!("There already is a rule for {pattern}."))
            }
            r => r.map(|_| ()).map_err(Into::into),
        }
    }

    pub async fn delete_branch_rule(&self, repo_id: i64, id: i64) -> Result<()> {
        sqlx::query("DELETE FROM branch_protections WHERE repo_id = ?1 AND id = ?2")
            .bind(repo_id)
            .bind(id)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    /// What a pull request needs before it may be merged. `head` is its
    /// current head commit (for discarding stale reviews).
    pub async fn merge_check(&self, repo: &Repo, issue: &Issue, pull: &PullRequest, head: Option<&str>) -> Result<MergeCheck> {
        let rule = self.branch_rule(repo.id, &pull.base_ref).await?;
        let reviews: Vec<(i64, String, String, Option<String>)> = sqlx::query_as(
            "SELECT c.author_id, u.username, c.kind, c.commit_id FROM comments c JOIN users u ON u.id = c.author_id
             WHERE c.id IN (SELECT MAX(id) FROM comments
                            WHERE issue_id = ?1 AND kind IN ('approved', 'changes_requested')
                            GROUP BY author_id)
             ORDER BY c.id",
        )
        .bind(issue.id)
        .fetch_all(&self.db)
        .await?;
        let dismiss_stale = rule.as_ref().is_some_and(|r| r.dismiss_stale);
        let mut check = MergeCheck { rule, approvals: 0, changes_requested: Vec::new() };
        for (user_id, name, kind, commit) in reviews {
            if dismiss_stale && commit.as_deref() != head {
                continue;
            }
            let Some(user) = self.user_by_id(user_id).await? else { continue };
            if issue.is_author(&user) || self.access(repo, Some(&user)).await? < Access::Write {
                continue;
            }
            match Verdict::parse(&kind) {
                Some(Verdict::Approved) => check.approvals += 1,
                Some(Verdict::ChangesRequested) => check.changes_requested.push(name),
                None => {}
            }
        }
        Ok(check)
    }

    /// Whether keel itself (merging, applying suggestions, deleting a
    /// merged branch) may move `branch` of `repo_id` without a pull request
    /// (`via_pull` false), or delete it.
    pub async fn may_update_branch(&self, repo_id: i64, branch: &str, via_pull: bool, delete: bool) -> Result<bool> {
        let Some(rule) = self.branch_rule(repo_id, branch).await? else { return Ok(true) };
        Ok(if delete { rule.allow_force_push } else { via_pull || !rule.require_pull })
    }

    /// Settings for `git receive-pack` into `repo_id` so the hook enforces
    /// its branch rules: environment variables to set. Empty without
    /// rules.
    pub async fn receive_pack_env(&self, repo_id: i64) -> Result<Vec<(String, String)>> {
        let rules = self.branch_rules(repo_id).await?;
        if rules.is_empty() {
            return Ok(Vec::new());
        }
        let exe = std::env::current_exe().map_err(anyhow::Error::from)?;
        let hooks = self.config.hooks_dir();
        Ok(vec![
            (RULES_ENV.into(), serde_json::to_string(&rules).map_err(anyhow::Error::from)?),
            (EXE_ENV.into(), exe.to_string_lossy().into_owned()),
            // `-c core.hooksPath=…` as environment (git 2.31+).
            ("GIT_CONFIG_COUNT".into(), "1".into()),
            ("GIT_CONFIG_KEY_0".into(), "core.hooksPath".into()),
            ("GIT_CONFIG_VALUE_0".into(), hooks.to_string_lossy().into_owned()),
        ])
    }
}

/// Writes the `pre-receive` hook that `receive_pack_env` points git at. It
/// runs this binary's `hook pre-receive` command, see [`pre_receive`].
pub(crate) fn install_hooks(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let script = format!("#!/bin/sh\n# Written by keel; enforces branch protection.\n[ -n \"${EXE_ENV}\" ] || exit 0\nexec \"${EXE_ENV}\" hook pre-receive\n");
    let path = dir.join("pre-receive");
    if std::fs::read_to_string(&path).ok().as_deref() != Some(script.as_str()) {
        std::fs::write(&path, script)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

/// Why one ref update breaks `rules`, if it does. `fast_forward` tells
/// whether `old` is an ancestor of `new`.
pub fn check_update(
    rules: &[BranchRule],
    refname: &str,
    old: Option<&str>,
    new: Option<&str>,
    fast_forward: impl FnOnce() -> bool,
) -> Option<String> {
    let branch = refname.strip_prefix("refs/heads/")?;
    let rule = rule_for(rules, branch)?;
    match (old, new) {
        (None, _) => None,
        (Some(_), None) if !rule.allow_force_push => Some(format!("{branch} is protected and can't be deleted.")),
        (Some(_), None) => None,
        (Some(_), Some(_)) if rule.require_pull => {
            Some(format!("{branch} is protected: changes must be made through a pull request."))
        }
        (Some(_), Some(_)) if !rule.allow_force_push && !fast_forward() => {
            Some(format!("{branch} is protected and can't be force-pushed."))
        }
        _ => None,
    }
}

/// The `pre-receive` hook: reads `<old> <new> <ref>` lines on stdin and
/// refuses the push if one breaks the rules in the environment. Runs inside
/// `git receive-pack`, so `git` sees the pushed (quarantined) objects.
/// Returns the exit code.
pub fn pre_receive() -> i32 {
    let rules: Vec<BranchRule> = match std::env::var(RULES_ENV) {
        Ok(json) => match serde_json::from_str(&json) {
            Ok(rules) => rules,
            Err(e) => {
                eprintln!("keel: bad branch rules: {e}");
                return 1;
            }
        },
        Err(_) => return 0,
    };
    let zero = |id: &str| id.bytes().all(|b| b == b'0');
    let mut refused = false;
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { return 1 };
        let mut parts = line.split(' ');
        let (Some(old), Some(new), Some(name)) = (parts.next(), parts.next(), parts.next()) else { continue };
        let (old, new) = ((!zero(old)).then_some(old), (!zero(new)).then_some(new));
        let ff = || {
            let (Some(old), Some(new)) = (old, new) else { return false };
            Command::new("git")
                .args(["merge-base", "--is-ancestor", old, new])
                .status()
                .is_ok_and(|s| s.success())
        };
        if let Some(why) = check_update(&rules, name, old, new, ff) {
            eprintln!("keel: {why}");
            refused = true;
        }
    }
    i32::from(refused)
}

/// Head commit pinned for a pull request, as a string.
pub(crate) async fn pinned_head(core: &Core, repo_id: i64, number: i64) -> Result<Option<String>> {
    Ok(core
        .git(repo_id, move |g| Ok(g.ref_tip(&pull_ref(number))))
        .await?
        .map(|h| h.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(pattern: &str, require_pull: bool, allow_force_push: bool) -> BranchRule {
        BranchRule {
            id: 1,
            pattern: pattern.into(),
            require_pull,
            required_approvals: 0,
            dismiss_stale: false,
            allow_force_push,
        }
    }

    #[test]
    fn patterns() {
        assert!(glob_match("main", "main"));
        assert!(!glob_match("main", "mainline"));
        assert!(glob_match("release/*", "release/1.0"));
        assert!(!glob_match("release/*", "release/1.0/hotfix"));
        assert!(!glob_match("release/*", "releases/1"));
        assert!(glob_match("*", "anything"));
        assert!(!glob_match("*", "a/b"));
        assert!(glob_match("v*-stable", "v2-stable"));
        assert!(glob_match("*/*", "a/b"));

        let rules = [rule("*", false, true), rule("release/*", true, false), rule("main", true, false)];
        assert_eq!(rule_for(&rules, "main").unwrap().pattern, "main");
        assert_eq!(rule_for(&rules, "topic").unwrap().pattern, "*");
        assert_eq!(rule_for(&rules, "release/2").unwrap().pattern, "release/*");
        assert!(rule_for(&rules, "user/topic").is_none());

        assert!(validate_pattern("release/*").is_ok());
        for bad in ["", "refs/heads/main", "a b", "a..b", "/x", "x/"] {
            assert!(validate_pattern(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn updates() {
        let a = Some("1111");
        let b = Some("2222");
        let rules = [rule("main", true, false), rule("dev", false, false), rule("wild", false, true)];
        let check = |r: &str, old, new, ff: bool| check_update(&rules, r, old, new, || ff);
        assert!(check("refs/heads/main", None, b, true).is_none(), "creating is fine");
        assert!(check("refs/heads/main", a, b, true).unwrap().contains("pull request"));
        assert!(check("refs/heads/main", a, None, true).unwrap().contains("deleted"));
        assert!(check("refs/heads/dev", a, b, true).is_none());
        assert!(check("refs/heads/dev", a, b, false).unwrap().contains("force"));
        assert!(check("refs/heads/wild", a, b, false).is_none());
        assert!(check("refs/heads/wild", a, None, false).is_none());
        assert!(check("refs/heads/other", a, None, false).is_none());
        assert!(check("refs/tags/main", a, None, false).is_none(), "tags aren't branches");
    }

    #[tokio::test]
    async fn rules_and_merge_checks() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let bob = core.register("bob", "bob@example.com", "password1").await.unwrap();
        let eve = core.register("eve", "eve@example.com", "password1").await.unwrap();
        let repo = core.create_repo(&ada, "engine", "", false).await.unwrap();
        core.set_collaborator(&repo, "bob", Access::Write).await.unwrap();

        let mut r = BranchRule { id: 0, required_approvals: 1, dismiss_stale: true, ..rule("main", true, false) };
        core.save_branch_rule(repo.id, &r).await.unwrap();
        assert!(core.save_branch_rule(repo.id, &r).await.is_err(), "duplicate");
        assert!(core.save_branch_rule(repo.id, &BranchRule { pattern: "refs/x".into(), ..r.clone() }).await.is_err());
        r.id = core.branch_rules(repo.id).await.unwrap()[0].id;
        assert!(!core.may_update_branch(repo.id, "main", false, false).await.unwrap());
        assert!(core.may_update_branch(repo.id, "main", true, false).await.unwrap());
        assert!(!core.may_update_branch(repo.id, "main", true, true).await.unwrap());
        assert!(core.may_update_branch(repo.id, "topic", false, true).await.unwrap());
        let env = core.receive_pack_env(repo.id).await.unwrap();
        assert!(env.iter().any(|(k, v)| k == RULES_ENV && v.contains("\"main\"")));

        // A pull request (flagged directly; see reviews.rs).
        let issue = core.create_issue(repo.id, &ada, "Change", "").await.unwrap();
        sqlx::query("UPDATE issues SET is_pull = 1 WHERE id = ?1").bind(issue.id).execute(&core.db).await.unwrap();
        sqlx::query("INSERT INTO pull_requests (issue_id, base_ref, head_ref) VALUES (?1, 'main', 'topic')")
            .bind(issue.id)
            .execute(&core.db)
            .await
            .unwrap();
        let issue = core.issue(repo.id, 1).await.unwrap().unwrap();
        let pull = core.pull(issue.id).await.unwrap().unwrap();
        let review = |user, verdict, commit: &'static str| {
            let (core, issue) = (core.clone(), issue.clone());
            async move {
                let id = core.add_review(&issue, &user, verdict, "").await.unwrap();
                sqlx::query("UPDATE comments SET commit_id = ?2 WHERE id = ?1")
                    .bind(id)
                    .bind(commit)
                    .execute(&core.db)
                    .await
                    .unwrap();
            }
        };
        let check = core.merge_check(&repo, &issue, &pull, Some("c1")).await.unwrap();
        assert!(check.blocked().unwrap().contains("1 approving review"));
        review(eve.clone(), Verdict::Approved, "c1").await;
        assert_eq!(core.merge_check(&repo, &issue, &pull, Some("c1")).await.unwrap().approvals, 0, "eve can't write");
        review(bob.clone(), Verdict::ChangesRequested, "c1").await;
        let check = core.merge_check(&repo, &issue, &pull, Some("c1")).await.unwrap();
        assert_eq!(check.blocked().unwrap(), "bob requested changes.");
        review(bob.clone(), Verdict::Approved, "c1").await;
        assert_eq!(core.merge_check(&repo, &issue, &pull, Some("c1")).await.unwrap().blocked(), None);
        // A new push makes the approval stale, unless that's turned off.
        assert!(core.merge_check(&repo, &issue, &pull, Some("c2")).await.unwrap().blocked().is_some());
        core.save_branch_rule(repo.id, &BranchRule { dismiss_stale: false, ..r.clone() }).await.unwrap();
        assert_eq!(core.merge_check(&repo, &issue, &pull, Some("c2")).await.unwrap().blocked(), None);

        core.delete_branch_rule(repo.id, r.id).await.unwrap();
        assert!(core.receive_pack_env(repo.id).await.unwrap().is_empty());
    }
}
