//! The contribution calendar on profiles: a year of days as squares, a week
//! per column (Sunday at the top, as on GitHub), shaded by how much someone
//! contributed that day.

use std::collections::HashMap;
use std::fmt::Write;

const WEEKS: i64 = 53;
const CELL: i64 = 10;
const STEP: i64 = 13;
const LEFT: i64 = 28;
const TOP: i64 = 16;
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// Sunday is 0. Day 0 (1970-01-01) was a Thursday.
fn weekday(day: i64) -> i64 {
    (day + 4).rem_euclid(7)
}

/// Year, month (0–11) and day of the month of a day since the epoch.
fn ymd(day: i64) -> (i32, usize, u8) {
    let date = time::OffsetDateTime::from_unix_timestamp(day * 86_400).map_or(time::Date::MIN, |t| t.date());
    (date.year(), u8::from(date.month()) as usize - 1, date.day())
}

/// 0 for nothing, then 1–4 by quarters of the busiest day.
fn level(n: i64, max: i64) -> u8 {
    match n {
        0 => 0,
        _ if n * 4 <= max => 1,
        _ if n * 2 <= max => 2,
        _ if n * 4 <= max * 3 => 3,
        _ => 4,
    }
}

/// The calendar (SVG) ending with `today`, and the year's total.
pub(crate) fn calendar(counts: &[(i64, i64)], today: i64) -> (String, i64) {
    let by_day: HashMap<i64, i64> = counts.iter().copied().collect();
    let start = today - weekday(today) - (WEEKS - 1) * 7;
    let total: i64 = by_day.iter().filter(|(d, _)| (start..=today).contains(*d)).map(|(_, n)| n).sum();
    let max = by_day.iter().filter(|(d, _)| (start..=today).contains(*d)).map(|(_, n)| *n).max().unwrap_or(0);
    let width = LEFT + WEEKS * STEP;
    let height = TOP + 7 * STEP;
    let mut svg = format!(
        r#"<svg class="calendar" viewBox="0 0 {width} {height}" role="img" aria-label="{total} contributions in the last year">"#
    );
    // Months: over the first full week of each.
    let mut last_label = -3;
    for week in 0..WEEKS {
        let first = start + week * 7;
        let (_, month, day) = ymd(first);
        if day <= 7 && week - last_label >= 3 && week < WEEKS - 1 {
            let _ = write!(svg, r#"<text x="{}" y="10" class="cal-label">{}</text>"#, LEFT + week * STEP, MONTHS[month]);
            last_label = week;
        }
    }
    for (row, name) in [(1, "Mon"), (3, "Wed"), (5, "Fri")] {
        let _ = write!(svg, r#"<text x="0" y="{}" class="cal-label">{name}</text>"#, TOP + row * STEP + 9);
    }
    for day in start..=today {
        let n = by_day.get(&day).copied().unwrap_or(0);
        let (year, month, dom) = ymd(day);
        let what = match n {
            0 => "No contributions".to_owned(),
            1 => "1 contribution".to_owned(),
            n => format!("{n} contributions"),
        };
        let week = (day - start) / 7;
        let _ = write!(
            svg,
            r#"<rect x="{}" y="{}" width="{CELL}" height="{CELL}" rx="2" class="cal-{}"><title>{what} on {} {dom}, {year}</title></rect>"#,
            LEFT + week * STEP,
            TOP + weekday(day) * STEP,
            level(n, max),
            MONTHS[month],
        );
    }
    svg.push_str("</svg>");
    (svg, total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_year_of_weeks() {
        assert_eq!(weekday(0), 4, "1970-01-01 was a Thursday");
        let today = 20_757; // 2026-10-31, a Saturday
        assert_eq!(ymd(today), (2026, 9, 31));
        assert_eq!(weekday(today), 6);
        let (svg, total) = calendar(&[(today, 3), (today - 1, 1), (today - 400, 9)], today);
        assert_eq!(total, 4, "only the year shown counts");
        assert_eq!(svg.matches("<rect").count() as i64, (WEEKS - 1) * 7 + 7);
        assert!(svg.contains(r#"class="cal-4"><title>3 contributions on Oct 31, 2026</title>"#), "{svg}");
        assert!(svg.contains(r#"class="cal-2"><title>1 contribution on Oct 30, 2026</title>"#));
        assert!(svg.contains("No contributions on"));
        assert!(svg.contains(">Nov</text>") && svg.contains(">Mon</text>"));
        assert_eq!((level(0, 8), level(2, 8), level(4, 8), level(6, 8), level(8, 8)), (0, 1, 2, 3, 4));
    }
}
