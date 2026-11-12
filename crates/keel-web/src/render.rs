//! HTML rendering helpers: syntax highlighting, Markdown, dates and sizes.

use std::sync::LazyLock;

use std::collections::HashMap;

use pulldown_cmark::{CowStr, Event, LinkType, Options, Parser, Tag, TagEnd};
use syntect::highlighting::{Color, FontStyle};
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxSet};
use syntect::util::LinesWithEndings;
use two_face::theme::EmbeddedThemeName;

/// Files larger than this are shown without highlighting: syntect is the
/// slowest part of rendering a page and huge files are rarely read anyway.
const MAX_HIGHLIGHT_BYTES: usize = 256 * 1024;

static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(two_face::syntax::extra_newlines);

/// A theme, read the way a browser read the stylesheet keel sent for it
/// until 0.24.2. Code was marked up with a span for every syntax scope,
/// nested, the scope's parts as classes (`string.quoted.double.c` became
/// `<span class="hl-string hl-quoted hl-double hl-c">`), and the theme was
/// a stylesheet over those classes (`.hl-string { color: … }`). That made
/// a page about 25 times the size of the file it showed.
///
/// Now the same rules are applied here, and only their outcome is sent:
/// a span where the colour changes (see [`Look`]). The outcome is the
/// same for every character, which is why this follows CSS and not the
/// theme format's own (slightly different) rules for which rule wins.
struct Sheet {
    selectors: Vec<Selector>,
    /// For each of the theme's rules: the class of its colour (its place
    /// in `colors`), if it sets one, and its font style.
    rules: Vec<(Option<u8>, FontStyle)>,
    /// The colours the theme gives code, each a class by its place here.
    colors: Vec<Color>,
}

/// One selector of a rule, as in `.hl-meta.hl-mapping .hl-string`.
struct Selector {
    /// The classes the element must have (last), and those ancestors of
    /// it must have, outermost first.
    path: Vec<Vec<String>>,
    /// How many classes that is: what CSS weighs selectors by.
    classes: usize,
    /// Which rule it belongs to: of two selectors that weigh the same,
    /// the later rule's wins.
    rule: usize,
}

/// The classes the element for `scope` had.
fn classes_of(scope: Scope) -> Vec<String> {
    scope.build_string().split('.').map(str::to_owned).collect()
}

impl Selector {
    /// Whether it matches the innermost of these nested elements, each
    /// given by its classes.
    fn matches(&self, elements: &[Vec<String>]) -> bool {
        let has = |element: &Vec<String>, part: &Vec<String>| part.iter().all(|class| element.contains(class));
        let mut parts = self.path.iter().rev();
        let mut elements = elements.iter().rev();
        match (parts.next(), elements.next()) {
            (Some(part), Some(element)) if has(element, part) => {}
            _ => return false,
        }
        // Each earlier part, some ancestor further out than the last.
        parts.all(|part| elements.any(|element| has(element, part)))
    }
}

impl Sheet {
    fn new(name: EmbeddedThemeName) -> Self {
        let themes = two_face::theme::extra();
        let (mut selectors, mut rules, mut colors) = (Vec::new(), Vec::new(), Vec::new());
        for (rule, item) in themes.get(name).scopes.iter().enumerate() {
            let color = item.style.foreground.map(|color| match colors.iter().position(|c| *c == color) {
                Some(class) => class as u8,
                None => {
                    colors.push(color);
                    (colors.len() - 1) as u8
                }
            });
            rules.push((color, item.style.font_style.unwrap_or_default()));
            for selector in &item.scope.selectors {
                let path: Vec<Vec<String>> = selector.extract_scopes().into_iter().map(classes_of).collect();
                if !path.is_empty() {
                    selectors.push(Selector { classes: path.iter().map(Vec::len).sum(), path, rule });
                }
            }
        }
        Self { selectors, rules, colors }
    }

    /// What the rules say about the innermost of these nested elements
    /// itself (not what it inherits): the colour of the rule that wins
    /// there, if any sets one, and the font styles of all that match
    /// (a rule can only add those).
    fn at(&self, elements: &[Vec<String>]) -> (Option<u8>, FontStyle) {
        let mut winner: Option<&Selector> = None;
        let mut font = FontStyle::empty();
        for selector in self.selectors.iter().filter(|s| s.matches(elements)) {
            let (color, style) = self.rules[selector.rule];
            font.insert(style);
            if color.is_some() && winner.is_none_or(|w| (selector.classes, selector.rule) >= (w.classes, w.rule)) {
                winner = Some(selector);
            }
        }
        (winner.and_then(|w| self.rules[w.rule].0), font)
    }
}

