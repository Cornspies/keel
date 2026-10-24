//! HTML rendering helpers: syntax highlighting, Markdown, dates and sizes.

use std::sync::LazyLock;

use pulldown_cmark::{CowStr, Event, LinkType, Options, Parser, Tag, TagEnd};
use syntect::html::{ClassStyle, ClassedHTMLGenerator, css_for_theme_with_class_style};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;
use two_face::theme::EmbeddedThemeName;

const CLASS_STYLE: ClassStyle = ClassStyle::SpacedPrefixed { prefix: "hl-" };

/// Files larger than this are shown without highlighting: syntect is the
/// slowest part of rendering a page and huge files are rarely read anyway.
const MAX_HIGHLIGHT_BYTES: usize = 256 * 1024;

static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(two_face::syntax::extra_newlines);

/// Loads the syntax definitions up front so the first request isn't slow.
pub fn warm_up() {
    LazyLock::force(&SYNTAXES);
}

/// CSS for highlighted code, following the OS light/dark preference.
pub fn highlight_css() -> String {
    let themes = two_face::theme::extra();
    let light = css_for_theme_with_class_style(themes.get(EmbeddedThemeName::OneHalfLight), CLASS_STYLE)
        .unwrap_or_default();
    let dark = css_for_theme_with_class_style(themes.get(EmbeddedThemeName::OneHalfDark), CLASS_STYLE)
        .unwrap_or_default();
    // Only token colours: backgrounds come from the site theme.
    let strip = |css: String| {
        css.lines()
            .filter(|l| !l.trim_start().starts_with("background-color"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "{}\n@media (prefers-color-scheme: dark) {{\n{}\n}}\n",
        strip(light),
        strip(dark)
    )
}

/// Highlights `text` and returns one self-contained HTML fragment per line,
/// so lines can be put in table rows with line numbers.
pub fn highlight_lines(filename: &str, text: &str) -> Vec<String> {
    if text.len() > MAX_HIGHLIGHT_BYTES {
        return plain_lines(text);
    }
    let ss = &*SYNTAXES;
    let ext = filename.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    let syntax = ss
        .find_syntax_by_extension(filename)
        .or_else(|| ss.find_syntax_by_extension(ext))
        .or_else(|| ss.find_syntax_by_first_line(text.lines().next().unwrap_or("")));
    let Some(syntax) = syntax else {
        return plain_lines(text);
    };
    let mut generator = ClassedHTMLGenerator::new_with_class_style(syntax, ss, CLASS_STYLE);
    for line in LinesWithEndings::from(text) {
        if generator
            .parse_html_for_line_which_includes_newline(line)
            .is_err()
        {
            return plain_lines(text);
        }
    }
    split_html_lines(&generator.finalize())
}

pub fn plain_lines(text: &str) -> Vec<String> {
    text.lines().map(escape).collect()
}

/// Splits generated HTML at newlines, closing spans that are open at the end
/// of a line and reopening them on the next, so every line is valid HTML.
fn split_html_lines(html: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut open: Vec<&str> = Vec::new();
    let mut line = String::new();
    let mut rest = html;
    while !rest.is_empty() {
        if rest.starts_with("</span>") {
            open.pop();
            line.push_str("</span>");
            rest = &rest["</span>".len()..];
        } else if rest.starts_with("<span")
            && let Some(end) = rest.find('>')
        {
            let tag = &rest[..=end];
            open.push(tag);
            line.push_str(tag);
            rest = &rest[end + 1..];
        } else if let Some(stripped) = rest.strip_prefix('\n') {
            line.extend(std::iter::repeat_n("</span>", open.len()));
            lines.push(std::mem::take(&mut line));
            line.extend(open.iter().copied());
            rest = stripped;
        } else {
            let next = rest[1..]
                .find(['<', '\n'])
                .map_or(rest.len(), |i| i + 1);
            line.push_str(&rest[..next]);
            rest = &rest[next..];
        }
    }
    // Like `str::lines`: what follows the final newline only counts as a
    // line if it has text (and not just tags).
    if has_text(&line) {
        line.extend(std::iter::repeat_n("</span>", open.len()));
        lines.push(line);
    }
    lines
}

fn has_text(html: &str) -> bool {
    let mut in_tag = false;
    html.chars().any(|c| {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => return true,
            _ => {}
        }
        false
    })
}

pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Where relative links in a Markdown file should point.
pub struct LinkBase<'a> {
    /// e.g. `/ada/engine/blob/main`
    pub blob: &'a str,
    /// e.g. `/ada/engine/raw/main`
    pub raw: &'a str,
    /// Directory of the Markdown file inside the repo, e.g. `docs`.
    pub dir: &'a str,
}

