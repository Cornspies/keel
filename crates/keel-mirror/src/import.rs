//! Imports repositories from GitHub and Gitea: the API client, and turning
//! what it returns into what `keel_core::imports` stores.

use std::collections::HashMap;
use std::path::PathBuf;

use futures_util::StreamExt;
use keel_core::{
    Core, ImportService, ImportedComment, ImportedIssue, ImportedLabel, ImportedMilestone, ImportedPull, ImportedRelease,
};
use serde_json::Value;
use tokio::io::AsyncWriteExt;

use crate::lfs;

/// Items asked for per page (Gitea's usual maximum).
const PER_PAGE: usize = 50;
/// A runaway pagination stops here.
const MAX_PAGES: usize = 4000;

/// The source's API.
struct Api<'a> {
    client: &'a reqwest::Client,
    service: ImportService,
    /// The repository's API, e.g. `https://api.github.com/repos/ada/engine`.
    base: String,
    token: String,
    allow_private: bool,
}

impl Api<'_> {
    fn request(&self, url: &str) -> Result<reqwest::RequestBuilder, String> {
        let parsed = reqwest::Url::parse(url).map_err(|e| e.to_string())?;
        lfs::check_url(&parsed, self.allow_private)?;
        let mut req = self.client.get(parsed.clone()).header("User-Agent", "keel");
        req = match self.service {
            ImportService::GitHub => req.header("Accept", "application/vnd.github+json").header("X-GitHub-Api-Version", "2022-11-28"),
            ImportService::Gitea => req.header("Accept", "application/json"),
        };
        // Only to the source itself, not to URLs it sends (e.g. for files).
        let ours = reqwest::Url::parse(&self.base).is_ok_and(|base| lfs::same_origin(&base, &parsed))
            || (self.service == ImportService::GitHub && parsed.scheme() == "https" && parsed.host_str() == Some("api.github.com"));
        if !self.token.is_empty() && ours {
            let auth = match self.service {
                ImportService::GitHub => format!("Bearer {}", self.token),
                ImportService::Gitea => format!("token {}", self.token),
            };
            req = req.header("Authorization", auth);
        }
        Ok(req)
    }

    async fn get(&self, path: &str, page: Option<usize>) -> Result<Value, String> {
        let mut url = format!("{}{path}", self.base);
        if let Some(page) = page {
            let sep = if url.contains('?') { '&' } else { '?' };
            url.push_str(&format!("{sep}per_page={PER_PAGE}&limit={PER_PAGE}&page={page}"));
        }
        let res = self.request(&url)?.send().await.map_err(|e| format!("{} didn't answer: {e}", self.service.name()))?;
        let status = res.status();
        let rate_limited = res.headers().get("x-ratelimit-remaining").is_some_and(|v| v == "0");
        match status.as_u16() {
            200..=299 => {
                let body = res.text().await.map_err(|e| format!("{} didn't answer: {e}", self.service.name()))?;
                serde_json::from_str(&body).map_err(|e| format!("{} sent something unexpected: {e}", self.service.name()))
            }
            401 => Err("The access token was refused.".into()),
            403 | 429 if rate_limited || status.as_u16() == 429 => Err(format!(
                "{}'s rate limit was reached; try again later, or with an access token.",
                self.service.name()
            )),
            404 => Err(format!(
                "{} doesn't know {path}; for a private repository, give an access token that can read it.",
                self.service.name()
            )),
            code => Err(format!("{} answered {code} for {path}.", self.service.name())),
        }
    }

    /// Every item of a paginated list.
    async fn all(&self, path: &str) -> Result<Vec<Value>, String> {
        let mut out = Vec::new();
        for page in 1..=MAX_PAGES {
            let items = match self.get(path, Some(page)).await? {
                Value::Array(items) => items,
                _ => return Err(format!("{} sent something unexpected for {path}.", self.service.name())),
            };
            let n = items.len();
            out.extend(items);
            if n == 0 {
                break;
            }
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// From the APIs' JSON (GitHub's and Gitea's are alike)
// ---------------------------------------------------------------------------

fn text(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_owned()
}

/// Unix time of an RFC 3339 timestamp.
fn time(v: &Value) -> Option<i64> {
    let s = v.as_str()?;
    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
        .ok()
        .map(|t| t.unix_timestamp())
        .filter(|t| *t > 0)
}

/// A user's profile on the source.
fn author(user: &Value, web: &str) -> String {
    match user["html_url"].as_str().filter(|u| u.starts_with("http")) {
        Some(url) => url.to_owned(),
        None => format!("{web}/{}", user["login"].as_str().filter(|l| !l.is_empty()).unwrap_or("ghost")),
    }
}

fn label(v: &Value) -> ImportedLabel {
    ImportedLabel { name: text(&v["name"]), color: text(&v["color"]), description: text(&v["description"]) }
}

fn milestone(v: &Value) -> ImportedMilestone {
    ImportedMilestone {
        title: text(&v["title"]),
        description: text(&v["description"]),
        due_on: time(&v["due_on"]),
        closed: v["state"] == "closed",
        created_at: time(&v["created_at"]).unwrap_or_default(),
        closed_at: time(&v["closed_at"]),
    }
}

/// An issue or pull request; `pulls` has the pull requests' details.
fn issue(v: &Value, pulls: &HashMap<i64, Value>, web: &str) -> Option<ImportedIssue> {
    let number = v["number"].as_i64()?;
    let is_pull = !v["pull_request"].is_null() || pulls.contains_key(&number) && v.get("head").is_some();
    let pull = is_pull.then(|| {
        let p = pulls.get(&number).unwrap_or(v);
        ImportedPull {
            base_ref: text(&p["base"]["ref"]),
            head_ref: text(&p["head"]["ref"]),
            head_commit: p["head"]["sha"].as_str().map(str::to_owned),
            merge_commit: p["merge_commit_sha"].as_str().filter(|_| !p["merged_at"].is_null()).map(str::to_owned),
            merged_at: time(&p["merged_at"]),
            draft: p["draft"].as_bool().unwrap_or(false),
        }
    });
    let merged = pull.as_ref().is_some_and(|p| p.merged_at.is_some());
    Some(ImportedIssue {
        number,
        title: text(&v["title"]),
        body: text(&v["body"]),
        author: author(&v["user"], web),
        state: if merged { "merged".into() } else { text(&v["state"]) },
        created_at: time(&v["created_at"]).unwrap_or_default(),
        updated_at: time(&v["updated_at"]).unwrap_or_default(),
        closed_at: time(&v["closed_at"]),
        labels: v["labels"].as_array().map(|ls| ls.iter().map(|l| text(&l["name"])).collect()).unwrap_or_default(),
        milestone: v["milestone"]["title"].as_str().map(str::to_owned),
        pull,
    })
}

/// The number at the end of an issue or pull request URL.
fn number_in(url: &Value) -> Option<i64> {
    url.as_str()?.trim_end_matches('/').rsplit('/').next()?.parse().ok()
}

fn comment(v: &Value, web: &str) -> Option<ImportedComment> {
    let number = number_in(&v["issue_url"]).or_else(|| number_in(&v["pull_request_url"]))?;
    let created_at = time(&v["created_at"]).unwrap_or_default();
    Some(ImportedComment {
        issue_number: number,
        kind: "comment".into(),
        body: text(&v["body"]),
        author: author(&v["user"], web),
        created_at,
        updated_at: time(&v["updated_at"]).unwrap_or(created_at),
        commit_id: None,
    })
}

/// A comment on a line of a pull request's diff (GitHub), as a comment
/// quoting the line.
fn line_comment(v: &Value, web: &str) -> Option<ImportedComment> {
    let mut c = comment(&serde_json::json!({
        "pull_request_url": v["pull_request_url"],
        "created_at": v["created_at"],
        "updated_at": v["updated_at"],
        "user": v["user"],
    }), web)?;
    let hunk: Vec<&str> = v["diff_hunk"].as_str().unwrap_or_default().lines().collect();
    let context = hunk[hunk.len().saturating_sub(4)..].join("\n");
    let line = v["line"].as_i64().or_else(|| v["original_line"].as_i64());
    let place = match line {
        Some(n) => format!("**`{}`** line {n}:", text(&v["path"])),
        None => format!("**`{}`**:", text(&v["path"])),
    };
    c.body = format!("{place}\n\n```diff\n{context}\n```\n\n{}", text(&v["body"]));
    Some(c)
}

/// A review, unless it's empty or unfinished.
fn review(v: &Value, number: i64, web: &str) -> Option<ImportedComment> {
    let kind = match v["state"].as_str()? {
        "APPROVED" => "approved",
        "CHANGES_REQUESTED" | "REQUEST_CHANGES" => "changes_requested",
        "COMMENTED" | "COMMENT" if !text(&v["body"]).trim().is_empty() => "comment",
        _ => return None,
    };
    let at = time(&v["submitted_at"]).unwrap_or_default();
    Some(ImportedComment {
        issue_number: number,
        kind: kind.into(),
        body: text(&v["body"]),
        author: author(&v["user"], web),
        created_at: at,
        updated_at: at,
        commit_id: v["commit_id"].as_str().map(str::to_owned),
    })
}

fn release(v: &Value, web: &str) -> ImportedRelease {
    ImportedRelease {
        tag: text(&v["tag_name"]),
        title: text(&v["name"]),
        notes: text(&v["body"]),
        draft: v["draft"].as_bool().unwrap_or(false),
        prerelease: v["prerelease"].as_bool().unwrap_or(false),
        author: author(&v["author"], web),
        created_at: time(&v["created_at"]).unwrap_or_default(),
        published_at: time(&v["published_at"]),
    }
}

// ---------------------------------------------------------------------------
// The import
// ---------------------------------------------------------------------------

fn message(e: keel_core::Error) -> String {
    match e {
        keel_core::Error::Invalid(m) => m,
        e => e.to_string(),
    }
}

/// Runs the import into `repo_id`, recording how it went.
pub async fn run(core: &Core, client: &reqwest::Client, repo_id: i64) {
    let outcome = import(core, client, repo_id).await;
    let error = match &outcome {
        Ok(warnings) if warnings.is_empty() => None,
        Ok(warnings) => Some(format!("Imported, but: {}", warnings.join(" "))),
        Err(e) => Some(e.clone()),
    };
    if let Err(e) = core.finish_import(repo_id, error.as_deref()).await {
        tracing::warn!(repo = repo_id, "recording an import: {e}");
    }
}

/// Imports; returns problems that didn't stop it.
async fn import(core: &Core, client: &reqwest::Client, repo_id: i64) -> Result<Vec<String>, String> {
    let progress = |p: String| async move {
        if let Err(e) = core.set_import_progress(repo_id, &p).await {
            tracing::warn!(repo = repo_id, "import progress: {e}");
        }
    };
    let import = core.import(repo_id).await.map_err(message)?.ok_or("The import is gone.")?;
    let repo = core.repo_by_id(repo_id).await.map_err(message)?.ok_or("The repository is gone.")?;
    let source = keel_core::import_source(&import.url).map_err(message)?;
    let web = source.url.split('/').take(3).collect::<Vec<_>>().join("/");
    let remote = core.import_remote(repo_id, &source.git_url).await.map_err(message)?;
    let mut warnings = Vec::new();

    progress("Fetching the git repository".into()).await;
    let wanted = core.import_git(repo_id, &remote).await.map_err(|e| format!("Fetching the repository failed: {}", message(e)))?;
    if !wanted.is_empty() {
        progress(format!("Downloading {} Git LFS files", wanted.len())).await;
        let (missing, problem) = lfs::download(core, client, repo_id, &remote, &wanted).await;
        if let Some(p) = problem {
            warnings.push(format!("{} Git LFS files couldn't be downloaded ({p}).", missing.len()));
        }
    }
    if import.with_wiki {
        progress("Fetching the wiki".into()).await;
        if let Err(e) = core.import_wiki(&repo, &remote).await {
            warnings.push(format!("The wiki couldn't be imported: {}.", message(e)));
        }
    }

    let api = Api {
        client,
        service: source.service,
        base: source.api_url.clone(),
        token: core.import_token(repo_id).await.map_err(message)?,
        allow_private: core.config.webhook_allow_private,
    };
    if import.with_issues {
        progress("Labels and milestones".into()).await;
        let labels: Vec<ImportedLabel> = api.all("/labels").await?.iter().map(label).collect();
        core.import_labels(repo_id, &labels).await.map_err(message)?;
        for m in api.all("/milestones?state=all").await? {
            core.import_milestone(repo_id, &milestone(&m)).await.map_err(message)?;
        }

        progress("Pull requests".into()).await;
        let pulls: HashMap<i64, Value> =
            api.all("/pulls?state=all").await?.into_iter().filter_map(|p| Some((p["number"].as_i64()?, p))).collect();
        progress("Issues".into()).await;
        let mut issues = api.all("/issues?state=all&sort=created&direction=asc").await?;
        // Pull requests that the issue list left out.
        let listed: Vec<i64> = issues.iter().filter_map(|i| i["number"].as_i64()).collect();
        issues.extend(pulls.iter().filter(|(n, _)| !listed.contains(n)).map(|(_, p)| p.clone()));
        let total = issues.len();
        for (n, v) in issues.iter().enumerate() {
            if n % 50 == 0 {
                progress(format!("Issues and pull requests: {n} of {total}")).await;
            }
            if let Some(i) = issue(v, &pulls, &web) {
                core.import_issue(repo_id, &i).await.map_err(message)?;
            }
        }

        progress("Comments".into()).await;
        let mut comments: Vec<ImportedComment> = api.all("/issues/comments").await?.iter().filter_map(|c| comment(c, &web)).collect();
        let mut numbers: Vec<i64> = pulls.keys().copied().collect();
        numbers.sort_unstable();
        for (n, number) in numbers.iter().enumerate() {
            if n % 20 == 0 {
                progress(format!("Reviews: {n} of {} pull requests", numbers.len())).await;
            }
            match api.all(&format!("/pulls/{number}/reviews")).await {
                Ok(reviews) => comments.extend(reviews.iter().filter_map(|r| review(r, *number, &web))),
                Err(e) => {
                    warnings.push(format!("Reviews of #{number} couldn't be read: {e}"));
                    break;
                }
            }
        }
        if source.service == ImportService::GitHub {
            match api.all("/pulls/comments").await {
                Ok(lines) => comments.extend(lines.iter().filter_map(|c| line_comment(c, &web))),
                Err(e) => warnings.push(format!("Comments on lines of pull requests couldn't be read: {e}")),
            }
        }
        // In the order they were written, which is how they're shown.
        comments.sort_by_key(|c| c.created_at);
        for c in &comments {
            core.import_comment(repo_id, c).await.map_err(message)?;
        }
        core.finish_imported_pulls(repo_id).await.map_err(message)?;
    }

    if import.with_releases {
        progress("Releases".into()).await;
        let uploader = match import.created_by {
            Some(id) => core.user_by_id(id).await.map_err(message)?,
            None => None,
        };
        let mut releases = api.all("/releases").await?;
        releases.reverse(); // oldest first
        for v in &releases {
            let Some(r) = core.import_release(repo_id, &release(v, &web)).await.map_err(message)? else { continue };
            let (Some(uploader), Some(assets)) = (&uploader, v["assets"].as_array()) else { continue };
            for a in assets {
                let name = text(&a["name"]);
                progress(format!("Release {}: {name}", r.tag)).await;
                if let Err(e) = asset(core, &api, &r, uploader, a).await {
                    warnings.push(format!("{} of {} couldn't be downloaded: {e}", name, r.tag));
                }
            }
        }
    }
    Ok(warnings)
}

/// Downloads a release file and attaches it.
async fn asset(core: &Core, api: &Api<'_>, release: &keel_core::Release, uploader: &keel_core::User, a: &Value) -> Result<(), String> {
    let max = core.config.lfs_max_bytes;
    if a["size"].as_u64().is_some_and(|s| s > max) {
        return Err("it's bigger than this server allows".into());
    }
    // GitHub serves private repositories' files through the API.
    let (url, binary) = match (api.service, a["url"].as_str()) {
        (ImportService::GitHub, Some(u)) => (u.to_owned(), true),
        _ => (text(&a["browser_download_url"]), false),
    };
    let mut req = api.request(&url)?;
    if binary {
        req = req.header("Accept", "application/octet-stream");
    }
    let res = req.send().await.map_err(|e| e.to_string())?;
    if !res.status().is_success() {
        return Err(format!("the server answered {}", res.status().as_u16()));
    }
    let dir = core.config.data_dir.join("releases");
    tokio::fs::create_dir_all(&dir).await.map_err(|e| e.to_string())?;
    let tmp: PathBuf = dir.join(format!("upload-import-{}-{}", release.id, a["id"].as_i64().unwrap_or_default()));
    let written: Result<u64, String> = async {
        let mut file = tokio::fs::File::create(&tmp).await.map_err(|e| e.to_string())?;
        let mut size = 0u64;
        let mut stream = res.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| e.to_string())?;
            size += chunk.len() as u64;
            if size > max {
                return Err("it's bigger than this server allows".into());
            }
            file.write_all(&chunk).await.map_err(|e| e.to_string())?;
        }
        file.sync_all().await.map_err(|e| e.to_string())?;
        Ok(size)
    }
    .await;
    let result = match written {
        Ok(size) => {
            let content_type = a["content_type"].as_str().unwrap_or("application/octet-stream");
            core.add_release_asset(release, uploader, &text(&a["name"]), content_type, &tmp, size).await.map(|_| ()).map_err(message)
        }
        Err(e) => Err(e),
    };
    let _ = tokio::fs::remove_file(&tmp).await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const WEB: &str = "https://github.com";

    #[test]
    fn token_only_to_the_source() {
        let client = reqwest::Client::new();
        let api = |service, base: &str| Api { client: &client, service, base: base.into(), token: "t0ken".into(), allow_private: false };
        let auth = |api: &Api, url: &str| api.request(url).unwrap().build().unwrap().headers().contains_key("Authorization");
        let gitea = api(ImportService::Gitea, "https://gitea.example/api/v1/repos/octo/engine");
        assert!(auth(&gitea, "https://gitea.example/api/v1/repos/octo/engine/issues"));
        assert!(auth(&gitea, "https://gitea.example/attachments/1"));
        assert!(!auth(&gitea, "https://evil.example/attachments/1"));
        assert!(!auth(&gitea, "http://gitea.example/attachments/1"));
        let github = api(ImportService::GitHub, "https://api.github.com/repos/octo/engine");
        assert!(auth(&github, "https://api.github.com/repos/octo/engine/releases/assets/1"));
        assert!(!auth(&github, "https://objects.example/x"));
    }

    #[test]
    fn github_json() {
        let pr = json!({
            "number": 5, "title": "Speed up", "body": null, "state": "closed",
            "user": { "login": "octocat", "html_url": "https://github.com/octocat" },
            "created_at": "2024-01-02T03:04:05Z", "updated_at": "2024-01-03T00:00:00Z", "closed_at": "2024-01-03T00:00:00Z",
            "merged_at": "2024-01-03T00:00:00Z", "merge_commit_sha": "abc",
            "base": { "ref": "main" }, "head": { "ref": "fast", "sha": "def" },
            "labels": [{ "name": "perf" }], "milestone": { "title": "v1" },
        });
        let as_issue = json!({
            "number": 5, "title": "Speed up", "body": null, "state": "closed", "pull_request": {},
            "user": pr["user"], "created_at": pr["created_at"], "updated_at": pr["updated_at"], "closed_at": pr["closed_at"],
            "labels": pr["labels"], "milestone": pr["milestone"],
        });
        let pulls = HashMap::from([(5, pr)]);
        let i = issue(&as_issue, &pulls, WEB).unwrap();
        assert_eq!((i.state.as_str(), i.created_at, i.body.as_str()), ("merged", 1704164645, ""));
        assert_eq!(i.author, "https://github.com/octocat");
        let p = i.pull.unwrap();
        assert_eq!((p.base_ref.as_str(), p.head_ref.as_str(), p.merge_commit.as_deref()), ("main", "fast", Some("abc")));
        assert_eq!((i.labels, i.milestone), (vec!["perf".to_owned()], Some("v1".into())));

        let plain = json!({ "number": 6, "title": "Bug", "state": "open", "user": null, "created_at": "2024-01-04T00:00:00+02:00" });
        let i = issue(&plain, &HashMap::new(), WEB).unwrap();
        assert!(i.pull.is_none());
        assert_eq!((i.author.as_str(), i.created_at), ("https://github.com/ghost", 1704319200));

        let c = comment(&json!({ "issue_url": "https://api.github.com/repos/a/b/issues/6", "body": "Me too",
            "user": { "login": "hubot" }, "created_at": "2024-01-05T00:00:00Z" }), WEB).unwrap();
        assert_eq!((c.issue_number, c.author.as_str()), (6, "https://github.com/hubot"));
        let r = review(&json!({ "state": "APPROVED", "body": "", "user": { "login": "a" }, "commit_id": "def",
            "submitted_at": "2024-01-05T00:00:00Z" }), 5, WEB).unwrap();
        assert_eq!((r.kind.as_str(), r.commit_id.as_deref()), ("approved", Some("def")));
        assert!(review(&json!({ "state": "COMMENTED", "body": "" }), 5, WEB).is_none());
        assert!(review(&json!({ "state": "PENDING", "body": "x" }), 5, WEB).is_none());
        let l = line_comment(&json!({ "pull_request_url": "https://api.github.com/repos/a/b/pulls/5", "path": "src/a.rs",
            "line": 3, "diff_hunk": "@@ -1,3 +1,3 @@\n a\n-b\n+c", "body": "Why?", "user": { "login": "a" },
            "created_at": "2024-01-05T00:00:00Z" }), WEB).unwrap();
        assert_eq!(l.issue_number, 5);
        assert!(l.body.starts_with("**`src/a.rs`** line 3:\n\n```diff\n@@ -1,3 +1,3 @@\n a\n-b\n+c\n```\n\nWhy?"), "{}", l.body);
    }

    #[test]
    fn gitea_json() {
        // Gitea: colors without "#", pull request comments by their page.
        assert_eq!(label(&json!({ "name": "bug", "color": "ee0701", "description": "x" })).color, "ee0701");
        let c = comment(&json!({ "issue_url": "", "pull_request_url": "https://codeberg.org/a/b/pulls/9", "body": "ok",
            "user": { "login": "ada", "html_url": "https://codeberg.org/ada" }, "created_at": "2024-01-05T00:00:00Z" }),
            "https://codeberg.org").unwrap();
        assert_eq!((c.issue_number, c.author.as_str()), (9, "https://codeberg.org/ada"));
        let r = review(&json!({ "state": "REQUEST_CHANGES", "body": "No", "user": { "login": "ada" },
            "submitted_at": "2024-01-05T00:00:00Z" }), 9, "https://codeberg.org").unwrap();
        assert_eq!(r.kind, "changes_requested");
        let m = milestone(&json!({ "title": "v1", "state": "closed", "due_on": null, "closed_at": "2024-01-05T00:00:00Z" }));
        assert!(m.closed && m.due_on.is_none() && m.closed_at.is_some());
    }
}
