//! Contributor and commit activity graphs, drawn as SVG on the server.

use askama::Template;
use axum::extract::{Path, State};
use keel_core::{DAY, RepoActivity};

use crate::error::{AppResult, html};
use crate::render::escape;
use crate::repo::{RepoView, load};
use crate::session::Viewer;
use crate::{AppState, Page};

/// Contributors listed.
const TOP: usize = 100;
/// Width of the charts' coordinate system; they scale to the page.
const WIDTH: f64 = 720.0;
/// For the small day-of-the-week chart, so its labels stay readable.
const NARROW: f64 = 300.0;
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

fn today() -> i64 {
    crate::now().div_euclid(DAY)
}

/// The (proleptic Gregorian) date of a day since the Unix epoch.
fn date_of(day: i64) -> time::Date {
    time::Date::from_julian_day((day + 2_440_588) as i32).unwrap_or(time::Date::MIN)
}

/// Weeks start on Sunday; the Unix epoch was a Thursday.
fn week_of(day: i64) -> i64 {
    (day + 4).div_euclid(7)
}

fn month_of(day: i64) -> i64 {
    let d = date_of(day);
    i64::from(d.year()) * 12 + i64::from(u8::from(d.month())) - 1
}

fn short_date(day: i64) -> String {
    let d = date_of(day);
    format!("{} {}, {}", MONTHS[usize::from(u8::from(d.month())) - 1], d.day(), d.year())
}

/// How a span of days is split into bars: weeks, or months for long spans.
struct Buckets {
    monthly: bool,
    /// The first week or month.
    first: i64,
    count: usize,
}

impl Buckets {
    fn new(from_day: i64, to_day: i64) -> Self {
        let weeks = week_of(to_day) - week_of(from_day) + 1;
        if weeks <= 104 {
            Self { monthly: false, first: week_of(from_day), count: weeks as usize }
        } else {
            let first = month_of(from_day);
            Self { monthly: true, first, count: (month_of(to_day) - first + 1) as usize }
        }
    }

    fn index(&self, day: i64) -> Option<usize> {
        let unit = if self.monthly { month_of(day) } else { week_of(day) };
        usize::try_from(unit - self.first).ok().filter(|&i| i < self.count)
    }

    fn counts<'a>(&self, days: impl Iterator<Item = (&'a i64, &'a i64)>) -> Vec<i64> {
        let mut out = vec![0; self.count];
        for (&day, &n) in days {
            if let Some(i) = self.index(day) {
                out[i] += n;
            }
        }
        out
    }

    /// What bar `i` covers, for its tooltip.
    fn describe(&self, i: usize) -> String {
        let unit = self.first + i as i64;
        if self.monthly {
            format!("{} {}", MONTHS[unit.rem_euclid(12) as usize], unit.div_euclid(12))
        } else {
            format!("the week of {}", short_date(unit * 7 - 4))
        }
    }

    /// Labels under the bars: months (with years in January) for weeks;
    /// years for months, thinned out to at most about a dozen.
    fn labels(&self) -> Vec<(usize, String)> {
        if self.monthly {
            let years = self.count / 12 + 1;
            let step = years.div_ceil(12).max(1) as i64;
            (0..self.count)
                .filter_map(|i| {
                    let unit = self.first + i as i64;
                    let year = unit.div_euclid(12);
                    (unit.rem_euclid(12) == 0 && year % step == 0).then(|| (i, year.to_string()))
                })
                .collect()
        } else {
            let mut out = Vec::new();
            let mut last_month = None;
            for i in 0..self.count {
                let d = date_of((self.first + i as i64) * 7 - 4);
                if last_month != Some(d.month()) {
                    let m = MONTHS[usize::from(u8::from(d.month())) - 1];
                    let text = if u8::from(d.month()) == 1 || i == 0 { format!("{m} {}", d.year()) } else { m.to_owned() };
                    out.push((i, text));
                    last_month = Some(d.month());
                }
            }
            // The first bar's label would collide with the next one.
            if out.len() > 1 && out[1].0 < 3 {
                out.remove(0);
            }
            out
        }
    }
}

