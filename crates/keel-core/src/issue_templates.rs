//! Issue and pull request templates from the repository's default branch,
//! where GitHub (and Gitea) look for them, so existing ones just work:
//!
//! - `ISSUE_TEMPLATE/*.md`: several issue templates to choose from, each
//!   with front matter (`name`, `about`, `title`, `labels`, `assignees`),
//!   and `ISSUE_TEMPLATE/config.yml` with `blank_issues_enabled: false` to
//!   only allow issues from a template;
//! - `ISSUE_TEMPLATE.md`: the text new issues start with;
//! - `PULL_REQUEST_TEMPLATE.md`: the text new pull requests start with,
//!
//! in `.keel/`, `.github/`, `.gitea/`, the top level or `docs/` (the first
//! that has any), with names in any case.

use keel_git::{EntryKind, GitRepo, Oid};

use crate::{Core, Result};

/// Where templates are looked for, in order.
const LOCATIONS: [&str; 5] = [".keel", ".github", ".gitea", "", "docs"];
/// Bigger files aren't templates.
const MAX_SIZE: usize = 64 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IssueTemplate {
    /// The file, e.g. `bug_report.md`, to pick the template by.
    pub file: String,
    pub name: String,
    pub about: String,
    pub title: String,
    pub labels: Vec<String>,
    pub assignees: Vec<String>,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssueTemplates {
    pub templates: Vec<IssueTemplate>,
    /// Whether issues without a template are welcome.
    pub blank_allowed: bool,
    /// People choose among the templates (from an `ISSUE_TEMPLATE`
    /// directory); otherwise new issues start with the only one.
    pub choose: bool,
}

impl Default for IssueTemplates {
    fn default() -> Self {
        Self { templates: Vec::new(), blank_allowed: true, choose: false }
    }
}

impl IssueTemplates {
    pub fn find(&self, file: &str) -> Option<&IssueTemplate> {
        self.templates.iter().find(|t| t.file == file)
    }
}

/// Removes quotes around a YAML scalar.
fn unquote(s: &str) -> &str {
    let s = s.trim();
    for q in ['"', '\''] {
        if let Some(inner) = s.strip_prefix(q).and_then(|s| s.strip_suffix(q)) {
            return inner;
        }
    }
    s
}

/// `a, b`, `[a, "b"]` or a YAML list's items.
fn list(value: &str, items: &[String]) -> Vec<String> {
    let value = value.trim();
    let value = value.strip_prefix('[').and_then(|v| v.strip_suffix(']')).unwrap_or(value);
    value
        .split(',')
        .map(unquote)
        .chain(items.iter().map(|i| unquote(i)))
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .collect()
}

/// A front matter line: key (lowercase), value, and the items of a list
/// that follows it.
type Field = (String, String, Vec<String>);

/// Splits `---`-delimited front matter (simple `key: value` lines and
/// lists) from the text after it.
fn front_matter(text: &str) -> (Vec<Field>, &str) {
    let Some(rest) = text.strip_prefix("---\n").or_else(|| text.strip_prefix("---\r\n")) else {
        return (Vec::new(), text);
    };
    let Some(end) = rest.find("\n---") else { return (Vec::new(), text) };
    let body = rest[end + 4..].trim_start_matches(['-']).trim_start_matches(['\r', '\n']);
    let mut fields: Vec<Field> = Vec::new();
    for line in rest[..end].lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(item) = trimmed.strip_prefix("- ") {
            if let Some(last) = fields.last_mut() {
                last.2.push(item.to_owned());
            }
            continue;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            fields.push((k.trim().to_ascii_lowercase(), v.trim().to_owned(), Vec::new()));
        }
    }
    (fields, body)
}

/// Reads an issue template (front matter and text).
pub fn parse_issue_template(file: &str, text: &str) -> IssueTemplate {
    let (fields, body) = front_matter(text);
    let stem = file.rsplit_once('.').map_or(file, |(s, _)| s);
    let mut t = IssueTemplate { file: file.to_owned(), name: stem.replace(['-', '_'], " "), body: body.to_owned(), ..Default::default() };
    for (k, v, items) in fields {
        match k.as_str() {
            "name" if !unquote(&v).is_empty() => t.name = unquote(&v).to_owned(),
            "about" | "description" => t.about = unquote(&v).to_owned(),
            "title" => t.title = unquote(&v).to_owned(),
            "labels" => t.labels = list(&v, &items),
            "assignees" => t.assignees = list(&v, &items),
            _ => {}
        }
    }
    t
}

/// `blank_issues_enabled` from an `ISSUE_TEMPLATE/config.yml`.
fn blank_issues_enabled(config: &str) -> bool {
    !config.lines().any(|l| {
        l.split_once(':')
            .is_some_and(|(k, v)| k.trim() == "blank_issues_enabled" && unquote(v.split('#').next().unwrap_or_default()) == "false")
    })
}

fn is_markdown(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".md") || lower.ends_with(".markdown")
}

fn read_text(g: &GitRepo, commit: Oid, path: &str) -> keel_git::Result<Option<String>> {
    Ok(g.blob(commit, path)?
        .filter(|b| !b.binary && b.data.len() <= MAX_SIZE)
        .map(|b| String::from_utf8_lossy(&b.data).replace("\r\n", "\n")))
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() { name.to_owned() } else { format!("{dir}/{name}") }
}

/// The location's entries named `name` (ignoring case) of the given kind.
fn entry(entries: &[keel_git::TreeEntry], name: &str, dir: bool) -> Option<String> {
    entries
        .iter()
        .find(|e| (e.kind == EntryKind::Dir) == dir && e.name.eq_ignore_ascii_case(name))
        .map(|e| e.name.clone())
}

