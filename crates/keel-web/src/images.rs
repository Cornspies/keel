//! Images from other servers, in Markdown (issues, comments, READMEs,
//! releases, wikis), are loaded through keel at `/_image`, like GitHub's
//! camo: viewers' browsers only talk to keel, so whoever posts an image
//! doesn't learn who looked at it (their address, browser, when). The
//! Content Security Policy only allows images from keel itself.
//!
//! Proxy URLs are signed, so keel only fetches images its own pages show.
//! It fetches them like webhooks (never from private or blocked addresses,
//! with a few checked redirects), only passes on images, up to 10 MiB, and
//! serves them sandboxed (an SVG opened directly can't run scripts).

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde::Deserialize;

use crate::AppState;

const MAX_IMAGE_BYTES: u64 = 10 << 20;
/// Images fetched at once; more get `503` rather than waiting.
pub(crate) const CONCURRENT_IMAGES: usize = 32;
const IMAGE_TYPES: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "image/avif",
    "image/svg+xml",
    "image/bmp",
    "image/x-icon",
    "image/vnd.microsoft.icon",
];
const IMAGE_CSP: &str = "sandbox; default-src 'none'; style-src 'unsafe-inline'";

static KEY: OnceLock<String> = OnceLock::new();
/// keel's own address (`--base-url`), whose images aren't proxied.
static OWN: OnceLock<reqwest::Url> = OnceLock::new();

/// Sets the key proxy URLs are signed with; the first call wins. Without
/// one (in tests), a random key is used.
pub(crate) fn set_key(key: String) {
    let _ = KEY.set(key);
}

/// Sets keel's own address; the first call wins.
pub(crate) fn set_base_url(base_url: &str) {
    if let Ok(url) = reqwest::Url::parse(base_url) {
        let _ = OWN.set(url);
    }
}

/// Whether `url` is on keel's own origin: its images are served to the
/// viewer directly (the CSP allows them), with the viewer's access to
/// private repositories, rather than fetched by keel from itself.
fn is_own(url: &reqwest::Url, own: Option<&reqwest::Url>) -> bool {
    own.is_some_and(|o| o.origin() == url.origin())
}

fn key() -> &'static str {
    KEY.get_or_init(|| {
        let mut bytes = [0u8; 32];
        let _ = getrandom::fill(&mut bytes);
        hex::encode(bytes)
    })
}

fn mac(url: &str) -> String {
    let full = keel_core::sign(key(), url.as_bytes());
    full.trim_start_matches("sha256=")[..32].to_owned()
}

/// The HTTP client for images: guarded like webhooks, following a few
/// checked redirects (image hosts and badge services use them).
pub(crate) fn client(outbound: &keel_core::Outbound) -> reqwest::Client {
    keel_hooks::safe_client_builder(outbound)
        .redirect(keel_hooks::checked_redirects(outbound, 3))
        .timeout(Duration::from_secs(15))
        .build()
        .expect("valid HTTP client configuration")
}

/// What an `<img src>` becomes: images on other servers go through the
/// proxy; keel's own (relative, or absolute on its origin) ones stay.
/// Browsers ignore tabs and line breaks in URLs and read `\` as `/`, so
/// `/\host/x.png` would be another server too.
pub(crate) fn img_src(src: &str) -> String {
    rewrite_src(src, OWN.get())
}

fn rewrite_src(src: &str, own: Option<&reqwest::Url>) -> String {
    let normalized: String = src
        .trim_start_matches(|c: char| c <= ' ')
        .chars()
        .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
        .map(|c| if c == '\\' { '/' } else { c })
        .collect();
    let lower = normalized.to_ascii_lowercase();
    let absolute = if lower.starts_with("http://") || lower.starts_with("https://") {
        normalized
    } else if normalized.starts_with("//") {
        format!("https:{normalized}")
    } else {
        return src.to_owned();
    };
    if let Ok(url) = reqwest::Url::parse(&absolute)
        && is_own(&url, own)
    {
        return url.into();
    }
    let encoded: String = form_urlencoded::byte_serialize(absolute.as_bytes()).collect();
    format!("/_image?url={encoded}&sig={}", mac(&absolute))
}

#[derive(Deserialize)]
pub struct ImageQuery {
    #[serde(default)]
    url: String,
    #[serde(default)]
    sig: String,
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, [(header::CACHE_CONTROL, "public, max-age=300")], "Image not available\n").into_response()
}

