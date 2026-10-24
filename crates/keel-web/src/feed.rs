//! Activity feed entries, for the home page and profiles.

use keel_core::Event;

use crate::render::{datetime, escape, relative_time};
use crate::repo::encode_path;

pub(crate) struct EventView {
    /// What happened, as HTML (escaped, with links).
    pub html: String,
    /// `push`, `issue`, `comment`, … (for the icon).
    pub kind: &'static str,
    pub when: String,
    pub when_full: String,
    /// Pushes: the newest commits, (short id, summary, url).
    pub commits: Vec<(String, String, String)>,
    pub more_commits: i64,
    /// Comments: the start of what was said.
    pub excerpt: String,
    pub id: i64,
}

fn link(url: &str, text: &str) -> String {
    format!("<a href=\"{}\">{}</a>", escape(url), escape(text))
}

pub(crate) fn view(e: &Event) -> EventView {
    let repo = format!("{}/{}", e.owner, e.repo);
    let base = format!("/{repo}");
    let repo_link = link(&base, &repo);
    let actor = e.actor.clone().unwrap_or_else(|| "ghost".into());
    let who = link(&format!("/{actor}"), &actor);
    let d = e.details();
    let ref_name = e.ref_name.as_deref().unwrap_or_default();
    let (is_tag, short) = match ref_name.strip_prefix("refs/tags/") {
        Some(t) => (true, t),
        None => (false, ref_name.strip_prefix("refs/heads/").unwrap_or(ref_name)),
    };
    let ref_link = if is_tag {
        link(&format!("{base}/releases/tag/{}", encode_path(short)), short)
    } else {
        link(&format!("{base}/tree/{}", encode_path(short)), short)
    };
    let issue = || {
        let n = e.issue_number.unwrap_or_default();
        let pull = e.is_pull.unwrap_or(false);
        let kind = if pull { "pull request" } else { "issue" };
        let url = format!("{base}/{}/{n}", if pull { "pull" } else { "issues" });
        format!("{kind} {}", link(&url, &format!("#{n} {}", e.issue_title.as_deref().unwrap_or_default())))
    };
    let count = d["count"].as_i64().unwrap_or(0);
    let (kind, html) = match e.kind.as_str() {
        "push" => {
            let s = if count == 1 { "" } else { "s" };
            if is_tag {
                ("push", format!("{who} moved tag {ref_link} in {repo_link}"))
            } else {
                ("push", format!("{who} pushed {count} commit{s} to {ref_link} in {repo_link}"))
            }
        }
        "create_ref" => {
            let what = if is_tag { "tag" } else { "branch" };
            ("create", format!("{who} created {what} {ref_link} in {repo_link}"))
        }
        "delete_ref" => {
            let what = if is_tag { "tag" } else { "branch" };
            ("delete", format!("{who} deleted {what} {} in {repo_link}", escape(short)))
        }
        "issue" => {
            let action = d["action"].as_str().unwrap_or("updated");
            let kind = match action {
                "opened" | "reopened" => "open",
                "merged" => "merged",
                _ => "closed",
            };
            (kind, format!("{who} {} {} in {repo_link}", escape(action), issue()))
        }
        "comment" => match d["kind"].as_str() {
            Some("approved") => ("approved", format!("{who} approved {} in {repo_link}", issue())),
            Some("changes_requested") => ("changes", format!("{who} requested changes on {} in {repo_link}", issue())),
            _ => ("comment", format!("{who} commented on {} in {repo_link}", issue())),
        },
        "release" => {
            let tag = e.release_tag.as_deref().unwrap_or_default();
            let title = e.release_title.as_deref().filter(|t| !t.is_empty()).unwrap_or(tag);
            let url = format!("{base}/releases/tag/{}", encode_path(tag));
            ("release", format!("{who} released {} in {repo_link}", link(&url, title)))
        }
        "fork" => {
            let parent = d["parent"].as_str().unwrap_or_default();
            ("create", format!("{who} forked {} to {repo_link}", link(&format!("/{parent}"), parent)))
        }
        "wiki" => {
            let pages = d["pages"].as_array().map(Vec::as_slice).unwrap_or_default();
            let html = match pages {
                [p] => {
                    let name = p["name"].as_str().unwrap_or_default();
                    let title = keel_core::page_title(name);
                    let page = match p["action"].as_str() {
                        Some("deleted") => escape(&title),
                        _ => link(&format!("{base}/wiki/{}", encode_path(name)), &title),
                    };
                    let action = p["action"].as_str().unwrap_or("edited");
                    format!("{who} {} wiki page {page} in {repo_link}", escape(action))
                }
                _ => format!("{who} updated {} {} in {repo_link}", pages.len(), link(&format!("{base}/wiki"), "wiki pages")),
            };
            ("wiki", html)
        }
        "star" => ("star", format!("{who} starred {repo_link}")),
        _ => ("create", format!("{who} created repository {repo_link}")),
    };
    let commits: Vec<(String, String, String)> = d["commits"]
        .as_array()
        .map(|cs| {
            cs.iter()
                .filter_map(|c| {
                    let sha = c["sha"].as_str()?;
                    Some((sha.get(..7).unwrap_or(sha).to_owned(), c["summary"].as_str()?.to_owned(), format!("{base}/commit/{sha}")))
                })
                .collect()
        })
        .unwrap_or_default();
    EventView {
        html,
        kind,
        when: relative_time(e.created_at),
        when_full: datetime(e.created_at),
        more_commits: (count - commits.len() as i64).max(0),
        commits,
        excerpt: d["excerpt"].as_str().unwrap_or_default().to_owned(),
        id: e.id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: &str, data: &str) -> Event {
        Event {
            id: 1,
            owner: "ada".into(),
            repo: "engine".into(),
            actor: Some("bob".into()),
            kind: kind.into(),
            ref_name: Some("refs/heads/main".into()),
            issue_number: Some(3),
            issue_title: Some("Crash <now>".into()),
            is_pull: Some(true),
            release_tag: Some("v1.0".into()),
            release_title: Some(String::new()),
            data: data.into(),
            created_at: 0,
        }
    }

    #[test]
    fn sentences() {
        let push = view(&event("push", r#"{"count": 5, "commits": [{"sha": "abcdef123456", "summary": "Fix"}]}"#));
        assert!(push.html.contains("pushed 5 commits to <a href=\"/ada/engine/tree/main\">main</a>"));
        assert_eq!((push.commits[0].0.as_str(), push.more_commits), ("abcdef1", 4));
        let merged = view(&event("issue", r#"{"action": "merged"}"#));
        assert!(merged.html.contains("merged pull request <a href=\"/ada/engine/pull/3\">#3 Crash &lt;now&gt;</a>"), "{}", merged.html);
        assert!(view(&event("release", "{}")).html.contains(">v1.0</a>"));
        assert!(view(&event("comment", r#"{"kind": "approved"}"#)).html.contains("approved pull request"));
        let wiki = view(&event("wiki", r#"{"pages": [{"name": "Getting-Started", "action": "created"}]}"#));
        assert!(wiki.html.contains("created wiki page <a href=\"/ada/engine/wiki/Getting-Started\">Getting Started</a>"), "{}", wiki.html);
        let wiki = view(&event("wiki", r#"{"pages": [{"name": "A", "action": "edited"}, {"name": "B", "action": "deleted"}]}"#));
        assert!(view(&event("star", "{}")).html.contains("starred <a href=\"/ada/engine\">ada/engine</a>"));
        assert!(wiki.html.contains("updated 2 <a href=\"/ada/engine/wiki\">wiki pages</a>"), "{}", wiki.html);
    }
}