/// The themes code is shown in: on light pages and on dark ones.
static LIGHT: LazyLock<Sheet> = LazyLock::new(|| Sheet::new(EmbeddedThemeName::OneHalfLight));
static DARK: LazyLock<Sheet> = LazyLock::new(|| Sheet::new(EmbeddedThemeName::OneHalfDark));

/// Loads the syntax definitions and themes up front so the first request
/// isn't slow.
pub fn warm_up() {
    LazyLock::force(&SYNTAXES);
    LazyLock::force(&LIGHT);
    LazyLock::force(&DARK);
}

/// How a piece of code looks: its colour's class on light pages and on
/// dark ones (`None`: the page's text colour), and its font style.
///
/// It's marked up with just that: `<span class="hl3 hd3">`, the light
/// theme's colour 3 and the dark theme's.
#[derive(Clone, Copy, Default, PartialEq)]
struct Look {
    light: Option<u8>,
    /// `None` also where only the light theme has a colour: that one then
    /// shows on dark pages too (as it did: its rules aren't limited to
    /// light pages).
    dark: Option<u8>,
    font: FontStyle,
    /// What the dark theme adds to `font`.
    dark_font: FontStyle,
}

impl Look {
    /// The look of text inside these nested elements (syntax scopes),
    /// each given by its classes.
    fn of(elements: &[Vec<String>]) -> Self {
        let mut look = Self::default();
        let (mut light_set, mut dark_set) = (false, false);
        // From the innermost out: an element a rule colours hides what
        // those around it would pass on; font styles add up.
        for depth in (1..=elements.len()).rev() {
            let (light, font) = LIGHT.at(&elements[..depth]);
            let (dark, dark_font) = DARK.at(&elements[..depth]);
            look.font.insert(font);
            look.dark_font.insert(dark_font);
            if !light_set && light.is_some() {
                (look.light, light_set) = (light, true);
            }
            if !dark_set && (dark.is_some() || light.is_some()) {
                (look.dark, dark_set) = (dark, true);
            }
        }
        look.dark_font.remove(look.font);
        look
    }

    fn plain(self) -> bool {
        self == Self::default()
    }

    /// Appends the classes, separated by spaces.
    fn classes(self, out: &mut String) {
        use std::fmt::Write;
        let mut class = |name: std::fmt::Arguments<'_>| {
            if !out.ends_with('"') {
                out.push(' ');
            }
            let _ = out.write_fmt(name);
        };
        if let Some(n) = self.light {
            class(format_args!("hl{n}"));
        }
        for (font, letter) in FONTS {
            if self.font.contains(font) {
                class(format_args!("hl{letter}"));
            }
        }
        if let Some(n) = self.dark {
            class(format_args!("hd{n}"));
        }
        for (font, letter) in FONTS {
            if self.dark_font.contains(font) {
                class(format_args!("hd{letter}"));
            }
        }
    }
}

/// Font styles, the letter in their classes (`hlb`, `hdi`) and their CSS.
const FONTS: [(FontStyle, char); 3] = [(FontStyle::BOLD, 'b'), (FontStyle::ITALIC, 'i'), (FontStyle::UNDERLINE, 'u')];
const FONT_CSS: [(char, &str); 3] = [('b', "font-weight: bold"), ('i', "font-style: italic"), ('u', "text-decoration: underline")];

/// One Half Dark, tuned to the dark theme's indigo: cooler keywords,
/// functions and comments, a softer orange for numbers.
fn indigo(color: &str) -> &str {
    const SWAPS: [(&str, &str); 4] = [("#c678dd", "#c39bf0"), ("#61afef", "#7fa8ff"), ("#d19a66", "#e0a46e"), ("#5c6370", "#5f6680")];
    SWAPS.iter().find(|(from, _)| *from == color).map_or(color, |(_, to)| to)
}

fn hex(c: Color) -> String {
    if c.a == 0xff { format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b) } else { format!("#{:02x}{:02x}{:02x}{:02x}", c.r, c.g, c.b, c.a) }
}

