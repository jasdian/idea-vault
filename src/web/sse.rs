//! SSE plumbing shared by chat (R9) and swarm (R7) (docs/09-web-ui.md, docs/adr/0004).
//!
//! Anything AI-generated streams over `text/event-stream` rather than a blocking request. The
//! stream emits `token` events per chunk, a terminal `done` event on success, and an `error`
//! event when the model call aborts (timeout/protocol/transport — D20 degrade, never hang).

use axum::response::sse::{Event, KeepAlive, Sse};
use futures::Stream;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::ai::ollama::TokenStream;

/// Escape a raw model token for direct insertion into HTML — streamed tokens bypass the
/// markdown/sanitize pipeline (they are plain text fragments), so they must never carry markup.
pub fn escape_token(token: &str) -> String {
    token
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// The channel type the pump writes SSE events into.
pub type EventSender = mpsc::Sender<Result<Event, std::convert::Infallible>>;

/// Build an SSE response from a receiver; the paired sender feeds it from a spawned task.
pub fn sse_response(
    rx: mpsc::Receiver<Result<Event, std::convert::Infallible>>,
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default())
}

/// How long a `token` send may wait on a non-reading client before the stream is treated as
/// disconnected. Without this bound, a stalled reader (backgrounded tab, dead proxy) would
/// block the pump forever — and with it the AI-semaphore permit its caller holds, exhausting
/// the process-wide concurrency bound for every other idea (ADR-0006).
const CLIENT_SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Pump a model token stream into `tx` as `token` events, returning the accumulated full text
/// on completion (D11: the caller persists it only then).
///
/// Returns `Ok(None)` if the client disconnected or stopped reading (send failed or timed out)
/// — the caller must abort without persisting; dropping the token stream aborts the underlying
/// Ollama request. Returns `Err` on a model failure after emitting an `error` event (nothing
/// partial is ever persisted).
pub async fn pump_tokens(
    mut tokens: TokenStream,
    tx: &EventSender,
) -> Result<Option<String>, crate::ai::AiError> {
    use futures::StreamExt;

    let mut full = String::new();
    while let Some(item) = tokens.next().await {
        match item {
            Ok(token) => {
                full.push_str(&token);
                let event = Event::default().event("token").data(escape_token(&token));
                match tokio::time::timeout(CLIENT_SEND_TIMEOUT, tx.send(Ok(event))).await {
                    Ok(Ok(())) => {}
                    // Disconnected or not reading: abort the model call, persist nothing (D11).
                    Ok(Err(_)) | Err(_) => return Ok(None),
                }
            }
            Err(e) => {
                let event = Event::default()
                    .event("error")
                    .data("the model call failed; nothing was saved");
                let _ = tx.send(Ok(event)).await;
                return Err(e);
            }
        }
    }
    Ok(Some(full))
}