fn plural(n: i64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// A bar chart: one bar per value with a tooltip, and labels under it
/// (`labels` gives the bar each starts at).
fn bar_chart(values: &[i64], tooltip: impl Fn(usize, i64) -> String, labels: &[(usize, String)], height: f64, title: &str) -> String {
    chart_of_width(WIDTH, values, tooltip, labels, height, title)
}

fn chart_of_width(
    width: f64,
    values: &[i64],
    tooltip: impl Fn(usize, i64) -> String,
    labels: &[(usize, String)],
    height: f64,
    title: &str,
) -> String {
    let n = values.len().max(1) as f64;
    let max = values.iter().copied().max().unwrap_or(0).max(1) as f64;
    let bar = width / n;
    let gap = if bar >= 4.0 { 1.0 } else { 0.0 };
    let total = if labels.is_empty() { height + 1.0 } else { height + 16.0 };
    let mut svg = format!(
        "<svg class=\"chart\" viewBox=\"0 0 {width} {total}\" role=\"img\" aria-label=\"{}\"><line class=\"axis\" x1=\"0\" y1=\"{height}\" x2=\"{width}\" y2=\"{height}\"/>",
        escape(title)
    );
    for (i, &v) in values.iter().enumerate() {
        if v == 0 {
            continue;
        }
        let h = (v as f64 / max * (height - 2.0)).max(1.5);
        svg.push_str(&format!(
            "<rect class=\"bar\" x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{h:.1}\"><title>{}</title></rect>",
            i as f64 * bar + gap / 2.0,
            height - h,
            (bar - gap).max(0.5),
            escape(&tooltip(i, v))
        ));
    }
    for (i, text) in labels {
        svg.push_str(&format!("<text x=\"{:.1}\" y=\"{}\">{}</text>", *i as f64 * bar, height + 12.0, escape(text)));
    }
    svg.push_str("</svg>");
    svg
}

pub(crate) struct ContributorView {
    pub rank: usize,
    pub name: String,
    /// Their account here, if their email belongs to one.
    pub username: Option<String>,
    pub commits: i64,
    pub share: String,
    pub chart: String,
}

#[derive(Template)]
#[template(path = "repo/contributors.html")]
struct ContributorsPage {
    page: Page,
    repo: RepoView,
    tab: &'static str,
    branch: String,
    total: i64,
    people: usize,
    since: String,
    chart: String,
    contributors: Vec<ContributorView>,
}

async fn activity(state: &AppState, repo: &keel_core::Repo) -> AppResult<Option<std::sync::Arc<RepoActivity>>> {
    Ok(state.core.repo_activity(repo).await?)
}

pub async fn contributors(
    State(state): State<AppState>,
    Viewer(viewer): Viewer,
    Path((owner, name)): Path<(String, String)>,
) -> AppResult {
    let (repo, access) = load(&state, viewer.as_ref(), &owner, &name).await?;
    let a = activity(&state, &repo).await?;
    let mut page = ContributorsPage {
        repo: RepoView::new(&state, &repo, access, viewer.as_ref(), "insights", "").await?,
        page: Page::new(&state, viewer, format!("Contributors · {}/{}", repo.owner, repo.name)),
        tab: "contributors",
        branch: String::new(),
        total: 0,
        people: 0,
        since: String::new(),
        chart: String::new(),
        contributors: Vec::new(),
    };
    if let Some(a) = a
        && let (Some((&first, _)), Some((&last, _))) = (a.days.first_key_value(), a.days.last_key_value())
    {
        let buckets = Buckets::new(first, last.max(today()));
        let labels = buckets.labels();
        let tip = |i: usize, n: i64| format!("{} in {}", plural(n, "commit", "commits"), buckets.describe(i));
        page.chart = bar_chart(&buckets.counts(a.days.iter()), tip, &labels, 120.0, "Commits over time");
        page.total = a.total();
        page.people = a.people.len();
        page.since = short_date(first);
        page.branch = a.branch.clone();
        let top = a.top(TOP);
        let emails: Vec<String> = top.iter().map(|c| c.email.to_lowercase()).collect();
        let accounts = state.core.commit_authors(&emails.iter().map(String::as_str).collect::<Vec<_>>()).await?;
        page.contributors = top
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let tip = |b: usize, n: i64| format!("{} in {}", plural(n, "commit", "commits"), buckets.describe(b));
                ContributorView {
                    rank: i + 1,
                    name: c.name.clone(),
                    username: accounts.get(&emails[i]).cloned(),
                    commits: c.commits,
                    share: format!("{:.1}%", c.commits as f64 * 100.0 / page.total.max(1) as f64),
                    chart: bar_chart(&buckets.counts(c.days.iter()), tip, &[], 36.0, &format!("Commits by {}", c.name)),
                }
            })
            .collect();
    }
    html(page)
}

