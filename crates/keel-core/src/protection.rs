//! Branch protection: rules for branches matching a pattern, enforced when
//! merging pull requests (required approvals) and on pushes (by a
//! `pre-receive` hook, see [`pre_receive`]).

use std::io::BufRead;
use std::path::Path;

use serde::{Deserialize, Serialize};

use std::collections::HashMap;

use keel_git::Oid;

use crate::reviews::Verdict;
use crate::{Access, Config, Core, Issue, MailConfig, PullRequest, Repo, Result, Verification, invalid, now, pull_ref};

const MAX_RULES: usize = 50;

/// Environment variables the hook gets from the server.
const RULES_ENV: &str = "KEEL_PROTECTION";
const EXE_ENV: &str = "KEEL_EXE";
/// For rules requiring verified commits: the data directory, whose
/// database the hook reads keys from.
const DATA_ENV: &str = "KEEL_DATA_DIR";
/// Set when the server sends no mail, so unverified emails count (see
/// `commit_authors`).
const TRUST_EMAILS_ENV: &str = "KEEL_TRUST_EMAILS";
/// Most commits one push may bring to a branch requiring verified ones.
const MAX_VERIFY: usize = 10_000;

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
    /// Allow force pushes, and deleting the branch unless `require_pull`.
    pub allow_force_push: bool,
    /// Status contexts that must succeed before merging, comma-separated.
    pub required_checks: String,
    /// Every commit the branch gets must be verified (see `signatures`).
    /// Commits keel makes are, as it signs them.
    #[serde(default)]
    pub require_signed: bool,
}

impl BranchRule {
    pub fn matches(&self, branch: &str) -> bool {
        glob_match(&self.pattern, branch)
    }

    pub fn checks(&self) -> Vec<&str> {
        self.required_checks.split(',').map(str::trim).filter(|c| !c.is_empty()).collect()
    }
}

/// Normalizes a comma- or newline-separated list of contexts.
fn normalize_checks(s: &str) -> Result<String> {
    let mut checks: Vec<&str> = Vec::new();
    for c in s.split([',', '\n']).map(str::trim).filter(|c| !c.is_empty()) {
        if c.chars().count() > 100 || c.chars().any(char::is_control) {
            return invalid("Check names are at most 100 characters.");
        }
        if !checks.contains(&c) {
            checks.push(c);
        }
    }
    if checks.len() > 20 {
        return invalid("A rule can require at most 20 checks.");
    }
    Ok(checks.join(","))
}

/// `*` matches any run of characters other than `/`. As it never matches
/// `/`, the pattern and the name match segment by segment.
fn glob_match(pattern: &str, name: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == name;
    }
    let (mut patterns, mut names) = (pattern.split('/'), name.split('/'));
    loop {
        match (patterns.next(), names.next()) {
            (None, None) => return true,
            (Some(p), Some(n)) if segment_match(p, n) => {}
            _ => return false,
        }
    }
}

/// Matches one segment without backtracking into earlier stars: on a
/// mismatch, only the last star takes one more character. Never more than
/// `pattern.len() * name.len()` steps, without recursion.
fn segment_match(pattern: &str, name: &str) -> bool {
    let (p, n): (Vec<char>, Vec<char>) = (pattern.chars().collect(), name.chars().collect());
    let (mut i, mut j) = (0, 0);
    // After the last star: where the pattern goes on, and where in `name`
    // the rest of the pattern is tried.
    let mut star = None;
    while j < n.len() {
        if p.get(i) == Some(&'*') {
            i += 1;
            star = Some((i, j));
        } else if p.get(i) == Some(&n[j]) {
            i += 1;
            j += 1;
        } else if let Some((after, from)) = star {
            i = after;
            j = from + 1;
            star = Some((after, j));
        } else {
            return false;
        }
    }
    p[i..].iter().all(|&c| c == '*')
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
    /// The rule's required checks and their state on the head commit
    /// (`None`: not reported yet).
    pub checks: Vec<(String, Option<String>)>,
}

impl MergeCheck {
    pub fn required(&self) -> i64 {
        self.rule.as_ref().map_or(0, |r| r.required_approvals)
    }