/// Renders Markdown to sanitized HTML. Relative links are rewritten to point
/// into the repository, relative images to raw file URLs.
pub fn markdown(src: &str, base: &LinkBase<'_>) -> String {
    render_markdown(src, base, None)
}

/// Markdown for issues and comments: additionally links `#123` to issues
/// and pull requests of the repository at `repo_url`, and `@name` to
/// profiles.
pub fn markdown_with_refs(src: &str, base: &LinkBase<'_>, repo_url: &str) -> String {
    render_markdown(src, base, Some(repo_url))
}

/// Markdown for wiki pages: relative links and `[[Page]]` or
/// `[[Text|Page]]` (as on GitHub) go to pages of the wiki at `wiki_url`,
/// relative images to its files at `raw_url`.
pub fn wiki_markdown(src: &str, wiki_url: &str, raw_url: &str) -> String {
    let base = LinkBase { blob: wiki_url, raw: raw_url, dir: "" };
    let options = MARKDOWN_OPTIONS | Options::ENABLE_WIKILINKS;
    let events = wiki_links(Parser::new_ext(src, options), wiki_url);
    finish_markdown(events.into_iter(), &base)
}

/// A wiki link's target: the page `name` (spaces as in titles), maybe
/// with an `#anchor`.
fn wiki_href(wiki_url: &str, target: &str) -> String {
    let (name, anchor) = match target.split_once('#') {
        Some((n, a)) => (n, format!("#{a}")),
        None => (target, String::new()),
    };
    let name = name.split_whitespace().collect::<Vec<_>>().join("-");
    if name.is_empty() {
        return anchor;
    }
    format!("{wiki_url}/{}{anchor}", crate::repo::encode_path(&name.replace('/', "-")))
}

/// Resolves wiki links. pulldown-cmark reads `[[a|b]]` the Obsidian way
/// (page `a`, text `b`); wikis from GitHub mean the opposite.
fn wiki_links<'a>(events: impl Iterator<Item = Event<'a>>, wiki_url: &str) -> Vec<Event<'a>> {
    let mut out = Vec::new();
    let mut events = events.into_iter();
    while let Some(event) = events.next() {
        let Event::Start(Tag::Link { link_type: LinkType::WikiLink { has_pothole }, dest_url, title, id }) = event else {
            out.push(event);
            continue;
        };
        let mut inner = Vec::new();
        for e in events.by_ref() {
            if matches!(e, Event::End(TagEnd::Link)) {
                break;
            }
            inner.push(e);
        }
        let (target, text) = if has_pothole {
            let target: String = inner
                .iter()
                .filter_map(|e| match e {
                    Event::Text(t) | Event::Code(t) => Some(t.as_ref()),
                    _ => None,
                })
                .collect();
            (target, vec![Event::Text(dest_url)])
        } else {
            (dest_url.to_string(), inner)
        };
        let href = wiki_href(wiki_url, target.trim());
        out.push(Event::Start(Tag::Link { link_type: LinkType::Inline, dest_url: href.into(), title, id }));
        out.extend(text);
        out.push(Event::End(TagEnd::Link));
    }
    out
}

const MARKDOWN_OPTIONS: Options = Options::ENABLE_TABLES
    .union(Options::ENABLE_STRIKETHROUGH)
    .union(Options::ENABLE_TASKLISTS)
    .union(Options::ENABLE_FOOTNOTES)
    .union(Options::ENABLE_HEADING_ATTRIBUTES);

fn render_markdown(src: &str, base: &LinkBase<'_>, repo_url: Option<&str>) -> String {
    let options = MARKDOWN_OPTIONS;
    // Inside code and links, `#1` and `@x` are left alone.
    let mut verbatim = 0usize;
    let events = Parser::new_ext(src, options).flat_map(move |event| {
        match &event {
            Event::Start(Tag::CodeBlock(_) | Tag::Link { .. }) => verbatim += 1,
            Event::End(pulldown_cmark::TagEnd::CodeBlock | pulldown_cmark::TagEnd::Link) => {
                verbatim = verbatim.saturating_sub(1)
            }
            _ => {}
        }
        match (event, repo_url) {
            (Event::Text(text), Some(url)) if verbatim == 0 => linkify(&text, url),
            (e, _) => vec![e],
        }
    });
    finish_markdown(events, base)
}