#[derive(Template)]
#[template(path = "repo/commit_activity.html")]
struct CommitActivityPage {
    page: Page,
    repo: RepoView,
    tab: &'static str,
    branch: String,
    /// In the last 52 weeks.
    total: i64,
    per_week: String,
    busiest: Option<String>,
    weeks_chart: String,
    days_chart: String,
}

pub async fn commit_activity(
    State(state): State<AppState>,
    Viewer(viewer): Viewer,
    Path((owner, name)): Path<(String, String)>,
) -> AppResult {
    let (repo, access) = load(&state, viewer.as_ref(), &owner, &name).await?;
    let a = activity(&state, &repo).await?;
    // This week and the 51 before it.
    let first = (week_of(today()) - 51) * 7 - 4;
    let buckets = Buckets::new(first, today());
    let days = a.as_ref().map(|a| a.days.range(first..).collect::<Vec<_>>()).unwrap_or_default();
    let weeks = buckets.counts(days.iter().copied());
    let mut weekdays = [0i64; 7];
    for &(&day, &n) in &days {
        weekdays[(day + 4).rem_euclid(7) as usize] += n;
    }
    let total: i64 = weeks.iter().sum();
    let busiest = weeks
        .iter()
        .enumerate()
        .filter(|(_, n)| **n > 0)
        .max_by_key(|(i, n)| (**n, *i))
        .map(|(i, n)| format!("{} in {}", plural(*n, "commit", "commits"), buckets.describe(i)));
    let week_tip = |i: usize, n: i64| format!("{} in {}", plural(n, "commit", "commits"), buckets.describe(i));
    let day_tip = |i: usize, n: i64| format!("{} on {}s", plural(n, "commit", "commits"), WEEKDAYS[i]);
    let day_labels: Vec<(usize, String)> = WEEKDAYS.iter().enumerate().map(|(i, d)| (i, (*d).to_owned())).collect();
    html(CommitActivityPage {
        repo: RepoView::new(&state, &repo, access, viewer.as_ref(), "insights", "").await?,
        page: Page::new(&state, viewer, format!("Commit activity · {}/{}", repo.owner, repo.name)),
        tab: "commits",
        branch: a.map(|a| a.branch.clone()).unwrap_or_default(),
        total,
        per_week: format!("{:.1}", total as f64 / 52.0),
        busiest,
        weeks_chart: bar_chart(&weeks, week_tip, &buckets.labels(), 140.0, "Commits per week in the last year"),
        days_chart: chart_of_width(NARROW, &weekdays, day_tip, &day_labels, 100.0, "Commits per day of the week in the last year"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar() {
        // 2024-03-03 was a Sunday: day 19785.
        assert_eq!(short_date(19_785), "Mar 3, 2024");
        assert_eq!(week_of(19_785), week_of(19_791), "Sunday to Saturday");
        assert_ne!(week_of(19_785), week_of(19_784));
        assert_eq!(week_of(19_785) * 7 - 4, 19_785, "weeks start on Sunday");
        assert_eq!(month_of(19_785), 2024 * 12 + 2);

        let weekly = Buckets::new(19_785, 19_785 + 30);
        assert!(!weekly.monthly);
        assert_eq!((weekly.count, weekly.index(19_786), weekly.index(19_785 + 7)), (5, Some(0), Some(1)));
        assert_eq!(weekly.describe(1), "the week of Mar 10, 2024");
        let monthly = Buckets::new(19_785, 19_785 + 3 * 365);
        assert!(monthly.monthly);
        assert_eq!(monthly.describe(0), "Mar 2024");
        assert_eq!(monthly.labels().first().map(|l| l.1.as_str()), Some("2025"));
        assert_eq!(monthly.index(19_700), None, "before the first month");
    }

    #[test]
    fn charts() {
        let svg = bar_chart(&[0, 2, 4], |i, n| format!("{n} <in> {i}"), &[(0, "Jan".into())], 100.0, "A & B");
        assert_eq!(svg.matches("<rect").count(), 2, "no bars for zero");
        assert!(svg.contains("height=\"98.0\""), "the largest fills the chart: {svg}");
        assert!(svg.contains("4 &lt;in&gt; 2") && svg.contains("A &amp; B"));
        assert!(svg.contains("<text x=\"0.0\" y=\"112\">Jan</text>"));
    }
}
