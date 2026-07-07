//! HTTP client for the local Ollama server (`GET /api/tags`, `POST /api/chat`) — the only place
//! in the crate that is allowed to speak Ollama's wire protocol
//! (docs/05-ai-integration.md "Ollama client contract").
//!
//! The base URL is always injected by the caller (ultimately `config::Config::ollama_url`,
//! itself env-driven) — this module never hardcodes `localhost:11434` (docs/12-deployment.md).

use std::time::Duration;

use futures::stream::BoxStream;
use futures::StreamExt;
use serde::Deserialize;

use crate::ai::AiError;

/// Overall wall-clock budget for the boot/health probe (D25: boot must not hang on Ollama).
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// Connect timeout applied to every request made by an [`OllamaClient`].
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);

/// Default hard timeout between token chunks (D20). Local models can be slow per token but a
/// gap this long means the call is wedged — abort rather than hang the SSE stream forever.
const DEFAULT_TOKEN_TIMEOUT: Duration = Duration::from_secs(120);

/// Upper bound on one buffered NDJSON line: a single chat chunk is tiny, so anything past this
/// is a broken peer, not a token.
const MAX_LINE_BYTES: usize = 1024 * 1024;

/// A live token stream from `/api/chat`: each item is one content chunk, in order. The stream
/// ends after Ollama's `done: true`; an `Err` item (timeout/protocol/transport) is terminal.
pub type TokenStream = BoxStream<'static, Result<String, AiError>>;

/// Result of probing the local Ollama server (docs/05-ai-integration.md D20).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiHealth {
    /// Server reachable and the configured model is present.
    Available,
    /// Server reachable but the configured model has not been pulled.
    ModelMissing,
    /// Server unreachable (connection refused, DNS failure, or the probe timed out).
    Unreachable,
}

/// A single entry in Ollama's `GET /api/tags` response.
#[derive(Debug, Deserialize)]
struct TagsModel {
    name: String,
}

/// Shape of Ollama's `GET /api/tags` response body — only the fields we need.
#[derive(Debug, Deserialize)]
struct TagsResponse {
    #[serde(default)]
    models: Vec<TagsModel>,
}

/// A single chat turn in the shape Ollama's `/api/chat` endpoint expects.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

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

/// Client for the local Ollama server. Cheap to clone (wraps a pooled `reqwest::Client`).
#[derive(Clone)]
pub struct OllamaClient {
    http: reqwest::Client,
    base_url: String,
    model: String,
    token_timeout: Duration,
}

impl OllamaClient {
    /// Build a client against `base_url` (trailing slash trimmed) for `model`.
    ///
    /// Errs if the underlying `reqwest::Client` fails to build (e.g. a malformed proxy env var
    /// or TLS backend init failure) — callers surface this instead of the process unwinding.
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Result<Self, AiError> {
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()?;

        Ok(Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            model: model.into(),
            token_timeout: DEFAULT_TOKEN_TIMEOUT,
        })
    }

    /// Override the hard inactivity timeout applied to the initial response and to every
    /// token gap of [`chat_stream`](Self::chat_stream) (D20 hard timeout).
    pub fn with_token_timeout(mut self, token_timeout: Duration) -> Self {
        self.token_timeout = token_timeout;
        self
    }

    /// The configured model tag (e.g. `llama3.2`).
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Probe the local Ollama server's health (docs/05-ai-integration.md D20).
    ///
    /// Never panics, never blocks longer than [`PROBE_TIMEOUT`] (D25): any transport error,
    /// non-2xx status, or unparsable body is treated as [`AiHealth::Unreachable`].
    pub async fn probe(&self) -> AiHealth {
        let url = format!("{}/api/tags", self.base_url);

        let response = match self.http.get(&url).timeout(PROBE_TIMEOUT).send().await {
            Ok(resp) => resp,
            Err(_) => return AiHealth::Unreachable,
        };

        if !response.status().is_success() {
            return AiHealth::Unreachable;
        }

        let body = match response.json::<TagsResponse>().await {
            Ok(body) => body,
            Err(_) => return AiHealth::Unreachable,
        };

        if body
            .models
            .iter()
            .any(|m| model_matches(&m.name, &self.model))
        {
            AiHealth::Available
        } else {
            AiHealth::ModelMissing
        }
    }

    /// Stream a chat completion from Ollama (`POST /api/chat`, `stream: true`, D11).
    ///
    /// Yields one content chunk per NDJSON line until `done: true`; the stream then ends. Any
    /// error item (hard timeout on a token gap, transport failure, or a protocol violation such
    /// as EOF before `done`) is terminal — callers must treat the turn as aborted and never
    /// persist the partial text (docs/05: "a partial turn must not become truth").
    ///
    /// Concurrency note: acquiring the process-wide semaphore (ADR-0006) is the caller's job —
    /// this module has no access to `AppState`. Dropping the returned stream aborts the request.
    pub async fn chat_stream(&self, messages: Vec<ChatMessage>) -> Result<TokenStream, AiError> {
        let url = format!("{}/api/chat", self.base_url);
        let body = serde_json::json!({
            "model": self.model,
            "messages": messages,
            "stream": true,
        });

        let response =
            tokio::time::timeout(self.token_timeout, self.http.post(&url).json(&body).send())
                .await
                .map_err(|_| AiError::Timeout)??
                .error_for_status()?;

        struct StreamState {
            body: BoxStream<'static, Result<bytes::Bytes, reqwest::Error>>,
            buf: Vec<u8>,
            token_timeout: Duration,
            finished: bool,
        }

        let state = StreamState {
            body: response.bytes_stream().boxed(),
            buf: Vec::new(),
            token_timeout: self.token_timeout,
            finished: false,
        };

        Ok(futures::stream::unfold(state, |mut st| async move {
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
        .boxed())
    }
}

/// True if `available` (a model name as returned by `/api/tags`, e.g. `llama3.2:latest`) matches
/// `wanted` (the configured model tag, e.g. `llama3.2`): either an exact match, or `available`
/// carries an explicit tag suffix (`wanted:<tag>`) that Ollama appends implicitly (`:latest`).
///
/// Pure and socket-free so it can be unit-tested directly.
pub(crate) fn model_matches(available: &str, wanted: &str) -> bool {
    available == wanted || available.starts_with(&format!("{wanted}:"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match() {
        assert!(model_matches("llama3.2", "llama3.2"));
    }

    #[test]
    fn implicit_latest_tag_matches() {
        assert!(model_matches("llama3.2:latest", "llama3.2"));
    }

    #[test]
    fn explicit_tag_matches() {
        assert!(model_matches("llama3.2:8b", "llama3.2"));
    }

    #[test]
    fn different_model_does_not_match() {
        assert!(!model_matches("mistral:latest", "llama3.2"));
    }

    #[test]
    fn prefix_without_colon_does_not_match() {
        // "llama3.20" must not be mistaken for a tagged "llama3.2".
        assert!(!model_matches("llama3.20", "llama3.2"));
    }

    #[test]
    fn empty_available_does_not_match() {
        assert!(!model_matches("", "llama3.2"));
    }
}