/// `GET /_image?url=…&sig=…`.
pub async fn image(State(state): State<AppState>, Query(q): Query<ImageQuery>) -> Response {
    let want = mac(&q.url);
    let signed = q.sig.len() == want.len() && q.sig.bytes().zip(want.bytes()).fold(0, |a, (x, y)| a | (x ^ y)) == 0;
    if !signed {
        return not_found();
    }
    let Ok(url) = reqwest::Url::parse(&q.url) else { return not_found() };
    // Never keel fetching itself (e.g. its own `/_image`, over and over).
    if is_own(&url, OWN.get()) || keel_hooks::check_url(&url, &state.core.config.outbound()).is_err() {
        return not_found();
    }
    let Ok(permit) = Arc::clone(&state.image_slots).try_acquire_owned() else {
        return (StatusCode::SERVICE_UNAVAILABLE, [(header::RETRY_AFTER, "5")], "Busy\n").into_response();
    };
    let res = match state.image_client.get(url).header(header::ACCEPT, "image/*").send().await {
        Ok(res) if res.status().is_success() => res,
        Ok(res) => {
            tracing::debug!("image {}: HTTP {}", q.url, res.status());
            return not_found();
        }
        Err(e) => {
            tracing::debug!("image {}: {e}", q.url);
            return not_found();
        }
    };
    let content_type = res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .map(|v| v.trim().to_ascii_lowercase())
        .unwrap_or_default();
    if !IMAGE_TYPES.contains(&content_type.as_str()) || res.content_length().is_some_and(|n| n > MAX_IMAGE_BYTES) {
        return not_found();
    }
    let cache = res
        .headers()
        .get(header::CACHE_CONTROL)
        .filter(|v| v.to_str().is_ok_and(|s| !s.contains("private")))
        .cloned()
        .unwrap_or(HeaderValue::from_static("public, max-age=3600"));
    // Streamed, and cut off at the limit even if the length wasn't given.
    let mut seen = 0u64;
    let body = res.bytes_stream().map(move |chunk| {
        let _held = &permit;
        let chunk = chunk.map_err(std::io::Error::other)?;
        seen += chunk.len() as u64;
        if seen > MAX_IMAGE_BYTES {
            return Err(std::io::Error::other("image too large"));
        }
        Ok(chunk)
    });
    let mut out = Body::from_stream(body).into_response();
    let h = out.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_str(&content_type).unwrap_or(HeaderValue::from_static("image/png")));
    h.insert(header::CACHE_CONTROL, cache);
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(IMAGE_CSP));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_other_servers_images() {
        assert_eq!(img_src("/ada/engine/raw/main/logo.png"), "/ada/engine/raw/main/logo.png");
        assert_eq!(img_src("logo.png"), "logo.png");
        let proxied = img_src("https://img.shields.io/badge/x-y-green.svg");
        assert!(proxied.starts_with("/_image?url=https%3A%2F%2Fimg.shields.io%2Fbadge%2Fx-y-green.svg&sig="));
        // Ways of writing another server that browsers accept.
        for sneaky in ["//evil.example/x.png", "/\\evil.example/x.png", " HTTP://evil.example/x.png", "/\t/evil.example/x.png"] {
            assert!(img_src(sneaky).starts_with("/_image?url="), "{sneaky:?}");
        }
        assert!(img_src("//evil.example/x.png").contains("url=https%3A%2F%2Fevil.example"));
        // Signatures are for the exact URL.
        let sig = |s: &str| s.rsplit("sig=").next().unwrap().to_owned();
        assert_eq!(sig(&proxied), mac("https://img.shields.io/badge/x-y-green.svg"));
        assert_ne!(sig(&proxied), mac("https://img.shields.io/badge/x-y-red.svg"));
    }

    #[test]
    fn leaves_keels_own_images() {
        let own = reqwest::Url::parse("https://git.example.com").unwrap();
        let own = Some(&own);
        assert_eq!(rewrite_src("https://git.example.com/ada/engine/raw/main/x.png", own), "https://git.example.com/ada/engine/raw/main/x.png");
        assert_eq!(rewrite_src("HTTPS://GIT.example.com:443/x.png", own), "https://git.example.com/x.png");
        assert_eq!(rewrite_src("//git.example.com/x.png", own), "https://git.example.com/x.png");
        // Another scheme, port or host is another origin.
        for other in ["http://git.example.com/x.png", "https://git.example.com:8443/x.png", "https://git.example.com.evil.example/x.png"] {
            assert!(rewrite_src(other, own).starts_with("/_image?url="), "{other}");
        }
    }
}
