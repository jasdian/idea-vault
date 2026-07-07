//! `web` — the HTTP surface: axum router, handlers, Askama templates, SSE plumbing.
//!
//! Top of the module graph (docs/02-module-reference.md D4): `web` may depend on everything below
//! it, but nothing depends on `web`. Handlers orchestrate the lower modules; domain rules live
//! there, not here. Response shapes are full pages, HTMX partials (`templates/_*.html`), or SSE
//! streams (docs/09-web-ui.md D16/D17).

pub mod routes;
pub mod sse;
pub mod templates;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

/// Errors surfaced by the HTTP layer. Inner `NotImplemented` variants (through any wrapper) map to
/// `501`; every other error maps to `500` and is logged (docs/09-web-ui.md D16, D24).
#[derive(Debug, thiserror::Error)]
pub enum WebError {
    #[error("not implemented: {0}")]
    NotImplemented(&'static str),

    #[error("vault error: {0}")]
    Vault(#[from] crate::vault::VaultError),

    #[error("index error: {0}")]
    Index(#[from] crate::index::IndexError),

    #[error("ai error: {0}")]
    Ai(#[from] crate::ai::AiError),

    #[error("concept error: {0}")]
    Concept(#[from] crate::concepts::ConceptError),

    /// Invalid client input (empty title, malformed form) → 400 with the reason.
    #[error("bad request: {0}")]
    BadRequest(String),

    /// The addressed idea does not exist → 404.
    #[error("not found: {0}")]
    NotFound(String),

    #[error("internal error: {0}")]
    Internal(String),
}

impl WebError {
    /// If this error is (or wraps) a scaffold `NotImplemented`, return what is not implemented.
    fn not_implemented_what(&self) -> Option<&'static str> {
        match self {
            WebError::NotImplemented(what) => Some(what),
            WebError::Vault(crate::vault::VaultError::NotImplemented(what)) => Some(what),
            WebError::Index(crate::index::IndexError::NotImplemented(what)) => Some(what),
            WebError::Ai(crate::ai::AiError::NotImplemented(what)) => Some(what),
            WebError::Concept(crate::concepts::ConceptError::NotImplemented(what)) => Some(what),
            _ => None,
        }
    }
}

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        if let Some(what) = self.not_implemented_what() {
            return (
                StatusCode::NOT_IMPLEMENTED,
                format!("not implemented: {what} (scaffold)"),
            )
                .into_response();
        }
        match self {
            WebError::BadRequest(reason) => {
                (StatusCode::BAD_REQUEST, format!("bad request: {reason}")).into_response()
            }
            WebError::NotFound(what)
            | WebError::Vault(crate::vault::VaultError::IdeaNotFound(what)) => {
                (StatusCode::NOT_FOUND, format!("not found: {what}")).into_response()
            }
            // A malformed slug in a URL is answered like a missing idea — 404, without
            // distinguishing "malformed" from "absent" for a probing client, and without
            // polluting the error log as if it were an internal fault.
            WebError::Vault(crate::vault::VaultError::InvalidSlug(_)) => {
                (StatusCode::NOT_FOUND, "not found".to_string()).into_response()
            }
            other => {
                tracing::error!(error = %other, "web handler error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error".to_string(),
                )
                    .into_response()
            }
        }
    }
}