/// Rewrites relative links and images, renders and sanitizes.
fn finish_markdown<'a>(events: impl Iterator<Item = Event<'a>>, base: &LinkBase<'_>) -> String {
    let rewrite = |url: CowStr<'static>, prefix: &str| -> CowStr<'static> {
        match resolve_relative(base.dir, &url) {
            Some(path) => format!("{prefix}/{path}").into(),
            None => url,
        }
    };
    let events = events.map(|event| match event {
        Event::Start(Tag::Link { link_type, dest_url, title, id }) => Event::Start(Tag::Link {
            link_type,
            dest_url: rewrite(dest_url.into_static(), base.blob),
            title,
            id,
        }),
        Event::Start(Tag::Image { link_type, dest_url, title, id }) => Event::Start(Tag::Image {
            link_type,
            dest_url: rewrite(dest_url.into_static(), base.raw),
            title,
            id,
        }),
        e => e,
    });
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, events);
    ammonia::clean(&html)
}

/// Splits text into plain text and links for `#123` and `@name`.
fn linkify(text: &str, repo_url: &str) -> Vec<Event<'static>> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut plain_start = 0;
    let mut i = 0;
    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'&' || b == b'/';
    while i < bytes.len() {
        let after_boundary = i == 0 || !word(bytes[i - 1]);
        let rest = &bytes[i + 1..];
        let (len, html) = match bytes[i] {
            b'#' if after_boundary => {
                let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
                let ends_cleanly = rest.get(digits).is_none_or(|b| !b.is_ascii_alphanumeric());
                if (1..=9).contains(&digits) && rest[0] != b'0' && ends_cleanly {
                    let n = &text[i + 1..i + 1 + digits];
                    (digits + 1, format!(r#"<a href="{repo_url}/issues/{n}">#{n}</a>"#))
                } else {
                    (0, String::new())
                }
            }
            b'@' if after_boundary => {
                let name_len = rest
                    .iter()
                    .take_while(|b| b.is_ascii_alphanumeric() || **b == b'-')
                    .count()
                    .min(39);
                let name = text[i + 1..i + 1 + name_len].trim_end_matches('-');
                if !name.is_empty() && !name.starts_with('-') {
                    (name.len() + 1, format!(r#"<a href="/{name}">@{name}</a>"#))
                } else {
                    (0, String::new())
                }
            }
            _ => (0, String::new()),
        };
        if len == 0 {
            i += 1;
            continue;
        }
        if plain_start < i {
            out.push(Event::Text(text[plain_start..i].to_owned().into()));
        }
        out.push(Event::InlineHtml(html.into()));
        i += len;
        plain_start = i;
    }
    if plain_start < text.len() {
        out.push(Event::Text(text[plain_start..].to_owned().into()));
    }
    out
}

/// Resolves a relative URL against a directory in the repository. Returns
/// `None` for absolute URLs, fragments and paths escaping the root.
fn resolve_relative(dir: &str, url: &str) -> Option<String> {
    if url.is_empty()
        || url.starts_with(['#', '/', '?'])
        || url.contains("://")
        || url.starts_with("mailto:")
        || url.starts_with("data:")
    {
        return None;
    }
    let (path, suffix) = match url.find(['#', '?']) {
        Some(i) => url.split_at(i),
        None => (url, ""),
    };
    let mut parts: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            s => parts.push(s),
        }
    }
    Some(format!("{}{suffix}", parts.join("/")))
}

/// "3 hours ago", "yesterday", "on 2024-03-01".
pub fn relative_time(ts: i64) -> String {
    let delta = crate::now() - ts;
    let plural = |n: i64, unit: &str| {
        if n == 1 {
            format!("1 {unit} ago")
        } else {
            format!("{n} {unit}s ago")
        }
    };
    match delta {
        d if d < 45 => "just now".into(),
        d if d < 3600 => plural((d + 30) / 60, "minute"),
        d if d < 86400 => plural(d / 3600, "hour"),
        d if d < 2 * 86400 => "yesterday".into(),
        d if d < 30 * 86400 => plural(d / 86400, "day"),
        _ => format!("on {}", date(ts)),
    }
}

pub fn date(ts: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(ts)
        .ok()
        .and_then(|t| t.format(time::macros::format_description!("[year]-[month]-[day]")).ok())
        .unwrap_or_default()
}

pub fn datetime(ts: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(ts)
        .ok()
        .and_then(|t| {
            t.format(time::macros::format_description!(
                "[year]-[month]-[day] [hour]:[minute] UTC"
            ))
            .ok()
        })
        .unwrap_or_default()
}

pub fn human_size(bytes: usize) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} bytes");
    }
    let mut size = bytes as f64 / 1024.0;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlighted_lines_are_balanced() {
        let src = "/* a\n multi-line comment */\nfn main() {\n    let s = \"<b>\";\n}\n";
        let lines = highlight_lines("main.rs", src);
        assert_eq!(lines.len(), 5);
        for l in &lines {
            assert_eq!(l.matches("<span").count(), l.matches("</span>").count(), "{l}");
        }
        assert!(lines[1].contains("comment"));
        assert!(lines[3].contains("&lt;b&gt;"));
        assert!(!lines.concat().contains("<b>"));
    }

    #[test]
    fn unknown_and_huge_files_are_escaped() {
        assert_eq!(highlight_lines("x.unknownext", "<a>\nb"), ["&lt;a&gt;", "b"]);
        let big = "x\n".repeat(MAX_HIGHLIGHT_BYTES);
        assert_eq!(highlight_lines("a.rs", &big).len(), MAX_HIGHLIGHT_BYTES);
    }

    #[test]
    fn markdown_links_and_sanitizing() {
        let base = LinkBase {
            blob: "/ada/engine/blob/main",
            raw: "/ada/engine/raw/main",
            dir: "docs",
        };
        let html = markdown(
            "[a](guide.md#x) [b](../LICENSE) [c](https://x.org) [d](#top) ![i](img/logo.png)\n\n<script>alert(1)</script>",
            &base,
        );
        assert!(html.contains(r#"href="/ada/engine/blob/main/docs/guide.md#x""#), "{html}");
        assert!(html.contains(r#"href="/ada/engine/blob/main/LICENSE""#));
        assert!(html.contains(r#"href="https://x.org""#));
        assert!(html.contains(r##"href="#top""##));
        assert!(html.contains(r#"src="/ada/engine/raw/main/docs/img/logo.png""#));
        assert!(!html.contains("<script"));
        assert_eq!(resolve_relative("", "../../etc/passwd"), None);
    }

    #[test]
    fn wiki_pages() {
        let html = wiki_markdown(
            "[[Getting Started]] [[the setup|Setup Guide#install]] [x](Other-Page) ![i](img/a.png) [[#top]]",
            "/a/r/wiki",
            "/a/r/wiki/_raw",
        );
        assert!(html.contains(r#"href="/a/r/wiki/Getting-Started" rel="noopener noreferrer">Getting Started</a>"#), "{html}");
        assert!(html.contains(r#"href="/a/r/wiki/Setup-Guide#install" rel="noopener noreferrer">the setup</a>"#), "{html}");
        assert!(html.contains(r#"href="/a/r/wiki/Other-Page""#));
        assert!(html.contains(r#"src="/a/r/wiki/_raw/img/a.png""#));
        assert!(html.contains(r##"href="#top""##), "{html}");
        assert!(wiki_markdown("`[[x]]`", "/w", "/r").contains("<code>[[x]]</code>"));
    }

    #[test]
    fn issue_references_and_mentions() {
        let base = LinkBase { blob: "/a/r/blob/main", raw: "/a/r/raw/main", dir: "" };
        let html = markdown_with_refs(
            "Fixes #12, see @bob-x. Not #0, a#3, x@y.org, `#4` or [#5](https://x.org).\n\n```\n#6\n```",
            &base,
            "/a/r",
        );
        assert!(html.contains(r#"href="/a/r/issues/12" rel="noopener noreferrer">#12</a>"#), "{html}");
        assert!(html.contains(r#"href="/bob-x" rel="noopener noreferrer">@bob-x</a>."#), "{html}");
        for not in ["issues/0", "issues/3", "/y.org", "issues/4", "issues/5", "issues/6"] {
            assert!(!html.contains(not), "{not} in {html}");
        }
        // Plain Markdown rendering leaves references alone.
        assert!(!markdown("#12 @bob", &base).contains("<a"));
    }

    #[test]
    fn sizes() {
        assert_eq!(human_size(10), "10 bytes");
        assert_eq!(human_size(1536), "1.5 KiB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MiB");
    }
}
