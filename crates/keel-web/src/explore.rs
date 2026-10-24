//! The Explore page: every repository the viewer can see, searchable and
//! filtered by topic.

use askama::Template;
use axum::extract::{Query, State};
use keel_core::{ExploreQuery, ExploreSort};
use serde::Deserialize;

use crate::error::{AppResult, html};
use crate::pages::{RepoCard, cards};
use crate::session::Viewer;
use crate::{AppState, Page};

const PER_PAGE: i64 = 20;

#[derive(Template)]
#[template(path = "explore.html")]
struct ExplorePage {
    page: Page,
    q: String,
    topic: Option<String>,
    sort: &'static str,
    sorts: Vec<(&'static str, &'static str)>,
    repos: Vec<RepoCard>,
    total: i64,
    topics: Vec<(String, i64)>,
    newer_url: Option<String>,
    older_url: Option<String>,
}

#[derive(Deserialize)]
pub struct ExploreParams {
    #[serde(default)]
    q: String,
    topic: Option<String>,
    sort: Option<String>,
    page: Option<i64>,
}

fn page_url(q: &str, topic: Option<&str>, sort: ExploreSort, page: i64) -> String {
    let mut s = form_urlencoded::Serializer::new(String::new());
    if !q.is_empty() {
        s.append_pair("q", q);
    }
    if let Some(t) = topic {
        s.append_pair("topic", t);
    }
    if sort != ExploreSort::default() {
        s.append_pair("sort", sort.as_str());
    }
    if page > 1 {
        s.append_pair("page", &page.to_string());
    }
    let query = s.finish();
    if query.is_empty() { "/explore".into() } else { format!("/explore?{query}") }
}

pub async fn index(State(state): State<AppState>, Viewer(viewer): Viewer, Query(p): Query<ExploreParams>) -> AppResult {
    let sort = p.sort.as_deref().and_then(ExploreSort::parse).unwrap_or_default();
    let topic = p.topic.map(|t| t.trim().to_lowercase()).filter(|t| !t.is_empty());
    let page = p.page.unwrap_or(1).max(1);
    let query = ExploreQuery { text: p.q.trim().to_owned(), topic: topic.clone(), sort };
    let (repos, total) = state.core.explore(viewer.as_ref(), &query, (page - 1) * PER_PAGE, PER_PAGE).await?;
    let topics = state.core.popular_topics(viewer.as_ref(), 30).await?;
    let url = |n| page_url(&query.text, topic.as_deref(), sort, n);
    html(ExplorePage {
        page: Page::new(&state, viewer, "Explore"),
        newer_url: (page > 1).then(|| url(page - 1)),
        older_url: (page * PER_PAGE < total).then(|| url(page + 1)),
        sorts: ExploreSort::ALL.iter().map(|s| (s.as_str(), s.label())).collect(),
        sort: sort.as_str(),
        repos: cards(&repos),
        q: query.text,
        topic,
        total,
        topics,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_urls() {
        assert_eq!(page_url("", None, ExploreSort::Updated, 1), "/explore");
        assert_eq!(page_url("a b", Some("rust"), ExploreSort::Name, 2), "/explore?q=a+b&topic=rust&sort=name&page=2");
    }
}
