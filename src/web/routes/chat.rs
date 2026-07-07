//! Chat route (docs/09-web-ui.md D17 R9): the discussion turn that streams AI tokens over SSE
//! (docs/adr/0004, D11). Chat shares `web::sse` and the process-wide AI semaphore with swarm.

use axum::extract::{Path, State};

use crate::app::AppState;
use crate::web::WebError;

/// R9 — `POST /idea/{slug}/chat` — one discussion turn, streamed token-by-token over SSE.
pub async fn chat(
    State(_state): State<AppState>,
    Path(_slug): Path<String>,
) -> Result<axum::response::Response, WebError> {
    // TODO(D11): see docs/05-ai-integration.md D11 & docs/adr/0004 — append the user turn to the
    // conversation, then return a `text/event-stream` driven by `web::sse` emitting
    // `token`/`done` events (acquiring the shared AI semaphore first).
    Err(WebError::NotImplemented("web::routes::chat::chat"))
}
