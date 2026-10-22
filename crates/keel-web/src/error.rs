use askama::Template;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};

#[derive(Debug)]
pub enum AppError {
    NotFound,
    Forbidden,
    BadRequest(String),
    Internal(anyhow::Error),
}

impl From<keel_core::Error> for AppError {
    fn from(e: keel_core::Error) -> Self {
        match e {
            keel_core::Error::NotFound => Self::NotFound,
            keel_core::Error::Invalid(msg) => Self::BadRequest(msg),
            keel_core::Error::Internal(e) => Self::Internal(e),
        }
    }
}

macro_rules! internal_from {
    ($($t:ty),*) => {$(
        impl From<$t> for AppError {
            fn from(e: $t) -> Self {
                Self::Internal(e.into())
            }
        }
    )*};
}
internal_from!(anyhow::Error, keel_git::Error, askama::Error, tokio::task::JoinError, std::io::Error);

#[derive(Template)]
#[template(path = "error.html")]
struct ErrorPage<'a> {
    css_url: &'static str,
    status: u16,
    title: &'a str,
    message: &'a str,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, title, message) = match &self {
            Self::NotFound => (
                StatusCode::NOT_FOUND,
                "Not found",
                "This page doesn’t exist, or you don’t have access to it.",
            ),
            Self::Forbidden => (
                StatusCode::FORBIDDEN,
                "Forbidden",
                "You don’t have permission to do that.",
            ),
            Self::BadRequest(msg) => (StatusCode::BAD_REQUEST, "Bad request", msg.as_str()),
            Self::Internal(e) => {
                tracing::error!("internal error: {e:#}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Something went wrong",
                    "An internal error occurred. It has been logged.",
                )
            }
        };
        let page = ErrorPage {
            css_url: crate::assets::css_url(),
            status: status.as_u16(),
            title,
            message,
        };
        match page.render() {
            Ok(html) => (status, Html(html)).into_response(),
            Err(_) => (status, title.to_owned()).into_response(),
        }
    }
}

pub type AppResult<T = Response> = Result<T, AppError>;

/// Renders a template into an HTML response.
pub fn html(t: impl Template) -> AppResult {
    Ok(Html(t.render()?).into_response())
}
