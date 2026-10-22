//! CSS and JS are compiled into the binary. URLs carry a content hash, so
//! browsers can cache them forever.

use std::sync::OnceLock;

use axum::extract::{Path, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};

use crate::{AppError, AppState};

const STYLE: &str = include_str!("../static/style.css");
const SCRIPT: &str = include_str!("../static/app.js");

static CSS_URL: OnceLock<&'static str> = OnceLock::new();

/// The stylesheet URL, for responses rendered without app state (errors).
pub fn css_url() -> &'static str {
    CSS_URL.get().copied().unwrap_or("/static/style.css")
}

pub struct Assets {
    css: String,
    pub css_url: &'static str,
    pub js_url: &'static str,
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
        Self {
            css_url,
            js_url: versioned("app.js", SCRIPT),
            css,
        }
    }
}

pub async fn serve(State(state): State<AppState>, Path(file): Path<String>) -> Result<Response, AppError> {
    let (content_type, body) = match file.as_str() {
        "style.css" => ("text/css; charset=utf-8", state.assets.css.clone()),
        "app.js" => ("text/javascript; charset=utf-8", SCRIPT.to_owned()),
        _ => return Err(AppError::NotFound),
    };
    Ok((
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        body,
    )
        .into_response())
}