    /// Why merging is blocked, if it is.
    pub fn blocked(&self) -> Option<String> {
        let failing: Vec<String> = self
            .checks
            .iter()
            .filter(|(_, state)| state.as_deref() != Some("success"))
            .map(|(context, state)| match state.as_deref() {
                Some("pending") => format!("{context} is pending"),
                Some(_) => format!("{context} failed"),
                None => format!("{context} hasn't reported"),
            })
            .collect();
        if !failing.is_empty() {
            return Some(format!("Required checks: {}.", failing.join(", ")));
        }
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
            "SELECT id, pattern, require_pull, required_approvals, dismiss_stale, allow_force_push, required_checks,
                require_signed
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
        let checks = normalize_checks(&rule.required_checks)?;
        if rule.id == 0 && self.branch_rules(repo_id).await?.len() >= MAX_RULES {
            return invalid(format!("A repository can have at most {MAX_RULES} rules."));
        }
        let result = if rule.id == 0 {
            sqlx::query(
                "INSERT INTO branch_protections
                    (repo_id, pattern, require_pull, required_approvals, dismiss_stale, allow_force_push, created_at,
                     required_checks, require_signed)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )
            .bind(repo_id)
            .bind(pattern)
            .bind(rule.require_pull)
            .bind(rule.required_approvals)
            .bind(rule.dismiss_stale)
            .bind(rule.allow_force_push)
            .bind(now())
            .bind(&checks)
            .bind(rule.require_signed)
            .execute(&self.db)
            .await
        } else {
            sqlx::query(
                "UPDATE branch_protections SET pattern = ?3, require_pull = ?4, required_approvals = ?5,
                    dismiss_stale = ?6, allow_force_push = ?7, required_checks = ?8, require_signed = ?9
                 WHERE repo_id = ?1 AND id = ?2",
            )
            .bind(repo_id)
            .bind(rule.id)
            .bind(pattern)
            .bind(rule.require_pull)
            .bind(rule.required_approvals)
            .bind(rule.dismiss_stale)
            .bind(rule.allow_force_push)
            .bind(&checks)
            .bind(rule.require_signed)
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
        let required: Vec<String> = rule.as_ref().map(|r| r.checks().into_iter().map(str::to_owned).collect()).unwrap_or_default();
        let checks = if required.is_empty() {
            Vec::new()
        } else {
            let statuses = match head {
                Some(head) => self.statuses(repo.id, head).await?,
                None => Vec::new(),
            };
            required
                .into_iter()
                .map(|context| {
                    let state = statuses.iter().find(|s| s.context == context).map(|s| s.state.clone());
                    (context, state)
                })
                .collect()
        };
        let mut check = MergeCheck { rule, approvals: 0, changes_requested: Vec::new(), checks };
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
        Ok(if delete { may_delete(&rule) } else { via_pull || !rule.require_pull })
    }

    /// Settings for `git receive-pack` into `repo_id` so the hook enforces
    /// its branch rules: environment variables to set. Empty without
    /// rules.
    pub async fn receive_pack_env(&self, repo_id: i64) -> Result<Vec<(String, String)>> {
        let rules = self.branch_rules(repo_id).await?;
        if rules.is_empty() {
            return Ok(self.push_limits_env(&[]));
        }
        let exe = std::env::current_exe().map_err(anyhow::Error::from)?;
        let hooks = self.config.hooks_dir().to_string_lossy().into_owned();
        let mut env = vec![
            (RULES_ENV.into(), serde_json::to_string(&rules).map_err(anyhow::Error::from)?),
            (EXE_ENV.into(), exe.to_string_lossy().into_owned()),
        ];
        env.extend(self.push_limits_env(&[("core.hooksPath", &hooks)]));
        if rules.iter().any(|r| r.require_signed) {
            env.push((DATA_ENV.into(), self.config.data_dir.to_string_lossy().into_owned()));
            if !self.mail_enabled() {
                env.push((TRUST_EMAILS_ENV.into(), "1".into()));
            }
        }
        Ok(env)
    }

    /// Environment for any `git receive-pack`: the push size limit, and
    /// `extra` settings, as `-c key=value` options in the environment (git
    /// 2.31+).
    pub(crate) fn push_limits_env(&self, extra: &[(&str, &str)]) -> Vec<(String, String)> {
        let limit = self.config.max_push_bytes.to_string();
        let config: Vec<(&str, &str)> = std::iter::once(("receive.maxInputSize", limit.as_str())).chain(extra.iter().copied()).collect();
        let mut env = vec![("GIT_CONFIG_COUNT".to_owned(), config.len().to_string())];
        for (i, (key, value)) in config.iter().enumerate() {
            env.push((format!("GIT_CONFIG_KEY_{i}"), (*key).to_owned()));
            env.push((format!("GIT_CONFIG_VALUE_{i}"), (*value).to_owned()));
        }
        env
    }

    /// Why `branch` (requiring verified commits) can't get the commits
    /// from `base` to `head`, if it can't: the first ones that aren't
    /// verified.
    pub(crate) async fn unverified(&self, repo_id: i64, branch: &str, base: Oid, head: Oid) -> Result<Option<String>> {
        let ids = self.git(repo_id, move |g| g.commit_ids_between(base, head, MAX_VERIFY + 1)).await?;
        let signed = self.git(repo_id, {
            let ids = ids.clone();
            move |g| g.signed_commits(&ids)
        })
        .await?;
        let found = self.verify_signed(signed).await?;
        Ok(unverified_message(branch, &ids, &found))
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

/// Whether branches under `rule` may be deleted. Not when they need pull
/// requests, even if force pushes are allowed: deleting and pushing anew
/// would replace the branch without one.
fn may_delete(rule: &BranchRule) -> bool {
    rule.allow_force_push && !rule.require_pull
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
        (Some(_), None) if !may_delete(rule) => Some(format!("{branch} is protected and can't be deleted.")),
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
    // Branches requiring verified commits that get new ones: name, old
    // and new tip.
    let mut to_verify: Vec<(String, Option<String>, String)> = Vec::new();
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { return 1 };
        let mut parts = line.split(' ');
        let (Some(old), Some(new), Some(name)) = (parts.next(), parts.next(), parts.next()) else { continue };
        let (old, new) = ((!zero(old)).then_some(old), (!zero(new)).then_some(new));
        let ff = || {
            let (Some(old), Some(new)) = (old, new) else { return false };
            // Without replace refs: one could give `new` a fake parent
            // `old` and make any rewrite pass as a fast-forward.
            keel_git::command()
                .args(["merge-base", "--is-ancestor", old, new])
                .status()
                .is_ok_and(|s| s.success())
        };
        if let Some(why) = check_update(&rules, name, old, new, ff) {
            eprintln!("keel: {why}");
            refused = true;
        }
        if let (Some(branch), Some(new)) = (name.strip_prefix("refs/heads/"), new)
            && rule_for(&rules, branch).is_some_and(|r| r.require_signed)
        {
            to_verify.push((branch.to_owned(), old.map(str::to_owned), new.to_owned()));
        }
    }
    if !refused && !to_verify.is_empty() {
        match verify_pushed(&rules, &to_verify) {
            Ok(problems) => {
                for why in &problems {
                    eprintln!("keel: {why}");
                }
                refused = !problems.is_empty();
            }
            Err(e) => {
                eprintln!("keel: couldn't check the commits' signatures: {e}");
                refused = true;
            }
        }
    }
    i32::from(refused)
}

/// For the hook: why the pushed commits can't go to branches requiring
/// verified ones. A branch gets the commits from its old tip to the new
/// one; a new branch, those that aren't on any other branch requiring
/// verified commits yet.
fn verify_pushed(rules: &[BranchRule], updates: &[(String, Option<String>, String)]) -> anyhow::Result<Vec<String>> {
    let git = |args: &[&str]| -> anyhow::Result<String> {
        let out = keel_git::command().args(args).output()?;
        anyhow::ensure!(out.status.success(), "git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let tips = git(&["for-each-ref", "--format=%(objectname) %(refname:lstrip=2)", "refs/heads/"])?;
    let mut per_branch = Vec::new();
    for (branch, old, new) in updates {
        let mut args = vec!["rev-list".to_owned(), format!("--max-count={}", MAX_VERIFY + 1), new.clone()];
        match old {
            Some(old) => args.push(format!("^{old}")),
            None => {
                let others = tips.lines().filter_map(|l| l.split_once(' ')).filter(|(_, b)| {
                    b != branch && rule_for(rules, b).is_some_and(|r| r.require_signed)
                });
                args.extend(others.map(|(tip, _)| format!("^{tip}")));
            }
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let ids = git(&args)?.lines().map(Oid::from_str).collect::<Result<Vec<_>, _>>()?;
        per_branch.push((branch, ids));
    }
    // The commits are still in quarantine: only git's environment finds them.
    let repo = keel_git::GitRepo::open_from_env()?;
    let all: Vec<Oid> = per_branch.iter().flat_map(|(_, ids)| ids.iter().copied()).collect();
    let signed = repo.signed_commits(&all)?;
    let data_dir = std::env::var_os(DATA_ENV).ok_or_else(|| anyhow::anyhow!("{DATA_ENV} isn't set"))?;
    // Only whether mail is configured matters here.
    let mail = std::env::var_os(TRUST_EMAILS_ENV).is_none().then(|| MailConfig { smtp_url: String::new(), from: String::new() });
    let config = Config { data_dir: data_dir.into(), mail, ..Config::default() };
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let found = runtime.block_on(async { Core::open_read_only(config).await?.verify_signed(signed).await.map_err(anyhow::Error::from) })?;
    Ok(per_branch.into_iter().filter_map(|(branch, ids)| unverified_message(branch, &ids, &found)).collect())
}

/// Why `branch` can't get the commits `ids`, if some aren't verified.
fn unverified_message(branch: &str, ids: &[Oid], found: &HashMap<Oid, Verification>) -> Option<String> {
    if ids.len() > MAX_VERIFY {
        return Some(format!("{branch} requires verified commits, and that's more than {MAX_VERIFY} new ones to check."));
    }
    let bad: Vec<String> = ids
        .iter()
        .filter_map(|id| match found.get(id) {
            Some(v) if v.verified => None,
            Some(v) => Some(format!("{} ({})", &id.to_string()[..10], v.reason.replace('_', " "))),
            None => Some(format!("{} (unsigned)", &id.to_string()[..10])),
        })
        .collect();
    if bad.is_empty() {
        return None;
    }
    let more = if bad.len() > 5 { format!(" and {} more", bad.len() - 5) } else { String::new() };
    Some(format!(
        "{branch} requires verified commits; not verified: {}{more}.",
        bad.iter().take(5).cloned().collect::<Vec<_>>().join(", ")
    ))
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
            required_checks: String::new(),
            require_signed: false,
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
        assert!(glob_match("release/*", "release/v1-β"));
        assert!(glob_match("*", "β"));
        assert!(glob_match("*β*", "αβγ"));
        assert!(!glob_match("*-β", "α-βx"));
        assert!(glob_match("a*b*c", "aXbYbZc"));
        assert!(!glob_match("a*b*c", "aXbYbZ"));
        assert!(glob_match("**", ""));
        assert!(!glob_match("*/*", "a"));
        let started = std::time::Instant::now();
        assert!(!glob_match(&("a*".repeat(50) + "b"), &"a".repeat(200)));
        assert!(!glob_match(&"*a".repeat(100), &"a".repeat(99)));
        assert!(started.elapsed() < std::time::Duration::from_millis(500), "{:?}", started.elapsed());

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
        let rules = [
            rule("main", true, false),
            rule("dev", false, false),
            rule("wild", false, true),
            rule("reviewed", true, true),
        ];
        let check = |r: &str, old, new, ff: bool| check_update(&rules, r, old, new, || ff);
        assert!(check("refs/heads/main", None, b, true).is_none(), "creating is fine");
        assert!(check("refs/heads/main", a, b, true).unwrap().contains("pull request"));
        assert!(check("refs/heads/main", a, None, true).unwrap().contains("deleted"));
        assert!(check("refs/heads/dev", a, b, true).is_none());
        assert!(check("refs/heads/dev", a, b, false).unwrap().contains("force"));
        assert!(check("refs/heads/wild", a, b, false).is_none());
        assert!(check("refs/heads/wild", a, None, false).is_none());
        assert!(check("refs/heads/other", a, None, false).is_none());
        // Deleting and pushing anew would skip the pull requests.
        assert!(check("refs/heads/reviewed", a, None, false).unwrap().contains("deleted"));
        assert!(check("refs/heads/reviewed", None, b, false).is_none(), "creating still is");
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

        // Required checks: on the head commit, all must succeed.
        let head = "c".repeat(40);
        let mut with_checks = BranchRule { required_approvals: 0, required_checks: "ci/build,\n lint, ci/build".into(), ..r.clone() };
        core.save_branch_rule(repo.id, &with_checks).await.unwrap();
        with_checks = core.branch_rules(repo.id).await.unwrap().remove(0);
        assert_eq!(with_checks.checks(), ["ci/build", "lint"]);
        let blocked = |c: MergeCheck| c.blocked().unwrap_or_default();
        let msg = blocked(core.merge_check(&repo, &issue, &pull, Some(&head)).await.unwrap());
        assert_eq!(msg, "Required checks: ci/build hasn't reported, lint hasn't reported.");
        core.set_status(&repo, &ada, &head, "failure", "ci/build", "", "").await.unwrap();
        core.set_status(&repo, &ada, &head, "pending", "lint", "", "").await.unwrap();
        let msg = blocked(core.merge_check(&repo, &issue, &pull, Some(&head)).await.unwrap());
        assert_eq!(msg, "Required checks: ci/build failed, lint is pending.");
        core.set_status(&repo, &ada, &head, "success", "ci/build", "", "").await.unwrap();
        core.set_status(&repo, &ada, &head, "success", "lint", "", "").await.unwrap();
        assert_eq!(core.merge_check(&repo, &issue, &pull, Some(&head)).await.unwrap().blocked(), None);
        assert!(core.merge_check(&repo, &issue, &pull, Some(&"d".repeat(40))).await.unwrap().blocked().is_some(), "per commit");

        core.delete_branch_rule(repo.id, r.id).await.unwrap();
        let env = core.receive_pack_env(repo.id).await.unwrap();
        assert!(!env.iter().any(|(k, _)| k == RULES_ENV), "no rules, no hook");
        assert!(env.iter().any(|(k, v)| k == "GIT_CONFIG_KEY_0" && v == "receive.maxInputSize"), "but a size limit");
    }
}