/// CSS for highlighted code (see [`Look`]), following the OS light/dark
/// preference unless the page picks one (`data-theme`, like the site's
/// colors). Only token colours: backgrounds come from the site theme.
pub fn highlight_css() -> String {
    let mut light = String::new();
    for (n, color) in LIGHT.colors.iter().enumerate() {
        light.push_str(&format!(".hl{n} {{ color: {}; }}\n", hex(*color)));
    }
    for (letter, style) in FONT_CSS {
        light.push_str(&format!(".hl{letter} {{ {style}; }}\n"));
    }
    let mut dark: Vec<(String, String)> = Vec::new();
    for (n, color) in DARK.colors.iter().enumerate() {
        dark.push((format!(".hd{n}"), format!("color: {}", indigo(&hex(*color)))));
    }
    for (letter, style) in FONT_CSS {
        dark.push((format!(".hd{letter}"), style.to_owned()));
    }
    // With the scope in front these weigh more than the light theme's
    // classes, which a span on a dark page has as well.
    let scoped = |scope: &str| dark.iter().map(|(class, style)| format!("{scope} {class} {{ {style}; }}\n")).collect::<String>();
    format!(
        "{light}@media (prefers-color-scheme: dark) {{\n{}}}\n{}",
        scoped(":root:not([data-theme=\"light\"])"),
        scoped(":root[data-theme=\"dark\"]")
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
    let mut parser = ParseState::new(syntax);
    // The syntax scopes the text at hand is in, outermost first.
    let mut scopes = ScopeStack::new();
    // A file has a few hundred different stacks of them, and thousands of
    // pieces of text: the look of each stack is worked out once.
    let mut looks: HashMap<Vec<Scope>, Look> = HashMap::new();
    let mut lines = Vec::new();
    for line in LinesWithEndings::from(text) {
        let Ok(changes) = parser.parse_line(line, ss) else {
            return plain_lines(text);
        };
        // The line ending isn't shown.
        let shown = line.strip_suffix('\n').unwrap_or(line);
        let shown = shown.strip_suffix('\r').unwrap_or(shown).len();
        let mut html = String::with_capacity(line.len() * 2);
        let (mut at, mut open) = (0, Look::default());
        // The text up to each change of scopes, then the rest of the line.
        for (end, change) in changes.iter().map(|(end, change)| (*end, Some(change))).chain([(line.len(), None)]) {
            let piece = &line[at.min(shown)..end.min(shown)];
            at = end;
            // Blank text looks the same in any colour: no reason to end
            // a span for it.
            if !piece.trim().is_empty() {
                let look = match looks.get(scopes.as_slice()) {
                    Some(look) => *look,
                    None => {
                        let elements: Vec<Vec<String>> = scopes.as_slice().iter().copied().map(classes_of).collect();
                        let look = Look::of(&elements);
                        looks.insert(scopes.as_slice().to_vec(), look);
                        look
                    }
                };
                if look != open {
                    if !open.plain() {
                        html.push_str("</span>");
                    }
                    if !look.plain() {
                        html.push_str("<span class=\"");
                        look.classes(&mut html);
                        html.push_str("\">");
                    }
                    open = look;
                }
            }
            escape_into(&mut html, piece);
            if let Some(change) = change
                && scopes.apply(change).is_err()
            {
                return plain_lines(text);
            }
        }
        if !open.plain() {
            html.push_str("</span>");
        }
        lines.push(html);
    }
    lines
}

pub fn plain_lines(text: &str) -> Vec<String> {
    text.lines().map(escape).collect()
}

pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    escape_into(&mut out, s);
    out
}

fn escape_into(out: &mut String, s: &str) {
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
    sanitize(&html)
}

