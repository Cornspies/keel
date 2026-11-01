//! The sidebar of a repository's front page: about (description, topics,
//! license, counts), the latest release, the languages used and the
//! contributors.

use std::sync::Arc;

use keel_core::Repo;
use keel_git::Oid;

use crate::render::{datetime, relative_time};
use crate::repo::{RepoView, encode_path};
use crate::{AppResult, AppState};

/// Contributors listed by name; the rest are counted.
const CONTRIBUTORS_SHOWN: usize = 8;
/// Languages listed by name; the rest are "Other".
const LANGUAGES_SHOWN: usize = 6;

/// Programming and markup languages by file extension, with the colours
/// GitHub's linguist gives them. Data, prose and configuration (JSON,
/// YAML, Markdown, …) don't count, as on GitHub.
const LANGUAGES: &[(&str, &str, &[&str])] = &[
    ("Rust", "#dea584", &["rs"]),
    ("Go", "#00add8", &["go"]),
    ("Python", "#3572a5", &["py", "pyi", "pyw"]),
    ("JavaScript", "#f1e05a", &["js", "mjs", "cjs", "jsx"]),
    ("TypeScript", "#3178c6", &["ts", "tsx", "mts", "cts"]),
    ("Java", "#b07219", &["java"]),
    ("Kotlin", "#a97bff", &["kt", "kts"]),
    ("Swift", "#f05138", &["swift"]),
    ("C", "#555555", &["c", "h"]),
    ("C++", "#f34b7d", &["cpp", "cc", "cxx", "hpp", "hh", "hxx", "ipp"]),
    ("C#", "#178600", &["cs"]),
    ("Objective-C", "#438eff", &["m", "mm"]),
    ("Ruby", "#701516", &["rb", "rake", "gemspec"]),
    ("PHP", "#4f5d95", &["php"]),
    ("Shell", "#89e051", &["sh", "bash", "zsh", "fish"]),
    ("PowerShell", "#012456", &["ps1", "psm1"]),
    ("Batchfile", "#c1f12e", &["bat", "cmd"]),
    ("HTML", "#e34c26", &["html", "htm", "xhtml"]),
    ("CSS", "#663399", &["css"]),
    ("SCSS", "#c6538c", &["scss"]),
    ("Less", "#1d365d", &["less"]),
    ("Vue", "#41b883", &["vue"]),
    ("Svelte", "#ff3e00", &["svelte"]),
    ("Astro", "#ff5a03", &["astro"]),
    ("Dart", "#00b4ab", &["dart"]),
    ("Scala", "#c22d40", &["scala", "sc"]),
    ("Haskell", "#5e5086", &["hs", "lhs"]),
    ("Elixir", "#6e4a7e", &["ex", "exs"]),
    ("Erlang", "#b83998", &["erl", "hrl"]),
    ("Clojure", "#db5855", &["clj", "cljs", "cljc", "edn"]),
    ("OCaml", "#ef7a08", &["ml", "mli"]),
    ("F#", "#b845fc", &["fs", "fsi", "fsx"]),
    ("Elm", "#60b5cc", &["elm"]),
    ("Lua", "#000080", &["lua"]),
    ("Perl", "#0298c3", &["pl", "pm"]),
    ("R", "#198ce7", &["r"]),
    ("Julia", "#a270ba", &["jl"]),
    ("Zig", "#ec915c", &["zig"]),
    ("Nim", "#ffc200", &["nim"]),
    ("Crystal", "#000100", &["cr"]),
    ("Nix", "#7e7eff", &["nix"]),
    ("Groovy", "#4298b8", &["groovy", "gradle"]),
    ("Solidity", "#aa6746", &["sol"]),
    ("HCL", "#844fba", &["tf", "hcl"]),
    ("CMake", "#da3434", &["cmake"]),
    ("Makefile", "#427819", &["mk", "mak"]),
    ("Dockerfile", "#384d54", &["dockerfile"]),
    ("Assembly", "#6e4c13", &["asm", "s"]),
    ("Vim Script", "#199f4b", &["vim"]),
    ("TeX", "#3d6117", &["tex", "sty", "cls"]),
    ("GLSL", "#5686a5", &["glsl", "vert", "frag"]),
    ("WebAssembly", "#04133b", &["wat"]),
];

/// The language of a file, by its name (some have no extension) or
/// extension.
fn language_of(path: &str) -> Option<(&'static str, &'static str)> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let by_name = match name {
        "Makefile" | "makefile" | "GNUmakefile" => Some("Makefile"),
        "Dockerfile" | "Containerfile" => Some("Dockerfile"),
        "CMakeLists.txt" => Some("CMake"),
        "Rakefile" | "Gemfile" => Some("Ruby"),
        _ => None,
    };
    if let Some(found) = by_name {
        return LANGUAGES.iter().find(|(n, _, _)| *n == found).map(|(n, c, _)| (*n, *c));
    }
    let (_, ext) = name.rsplit_once('.')?;
    let ext = ext.to_ascii_lowercase();
    LANGUAGES.iter().find(|(_, _, exts)| exts.contains(&ext.as_str())).map(|(n, c, _)| (*n, *c))
}

