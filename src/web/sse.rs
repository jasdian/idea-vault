//! SSE plumbing shared by chat (R9) and swarm (R7) (docs/09-web-ui.md, docs/adr/0004).
//!
//! Anything AI-generated streams over `text/event-stream` rather than a blocking request. The
//! stream emits `token` events per chunk and a terminal `done` event (D11).

/// Drive an AI chat turn as an SSE token stream.
///
/// TODO(chat): see docs/05-ai-integration.md D11 (shared by chat R9 and swarm R7) — acquire the
/// process-wide AI semaphore, call `ai::ollama::chat_stream`, and re-emit each NDJSON chunk as a
/// named `token` SSE event, closing with a `done` event on completion or client disconnect.
pub async fn chat_sse_stub() -> Result<(), crate::web::WebError> {
    Err(crate::web::WebError::NotImplemented("web::sse::chat_sse"))
}