/// Removes anything unsafe (scripts, event handlers, unknown tags), and
/// sends images on other servers through the image proxy, raw `<img>` tags
/// included.
fn sanitize(html: &str) -> String {
    ammonia::Builder::default()
        .attribute_filter(|element, attribute, value| match (element, attribute) {
            ("img", "src") => Some(crate::images::img_src(value).into()),
            // Its images would load directly, not through the proxy `src` uses.
            ("img", "srcset") => None,
            _ => Some(value.into()),
        })
        .clean(html)
        .to_string()
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
    fn highlight_css_follows_the_theme_switch() {
        let css = highlight_css();
        assert!(css.contains(":root[data-theme=\"dark\"] .hd0 { color: #"), "{css}");
        assert!(css.contains("@media (prefers-color-scheme: dark) {\n:root:not([data-theme=\"light\"]) .hd0 { color: #"), "{css}");
        // The dark theme's code colours are tuned to its indigo.
        let dark = &css[css.find("@media").unwrap()..];
        assert!(dark.contains("#c39bf0") && dark.contains("#5f6680"), "{dark}");
        assert!(!dark.contains("#c678dd") && css.contains(".hl0 { color: #") && css.contains("#a626a4"), "light keeps One Half Light");
        // A class for every colour either theme uses, and no others.
        assert_eq!(css.matches("\n.hl").count() + 1, LIGHT.colors.len() + 3);
        assert!(LIGHT.colors.len() > 5 && DARK.colors.len() > 5);
    }

    #[test]
    fn colours_follow_the_stylesheet_rules() {
        let stack = |scopes: &[&str]| -> Vec<Vec<String>> { scopes.iter().map(|s| s.split('.').map(str::to_owned).collect()).collect() };
        let color = |sheet: &Sheet, scopes: &[&str]| -> Option<String> {
            let elements = stack(scopes);
            (1..=elements.len()).rev().find_map(|depth| sheet.at(&elements[..depth]).0).map(|class| hex(sheet.colors[class as usize]))
        };
        // A rule for the element itself; nothing for scopes no rule names.
        assert_eq!(color(&LIGHT, &["source.c", "comment.block.c"]).as_deref(), Some("#a0a1a7"));
        assert_eq!(color(&LIGHT, &["source.c", "meta.block.c"]), None);
        // An element without a rule of its own takes the colour of the
        // one around it.
        assert_eq!(color(&LIGHT, &["source.c", "string.quoted.double.c", "punctuation.definition.string.begin.c"]).as_deref(), Some("#50a14f"));
        // The innermost element with a rule decides, whatever is around.
        let inner = color(&LIGHT, &["source.shell", "string.quoted.double.shell", "variable.other.readwrite.shell"]);
        assert_eq!(inner, color(&LIGHT, &["source.shell", "variable.other.readwrite.shell"]));
        assert_ne!(inner.as_deref(), Some("#50a14f"), "not the string's");
        // Classes match in any order and however many more there are, as
        // in CSS: `.hl-entity.hl-name` on `entity.other.inherited-class.name`.
        let dark = Look::of(&stack(&["source.c", "meta.preprocessor.macro.c", "entity.name.constant.preprocessor.c"]));
        assert!(dark.dark.is_some(), "the dark theme colours every entity.name");
        // The look says where the themes differ.
        assert!(Look::of(&stack(&["source.c", "meta.block.c"])).plain());
    }

    #[test]
    fn highlighted_lines_are_balanced() {
        let src = "/* a\n multi-line comment */\nfn main() {\r\n    let s = \"<b>\";\n}\n";
        let lines = highlight_lines("main.rs", src);
        assert_eq!(lines.len(), 5);
        for l in &lines {
            assert_eq!(l.matches("<span").count(), l.matches("</span>").count(), "{l}");
            assert!(!l.contains('\n') && !l.contains('\r'), "{l:?}");
        }
        // A comment that spans lines is one span on each, in the same
        // colour, and nothing else.
        let comment = lines[0].split_once('>').unwrap().0;
        assert!(comment.starts_with("<span class=\"hl") && comment.contains(" hd"), "{comment}");
        assert_eq!(lines[0], format!("{comment}>/* a</span>"));
        assert_eq!(lines[1], format!("{comment}> multi-line comment */</span>"));
        assert!(lines[3].contains("&lt;b&gt;"));
        assert!(!lines.concat().contains("<b>"));
        // Only classes the stylesheet has.
        let css = highlight_css();
        for class in lines.concat().split('"').filter(|c| c.starts_with("hl") || c.starts_with("hd")).flat_map(|c| c.split(' ')) {
            assert!(css.contains(&format!(".{class} {{")), "{class}");
        }
        // The text is all there, in order.
        let text: String = lines.iter().map(|l| {
            let mut out = String::new();
            let mut in_tag = false;
            for c in l.chars() {
                match c {
                    '<' => in_tag = true,
                    '>' => in_tag = false,
                    c if !in_tag => out.push(c),
                    _ => {}
                }
            }
            out + "\n"
        }).collect();
        assert_eq!(text, "/* a\n multi-line comment */\nfn main() {\n    let s = &quot;&lt;b&gt;&quot;;\n}\n");
    }

    #[test]
    fn highlighting_is_compact() {
        // Much smaller than a span for every syntax scope, which took
        // about 650 bytes for a line like these.
        let src = "static int count(const char *s)\n{\n\tint n = 0;\n\twhile (*s++)\n\t\tn++;\n\treturn n;\n}\n".repeat(50);
        let lines = highlight_lines("count.c", &src);
        assert_eq!(lines.len(), 350);
        let html: usize = lines.iter().map(String::len).sum();
        assert!(html < src.len() * 6, "{html} bytes for {} of code: {}", src.len(), lines[..7].join("\n"));
        assert!(lines[2].contains("<span class=\"hl"), "{}", lines[2]);
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
    fn images_elsewhere_go_through_the_proxy() {
        let base = LinkBase { blob: "/a/r/blob/main", raw: "/a/r/raw/main", dir: "" };
        let html = markdown(
            "![ci](https://ci.example.com/badge.svg) ![local](logo.png)\n\n\
             <img src=\"//tracker.example/pixel.gif\" srcset=\"https://tracker.example/2x.gif 2x\">",
            &base,
        );
        assert!(html.contains(r#"src="/_image?url=https%3A%2F%2Fci.example.com%2Fbadge.svg&amp;sig="#), "{html}");
        assert!(html.contains(r#"src="/_image?url=https%3A%2F%2Ftracker.example%2Fpixel.gif&amp;sig="#), "{html}");
        assert!(html.contains(r#"src="/a/r/raw/main/logo.png""#));
        assert!(!html.contains("srcset") && !html.contains("\"https://") && !html.contains("\"//"), "{html}");
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
