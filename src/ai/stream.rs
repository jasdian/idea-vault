//! Adapts Ollama's streaming NDJSON `/api/chat` response into the crate's token stream
//! (docs/05-ai-integration.md D11): one `Ok(content)` item per NDJSON chunk until `done: true`.
//! `LlmBackend::chat` consumes this to build a full reply; the browser chat route is a blocking
//! POST (no SSE). This module stays free of HTTP-framework types per D4 — `ai` depends on `domain`.

use std::time::Duration;

use futures::stream::BoxStream;
use futures::StreamExt;
use serde::Deserialize;

use crate::ai::call::{fill_slot, meta_slot, CallMeta, CallUsage, MetaSlot};
use crate::ai::ollama::TokenStream;
use crate::ai::AiError;

/// Upper bound on one buffered NDJSON line: a single chat chunk is tiny, so anything past this
/// is a broken peer, not a token.
const MAX_LINE_BYTES: usize = 1024 * 1024;

/// One NDJSON line of Ollama's streaming `/api/chat` response — only the fields we need. The
/// terminal (`done: true`) line also carries why generation stopped and the token counts, which
/// become the call's [`CallMeta`] (docs/adr/0037); the non-streaming tool-round body has the same
/// top-level shape.
#[derive(Debug, Deserialize)]
struct ChatChunk {
    #[serde(default)]
    message: Option<ChatChunkMessage>,
    #[serde(default)]
    done: bool,
    #[serde(default)]
    done_reason: Option<String>,
    #[serde(default)]
    prompt_eval_count: Option<u64>,
    #[serde(default)]
    eval_count: Option<u64>,
}

impl ChatChunk {
    /// The meta of the one request this terminal chunk closes.
    fn meta(&self) -> CallMeta {
        CallMeta {
            usage: CallUsage {
                prompt_tokens: self.prompt_eval_count,
                output_tokens: self.eval_count,
                api_calls: 1,
            },
            stop_reason: self.done_reason.clone(),
            ..CallMeta::default()
        }
    }
}

/// The [`CallMeta`] of a non-streaming `/api/chat` response body (one tool round). A body that
/// does not parse as a chunk still counts as one request.
pub(crate) fn meta_from_final(body: &serde_json::Value) -> CallMeta {
    serde_json::from_value::<ChatChunk>(body.clone())
        .map(|c| c.meta())
        .unwrap_or_else(|_| CallMeta {
            usage: CallUsage {
                api_calls: 1,
                ..CallUsage::default()
            },
            ..CallMeta::default()
        })
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
/// one; error items are terminal; buffered lines are capped at [`MAX_LINE_BYTES`]. The terminal
/// chunk's stop reason and token counts land in the stream's meta slot.
pub(crate) fn ndjson_to_tokens(
    body: BoxStream<'static, Result<bytes::Bytes, reqwest::Error>>,
    token_timeout: Duration,
) -> TokenStream {
    struct StreamState {
        body: BoxStream<'static, Result<bytes::Bytes, reqwest::Error>>,
        buf: Vec<u8>,
        token_timeout: Duration,
        finished: bool,
        meta: MetaSlot,
    }

    let meta = meta_slot();
    let state = StreamState {
        body,
        buf: Vec::new(),
        token_timeout,
        finished: false,
        meta: meta.clone(),
    };

    let tokens = futures::stream::unfold(state, |mut st| async move {
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
                    fill_slot(&st.meta, chunk.meta());
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
    .boxed();
    TokenStream::new(tokens, meta)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::call::read_slot;

    fn body(lines: &str) -> BoxStream<'static, Result<bytes::Bytes, reqwest::Error>> {
        futures::stream::iter(vec![Ok(bytes::Bytes::from(lines.to_string()))]).boxed()
    }

    #[tokio::test]
    async fn terminal_chunk_carries_done_reason_and_eval_counts() {
        let lines = concat!(
            "{\"message\":{\"content\":\"Hel\"},\"done\":false}\n",
            "{\"message\":{\"content\":\"lo\"},\"done\":false}\n",
            "{\"message\":{\"content\":\"\"},\"done\":true,\"done_reason\":\"length\",",
            "\"prompt_eval_count\":812,\"eval_count\":64}\n",
        );
        let mut stream = ndjson_to_tokens(body(lines), Duration::from_secs(5));
        let slot = stream.meta();
        assert_eq!(read_slot(&slot), None, "empty before the terminal chunk");
        let mut text = String::new();
        while let Some(item) = stream.next().await {
            text.push_str(&item.unwrap());
        }
        assert_eq!(text, "Hello");
        let meta = read_slot(&slot).expect("filled by the done chunk");
        assert_eq!(meta.stop_reason.as_deref(), Some("length"));
        assert_eq!(meta.usage.prompt_tokens, Some(812));
        assert_eq!(meta.usage.output_tokens, Some(64));
        assert_eq!(meta.usage.api_calls, 1);
        assert!(meta.output_truncated());
    }

    #[tokio::test]
    async fn a_stream_cut_before_done_leaves_no_meta() {
        let mut stream = ndjson_to_tokens(
            body("{\"message\":{\"content\":\"x\"},\"done\":false}\n"),
            Duration::from_secs(5),
        );
        while stream.next().await.is_some() {}
        assert_eq!(read_slot(&stream.meta()), None);
    }

    #[test]
    fn a_non_streaming_body_yields_its_meta() {
        let v = serde_json::json!({
            "message": {"content": "x"}, "done": true, "done_reason": "stop",
            "prompt_eval_count": 3, "eval_count": 4
        });
        let meta = meta_from_final(&v);
        assert_eq!(meta.stop_reason.as_deref(), Some("stop"));
        assert_eq!(meta.usage.prompt_tokens, Some(3));
        assert_eq!(meta.usage.api_calls, 1);
    }
}