fn find_issue_templates(g: &GitRepo) -> keel_git::Result<IssueTemplates> {
    let Some(tip) = g.default_branch()?.and_then(|b| g.branch_tip(&b)) else { return Ok(IssueTemplates::default()) };
    for location in LOCATIONS {
        let Some(entries) = g.tree(tip, location)? else { continue };
        if let Some(dir) = entry(&entries, "ISSUE_TEMPLATE", true) {
            let dir = join(location, &dir);
            let files = g.tree(tip, &dir)?.unwrap_or_default();
            let mut templates = Vec::new();
            let mut blank_allowed = true;
            for f in &files {
                let path = join(&dir, &f.name);
                let lower = f.name.to_ascii_lowercase();
                if lower == "config.yml" || lower == "config.yaml" {
                    blank_allowed = read_text(g, tip, &path)?.is_none_or(|c| blank_issues_enabled(&c));
                } else if is_markdown(&f.name)
                    && let Some(text) = read_text(g, tip, &path)?
                {
                    templates.push(parse_issue_template(&f.name, &text));
                }
            }
            if !templates.is_empty() {
                templates.sort_by_cached_key(|t| t.file.to_lowercase());
                return Ok(IssueTemplates { templates, blank_allowed, choose: true });
            }
        }
        for name in ["ISSUE_TEMPLATE.md", "ISSUE_TEMPLATE.markdown"] {
            if let Some(file) = entry(&entries, name, false)
                && let Some(text) = read_text(g, tip, &join(location, &file))?
            {
                return Ok(IssueTemplates { templates: vec![parse_issue_template(&file, &text)], blank_allowed: true, choose: false });
            }
        }
    }
    Ok(IssueTemplates::default())
}

fn find_pull_template(g: &GitRepo) -> keel_git::Result<Option<String>> {
    let Some(tip) = g.default_branch()?.and_then(|b| g.branch_tip(&b)) else { return Ok(None) };
    for location in LOCATIONS {
        let Some(entries) = g.tree(tip, location)? else { continue };
        for name in ["PULL_REQUEST_TEMPLATE.md", "PULL_REQUEST_TEMPLATE.markdown"] {
            if let Some(file) = entry(&entries, name, false)
                && let Some(text) = read_text(g, tip, &join(location, &file))?
            {
                // Front matter isn't shown to people writing the description.
                return Ok(Some(front_matter(&text).1.to_owned()));
            }
        }
    }
    Ok(None)
}

impl Core {
    /// The repository's issue templates (none if it has none).
    pub async fn issue_templates(&self, repo_id: i64) -> Result<IssueTemplates> {
        self.git(repo_id, find_issue_templates).await
    }

    /// The text new pull requests into the repository start with.
    pub async fn pull_template(&self, repo_id: i64) -> Result<Option<String>> {
        self.git(repo_id, find_pull_template).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates() {
        let t = parse_issue_template(
            "bug_report.md",
            "---\nname: Bug report\nabout: \"Something's broken\"\ntitle: '[Bug] '\nlabels: bug, \"needs triage\"\nassignees:\n  - ada\n  - bob\n---\n\n**What happened?**\n",
        );
        assert_eq!(t.name, "Bug report");
        assert_eq!(t.about, "Something's broken");
        assert_eq!(t.title, "[Bug] ");
        assert_eq!(t.labels, ["bug", "needs triage"]);
        assert_eq!(t.assignees, ["ada", "bob"]);
        assert_eq!(t.body, "**What happened?**\n");

        let t = parse_issue_template("feature-request.md", "---\nlabels: [enhancement]\n---\nIdea:");
        assert_eq!((t.name.as_str(), t.labels.as_slice(), t.body.as_str()), ("feature request", &["enhancement".to_owned()][..], "Idea:"));
        let plain = parse_issue_template("ISSUE_TEMPLATE.md", "Steps:\n---\n");
        assert_eq!(plain.body, "Steps:\n---\n");

        assert!(!blank_issues_enabled("blank_issues_enabled: false # templates only\ncontact_links: []"));
        assert!(blank_issues_enabled("contact_links: []"));
    }

    #[tokio::test]
    async fn finding_templates() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let repo = core.create_repo(&ada, "engine", "", false).await.unwrap();
        assert_eq!(core.issue_templates(repo.id).await.unwrap(), IssueTemplates::default());
        assert_eq!(core.pull_template(repo.id).await.unwrap(), None);

        let g = GitRepo::open(&core.repo_path(&repo)).unwrap();
        let who = keel_git::Identity::new("ada", "ada@example.com");
        let files: [(&str, Option<&[u8]>); 5] = [
            (".github/ISSUE_TEMPLATE/bug.md", Some(b"---\nname: Bug\n---\nBroken?")),
            (".github/ISSUE_TEMPLATE/a_idea.md", Some(b"An idea")),
            (".github/ISSUE_TEMPLATE/config.yml", Some(b"blank_issues_enabled: false\n")),
            ("ISSUE_TEMPLATE.md", Some(b"Not used: the directory wins")),
            ("docs/pull_request_template.md", Some(b"## Changes\n")),
        ];
        g.commit_files("main", None, &files, "Templates", &who).unwrap();
        let found = core.issue_templates(repo.id).await.unwrap();
        assert_eq!(found.templates.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["a idea", "Bug"]);
        assert!(!found.blank_allowed && found.choose);
        assert_eq!(found.find("bug.md").unwrap().body, "Broken?");
        assert_eq!(core.pull_template(repo.id).await.unwrap().as_deref(), Some("## Changes\n"));
    }
}
