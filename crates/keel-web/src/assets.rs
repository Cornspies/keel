//! CSS, JS and the icon are compiled into the binary, and compressed once
//! at startup. URLs carry a content hash, so browsers can cache them
//! forever.

use std::collections::HashMap;
use std::io::Write;
use std::sync::OnceLock;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::{IntoResponse, Redirect, Response};
use bytes::Bytes;
use sha2::{Digest, Sha256};

use crate::{AppError, AppState};

const STYLE: &str = include_str!("../static/style.css");
const SCRIPT: &str = include_str!("../static/app.js");
/// The icon in browser tabs: a "k" on the accent colour.
const FAVICON: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32"><rect width="32" height="32" rx="7" fill="#5566e6"/><path d="M11 7.5v17M21.5 8.5 12 17l9.5 7.5" fill="none" stroke="#fff" stroke-width="3.4" stroke-linecap="round" stroke-linejoin="round"/></svg>"##;
/// Which links Chromium-based browsers may load while the pointer rests on
/// them (the Speculation Rules API), so the page is there by the click.
/// Only pages that change nothing when loaded: not signing in or out,
/// downloads (which are counted), raw files, git, or `nofollow` links
/// (blame, which is costly).
const SPECULATION_RULES: &str = r#"{"prefetch":[{"where":{"and":[
  {"href_matches":"/*"},
  {"not":{"href_matches":["/login*","/logout*","/password/*","/email/*","/*/*/archive/*","/*/*/releases/download/*","/*/*/raw/*","/*/*/wiki/_raw/*","/_image*","/avatars/*","/*.git/*","/*/*.git/*","/api/*","/metrics*"]}},
  {"not":{"selector_matches":"[rel~=nofollow], [download], [data-no-prefetch]"}}
]},"eagerness":"moderate"}]}"#;

static CSS_URL: OnceLock<&'static str> = OnceLock::new();

/// The stylesheet URL, for responses rendered without app state (errors).
pub fn css_url() -> &'static str {
    CSS_URL.get().copied().unwrap_or("/static/style.css")
}

/// A file in the binary, compressed once at startup (it never changes
/// while keel runs) instead of on every request.
struct Asset {
    content_type: &'static str,
    raw: Bytes,
    br: Bytes,
    gzip: Bytes,
}

impl Asset {
    fn new(content_type: &'static str, text: &str) -> Self {
        let raw = Bytes::from(text.to_owned());
        let mut br = Vec::new();
        let params = brotli::enc::BrotliEncoderParams { quality: 11, ..Default::default() };
        brotli::BrotliCompress(&mut raw.as_ref(), &mut br, &params).expect("compressing in memory");
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        gz.write_all(&raw).expect("compressing in memory");
        Self { content_type, gzip: Bytes::from(gz.finish().expect("compressing in memory")), br: Bytes::from(br), raw }
    }
}

pub struct Assets {
    files: HashMap<&'static str, Asset>,
    pub css_url: &'static str,
    pub js_url: &'static str,
    pub icon_url: &'static str,
    pub rules_url: &'static str,
}

fn versioned(name: &str, content: &str) -> &'static str {
    let hash = hex::encode(&Sha256::digest(content.as_bytes())[..6]);
    // Built once at startup; leaking gives templates a plain `&'static str`.
    Box::leak(format!("/static/{name}?v={hash}").into_boxed_str())
}

impl Assets {
    pub fn new() -> Self {
        let css = format!("{STYLE}\n/* Syntax highlighting */\n{}", crate::render::highlight_css());
        let css_url = versioned("style.css", &css);
        let _ = CSS_URL.set(css_url);
        let files = HashMap::from([
            ("style.css", Asset::new("text/css; charset=utf-8", &css)),
            ("app.js", Asset::new("text/javascript; charset=utf-8", SCRIPT)),
            ("favicon.svg", Asset::new("image/svg+xml", FAVICON)),
            ("speculation-rules.json", Asset::new("application/speculationrules+json", SPECULATION_RULES)),
        ]);
        Self {
            css_url,
            js_url: versioned("app.js", SCRIPT),
            icon_url: versioned("favicon.svg", FAVICON),
            rules_url: versioned("speculation-rules.json", SPECULATION_RULES),
            files,
        }
    }
}

pub async fn serve(State(state): State<AppState>, Path(file): Path<String>, headers: HeaderMap) -> Result<Response, AppError> {
    let asset = state.assets.files.get(file.as_str()).ok_or(AppError::NotFound)?;
    let accepts = headers.get(header::ACCEPT_ENCODING).and_then(|v| v.to_str().ok()).unwrap_or_default();
    let (body, encoding) = if accepts.contains("br") {
        (asset.br.clone(), Some("br"))
    } else if accepts.contains("gzip") {
        (asset.gzip.clone(), Some("gzip"))
    } else {
        (asset.raw.clone(), None)
    };
    let mut res = (
        [
            (header::CONTENT_TYPE, asset.content_type),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
            (header::VARY, "accept-encoding"),
        ],
        body,
    )
        .into_response();
    if let Some(e) = encoding {
        res.headers_mut().insert(header::CONTENT_ENCODING, HeaderValue::from_static(e));
    }
    Ok(res)
}

/// `/favicon.ico`, which browsers ask for when a page names no icon (an
/// image, a feed): the SVG icon.
pub async fn favicon(State(state): State<AppState>) -> Redirect {
    Redirect::permanent(state.assets.icon_url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compressed_once() {
        let a = Asset::new("text/css", &"body { color: red }\n".repeat(200));
        assert!(a.br.len() < a.raw.len() / 10 && a.gzip.len() < a.raw.len() / 10);
        let mut out = Vec::new();
        brotli::BrotliDecompress(&mut a.br.as_ref(), &mut out).unwrap();
        assert_eq!(out, a.raw);
        let mut gz = flate2::read::GzDecoder::new(a.gzip.as_ref());
        let mut text = String::new();
        std::io::Read::read_to_string(&mut gz, &mut text).unwrap();
        assert_eq!(text.as_bytes(), a.raw);
    }
}
