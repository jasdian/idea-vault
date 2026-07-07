//! Adapts Ollama's streaming NDJSON `/api/chat` response into the crate's token stream
//! (docs/05-ai-integration.md D11): one `Ok(content)` item per NDJSON chunk until `done: true`.
//! The axum-facing SSE eventing lives in `web::sse` (this module stays free of HTTP-framework
//! types per D4 — `ai` depends on `domain` only).

use std::time::Duration;

use futures::stream::BoxStream;
use futures::StreamExt;
use serde::Deserialize;

use crate::ai::ollama::TokenStream;
use crate::ai::AiError;

/// Upper bound on one buffered NDJSON line: a single chat chunk is tiny, so anything past this
/// is a broken peer, not a token.
const MAX_LINE_BYTES: usize = 1024 * 1024;

/// One NDJSON line of Ollama's streaming `/api/chat` response — only the fields we need.
#[derive(Debug, Deserialize)]
struct ChatChunk {
    #[serde(default)]
    message: Option<ChatChunkMessage>,
    #[serde(default)]
    done: bool,
}

#[derive(Debug, Deserialize)]
struct ChatChunkMessage {
    #[serde(default)]
    content: String,
}

/// Decode a raw NDJSON byte stream into a [`TokenStream`].
///
/// Every await on the body is bounded by `token_timeout` (D20 hard timeout); EOF before
/// `done: true` is a protocol error so a partial reply can never be mistaken for a complete
/// one; error items are terminal; buffered lines are capped at [`MAX_LINE_BYTES`].
pub(crate) fn ndjson_to_tokens(
    body: BoxStream<'static, Result<bytes::Bytes, reqwest::Error>>,
    token_timeout: Duration,
) -> TokenStream {
    struct StreamState {
        body: BoxStream<'static, Result<bytes::Bytes, reqwest::Error>>,
        buf: Vec<u8>,
        token_timeout: Duration,
        finished: bool,
    }

    let state = StreamState {
        body,
        buf: Vec::new(),
        token_timeout,
        finished: false,
    };

    futures::stream::unfold(state, |mut st| async move {
        if st.finished {
            return None;
        }
        loop {
            // Drain complete NDJSON lines already buffered.
            while let Some(pos) = st.buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = st.buf.drain(..=pos).collect();
                let line = &line[..line.len() - 1];
                let line = line.strip_suffix(b"\r").unwrap_or(line);
                if line.is_empty() {
                    continue;
                }
                let chunk: ChatChunk = match serde_json::from_slice(line) {
                    Ok(chunk) => chunk,
                    Err(e) => {
                        st.finished = true;
                        return Some((
                            Err(AiError::Protocol(format!("bad NDJSON chat line: {e}"))),
                            st,
                        ));
                    }
                };
                if chunk.done {
                    st.finished = true;
                }
                let content = chunk.message.map(|m| m.content).unwrap_or_default();
                if !content.is_empty() {
                    return Some((Ok(content), st));
                }
                if st.finished {
                    return None;
                }
            }

            // Need more bytes: every gap is bounded by the hard timeout (D20).
            match tokio::time::timeout(st.token_timeout, st.body.next()).await {
                Err(_) => {
                    st.finished = true;
                    return Some((Err(AiError::Timeout), st));
                }
                Ok(None) => {
                    st.finished = true;
                    return Some((
                        Err(AiError::Protocol(
                            "stream ended before done: true".to_string(),
                        )),
                        st,
                    ));
                }
                Ok(Some(Err(e))) => {
                    st.finished = true;
                    return Some((Err(AiError::Http(e)), st));
                }
                Ok(Some(Ok(bytes))) => {
                    st.buf.extend_from_slice(&bytes);
                    // The hard timeout bounds *time*; this bounds *bytes* — a peer that
                    // never sends a newline must not grow the buffer without limit.
                    if st.buf.len() > MAX_LINE_BYTES {
                        st.finished = true;
                        return Some((
                            Err(AiError::Protocol(format!(
                                "NDJSON line exceeded {MAX_LINE_BYTES} bytes"
                            ))),
                            st,
                        ));
                    }
                }
            }
        }
    })
    .boxed()
}