/// Paths that aren't the project's own code, which linguist leaves out
/// too: dependencies, build output, minified files and documentation.
fn excluded(path: &str) -> bool {
    const DIRS: &[&str] = &["node_modules", "vendor", "third_party", "third-party", "bower_components", "dist", "docs", "doc", "Documentation"];
    let mut parts = path.split('/');
    let name = parts.next_back().unwrap_or_default();
    parts.any(|d| DIRS.contains(&d)) || name.contains(".min.") || name.ends_with(".pb.go") || name.ends_with("_pb2.py")
}

/// A language's share of the code.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Language {
    pub name: &'static str,
    pub color: &'static str,
    pub bytes: u64,
}

/// Bytes per language, largest first.
pub(crate) fn breakdown(files: &[(String, u64)]) -> Vec<Language> {
    let mut by: Vec<Language> = Vec::new();
    for (path, size) in files {
        if excluded(path) {
            continue;
        }
        let Some((name, color)) = language_of(path) else { continue };
        match by.iter_mut().find(|l| l.name == name) {
            Some(l) => l.bytes += size,
            None => by.push(Language { name, color, bytes: *size }),
        }
    }
    by.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.name.cmp(b.name)));
    by
}

/// The license a `LICENSE` file holds, by its wording; `None` if it's not
/// one of the common ones.
pub(crate) fn license_name(text: &str) -> Option<&'static str> {
    let t: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let has = |s: &str| t.contains(s);
    Some(match () {
        _ if has("GNU AFFERO GENERAL PUBLIC LICENSE") => "AGPL-3.0",
        _ if has("GNU LESSER GENERAL PUBLIC LICENSE") => {
            if has("Version 2.1") { "LGPL-2.1" } else { "LGPL-3.0" }
        }
        _ if has("GNU GENERAL PUBLIC LICENSE") => {
            if has("Version 3") { "GPL-3.0" } else { "GPL-2.0" }
        }
        _ if has("Apache License") && has("Version 2.0") => "Apache-2.0",
        _ if has("Mozilla Public License Version 2.0") || has("Mozilla Public License, version 2.0") => "MPL-2.0",
        _ if has("Permission is hereby granted, free of charge") => "MIT",
        _ if has("Permission to use, copy, modify, and/or distribute this software for any purpose") => "ISC",
        _ if has("Redistribution and use in source and binary forms") => {
            if has("Neither the name") || has("names of its contributors") { "BSD-3-Clause" } else { "BSD-2-Clause" }
        }
        _ if has("This is free and unencumbered software released into the public domain") => "Unlicense",
        _ if has("Boost Software License") => "BSL-1.0",
        _ if has("CC0 1.0 Universal") => "CC0-1.0",
        _ => return None,
    })
}

/// Whether a file at the top of the tree is the license.
pub(crate) fn is_license_file(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let stem = lower.split('.').next().unwrap_or_default();
    matches!(stem, "license" | "licence" | "copying" | "unlicense") && !lower.ends_with(".rs")
}

pub(crate) struct LanguageView {
    pub name: &'static str,
    pub color: &'static str,
    pub percent: String,
}

pub(crate) struct PersonView {
    pub name: String,
    pub username: Option<String>,
    pub initial: String,
    pub commits: i64,
}

pub(crate) struct ReleaseView {
    pub name: String,
    pub tag: String,
    pub url: String,
    pub when: String,
    pub when_full: String,
}

/// Everything the sidebar shows.
pub(crate) struct RepoSide {
    pub readme: bool,
    /// The license's short name (or "License" if unknown) and its file.
    pub license: Option<(String, String)>,
    pub latest_release: Option<ReleaseView>,
    pub new_release_url: Option<String>,
    pub languages: Vec<LanguageView>,
    /// The bar above the list (inline SVG: style attributes are blocked by
    /// the Content Security Policy).
    pub language_bar: String,
    pub people: Vec<PersonView>,
    pub people_total: usize,
}

/// The languages at `commit`, cached by commit.
pub(crate) async fn languages(state: &AppState, repo: &Repo, commit: Oid) -> AppResult<Arc<Vec<Language>>> {
    if let Some(hit) = state.languages.get(&commit) {
        return Ok(hit);
    }
    let files = crate::repo::with_git(state, repo, move |g| Ok(g.file_sizes(commit)?)).await?;
    let langs = Arc::new(breakdown(&files));
    state.languages.insert(commit, langs.clone());
    Ok(langs)
}

