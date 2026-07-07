//! Adapts Ollama's streaming NDJSON `/api/chat` response into SSE events for the browser
//! (docs/05-ai-integration.md D11).
//!
//! TODO(chat): see docs/05-ai-integration.md D11 — decode each NDJSON line into a `token` SSE
//! event (`{message.content: "..."}`) and emit a final `done` SSE event once Ollama sends
//! `{done: true}`. Persist boundaries strictly: the caller appends the user turn to
//! `conversation.md` *before* this stream starts, and the assistant turn only *after* this stream
//! completes — a partial turn must never become truth. On client disconnect the caller must abort
//! the underlying Ollama request and release the concurrency semaphore (ADR-0006).

use crate::ai::AiError;

/// Convert Ollama's NDJSON token stream into a stream of SSE events.
///
/// TODO(chat): see docs/05-ai-integration.md D11.
pub async fn tokens_to_sse() -> Result<(), AiError> {
    Err(AiError::NotImplemented("ai::stream::tokens_to_sse"))
}