/// Builds the sidebar for the front page of `repo` at `commit`.
pub(crate) async fn load(
    state: &AppState,
    repo: &Repo,
    view: &RepoView,
    rev: &str,
    commit: Oid,
    readme: bool,
    license: Option<(String, Option<&'static str>)>,
) -> AppResult<RepoSide> {
    let base = format!("/{}/{}", repo.owner, repo.name);
    let latest_release = state.core.latest_release(repo.id).await?.map(|r| {
        let at = r.published_at.unwrap_or(r.created_at);
        ReleaseView {
            name: r.name().to_owned(),
            url: format!("{base}/releases/tag/{}", encode_path(&r.tag)),
            tag: r.tag,
            when: relative_time(at),
            when_full: datetime(at),
        }
    });

    let langs = languages(state, repo, commit).await?;
    let total: u64 = langs.iter().map(|l| l.bytes).sum::<u64>().max(1);
    let mut shown: Vec<Language> = langs.iter().take(LANGUAGES_SHOWN).cloned().collect();
    let rest: u64 = langs.iter().skip(LANGUAGES_SHOWN).map(|l| l.bytes).sum();
    if rest > 0 {
        shown.push(Language { name: "Other", color: "#8b949e", bytes: rest });
    }
    let percent = |b: u64| b as f64 * 100.0 / total as f64;
    let mut x = 0.0;
    let mut rects = String::new();
    for l in &shown {
        let w = percent(l.bytes);
        rects.push_str(&format!(r#"<rect x="{x:.3}" width="{w:.3}" height="8" fill="{}"/>"#, l.color));
        x += w;
    }
    let label: Vec<String> = shown.iter().map(|l| format!("{} {:.1}%", l.name, percent(l.bytes))).collect();
    let language_bar = if shown.is_empty() {
        String::new()
    } else {
        format!(
            r#"<svg class="lang-bar" viewBox="0 0 100 8" preserveAspectRatio="none" role="img" aria-label="{}">{rects}</svg>"#,
            label.join(", ")
        )
    };
    let languages = shown
        .iter()
        .map(|l| {
            let p = percent(l.bytes);
            LanguageView { name: l.name, color: l.color, percent: if p < 0.1 { "<0.1%".into() } else { format!("{p:.1}%") } }
        })
        .collect();

    let (mut people, mut people_total) = (Vec::new(), 0);
    if let Some(a) = state.core.repo_activity(repo).await? {
        people_total = a.people.len();
        let top = a.top(CONTRIBUTORS_SHOWN);
        let emails: Vec<String> = top.iter().map(|c| c.email.to_lowercase()).collect();
        let accounts = state.core.commit_authors(&emails.iter().map(String::as_str).collect::<Vec<_>>()).await?;
        people = top
            .iter()
            .zip(&emails)
            .map(|(c, e)| PersonView {
                initial: c.name.chars().next().map(|ch| ch.to_uppercase().to_string()).unwrap_or_else(|| "?".into()),
                name: c.name.clone(),
                username: accounts.get(e).cloned(),
                commits: c.commits,
            })
            .collect();
    }

    Ok(RepoSide {
        readme,
        license: license.map(|(file, name)| {
            (name.map_or_else(|| "License".to_owned(), |n| format!("{n} license")), format!("{base}/blob/{}/{}", encode_path(rev), encode_path(&file)))
        }),
        latest_release,
        new_release_url: (view.can_write && view.mirror.is_none()).then(|| format!("{base}/releases/new")),
        languages,
        language_bar,
        people,
        people_total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn languages_by_file() {
        let files: Vec<(String, u64)> = [
            ("src/main.rs", 900),
            ("src/lib.rs", 100),
            ("web/app.ts", 400),
            ("web/node_modules/x/index.js", 99_999),
            ("static/app.min.js", 5000),
            ("README.md", 7000),
            ("Cargo.toml", 300),
            ("Makefile", 100),
            ("docs/guide.py", 800),
        ]
        .map(|(p, s)| (p.to_owned(), s))
        .into();
        let b = breakdown(&files);
        let names: Vec<(&str, u64)> = b.iter().map(|l| (l.name, l.bytes)).collect();
        assert_eq!(names, [("Rust", 1000), ("TypeScript", 400), ("Makefile", 100)]);
        assert_eq!(b[0].color, "#dea584");
        assert_eq!(language_of("Dockerfile").unwrap().0, "Dockerfile");
        assert_eq!(language_of("x/HEADER.H").unwrap().0, "C");
        assert_eq!(language_of("notes.txt"), None);
    }

    #[test]
    fn licenses() {
        assert_eq!(license_name("MIT License\n\nPermission is hereby granted, free of\ncharge, to any person"), Some("MIT"), "however it's wrapped");
        assert_eq!(license_name("Permission is hereby granted, free of charge, to any person obtaining a copy"), Some("MIT"));
        assert_eq!(license_name("Apache License\n                           Version 2.0, January 2004"), Some("Apache-2.0"));
        assert_eq!(license_name("GNU GENERAL PUBLIC LICENSE\n Version 3, 29 June 2007"), Some("GPL-3.0"));
        assert_eq!(license_name("GNU LESSER GENERAL PUBLIC LICENSE Version 2.1"), Some("LGPL-2.1"));
        assert_eq!(license_name("Our own terms."), None);
        assert!(is_license_file("LICENSE") && is_license_file("LICENSE.md") && is_license_file("COPYING"));
        assert!(!is_license_file("license.rs") && !is_license_file("README.md"));
    }
}
