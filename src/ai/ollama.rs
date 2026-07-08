//! HTTP client for the local Ollama server (`GET /api/tags`, `POST /api/chat`,
//! `POST /api/show`) — the only place in the crate that is allowed to speak Ollama's wire
//! protocol (docs/05-ai-integration.md "Ollama client contract").
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

/// Wall-clock budget for the `/api/show` model-metadata query (dynamic context budget). A slow
/// or absent server yields `None` — callers fall back rather than block on metadata.
const SHOW_TIMEOUT: Duration = Duration::from_secs(5);

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

/// Per-call knobs forwarded as Ollama's `options` object. Fields that are `None` are omitted
/// from the wire body entirely (the server default applies). `num_ctx` matters: without it
/// Ollama silently truncates the prompt at its own default window (~4k tokens) no matter what
/// the caller assembled — the router always sends the live budget window.
#[derive(Debug, Clone, Copy, Default)]
pub struct ChatOptions {
    /// Sampling temperature (`options.temperature`).
    pub temperature: Option<f32>,
    /// Context window in tokens (`options.num_ctx`).
    pub num_ctx: Option<usize>,
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

    /// Complete a chat non-interactively: consume [`chat_stream`](Self::chat_stream) to the end
    /// and return the concatenated text. Used by extraction/skills where no browser is watching
    /// tokens; the same hard-timeout and persist-boundary guarantees apply — any stream error
    /// aborts the whole call, a partial response is never returned as if complete.
    pub async fn chat(&self, messages: Vec<ChatMessage>) -> Result<String, AiError> {
        self.chat_with(ChatOptions::default(), messages).await
    }

    /// Non-streaming completion with per-call [`ChatOptions`] (runtime settings).
    pub async fn chat_with(
        &self,
        options: ChatOptions,
        messages: Vec<ChatMessage>,
    ) -> Result<String, AiError> {
        let mut stream = self.chat_stream_with(options, messages).await?;
        let mut out = String::new();
        while let Some(item) = stream.next().await {
            out.push_str(&item?);
        }
        Ok(out)
    }

    /// Query the configured model's native context window in tokens (`POST /api/show`).
    ///
    /// Best-effort metadata: any transport error, non-2xx status, timeout, or a body without a
    /// recognizable `context_length` key yields `None` — the caller falls back to its default
    /// window and may retry later (failures are deliberately not an error type).
    pub async fn show_context_length(&self) -> Option<usize> {
        let url = format!("{}/api/show", self.base_url);
        let response = self
            .http
            .post(&url)
            .json(&serde_json::json!({ "model": self.model }))
            .timeout(SHOW_TIMEOUT)
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let body = response.json::<serde_json::Value>().await.ok()?;
        context_length_from_show(&body)
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
        self.chat_stream_with(ChatOptions::default(), messages)
            .await
    }

    /// Stream a chat completion with per-call [`ChatOptions`] (runtime settings): set fields go
    /// in Ollama's `options` object; an all-`None` options sends no `options` (server defaults).
    pub async fn chat_stream_with(
        &self,
        options: ChatOptions,
        messages: Vec<ChatMessage>,
    ) -> Result<TokenStream, AiError> {
        let url = format!("{}/api/chat", self.base_url);
        let body = build_chat_body(&self.model, options, &messages);

        let response =
            tokio::time::timeout(self.token_timeout, self.http.post(&url).json(&body).send())
                .await
                .map_err(|_| AiError::Timeout)??
                .error_for_status()?;

        Ok(crate::ai::stream::ndjson_to_tokens(
            response.bytes_stream().boxed(),
            self.token_timeout,
        ))
    }
}

/// Build the `POST /api/chat` request body. Pure and socket-free so the wire shape (including
/// `options.num_ctx` presence/absence) is unit-testable directly.
pub(crate) fn build_chat_body(
    model: &str,
    options: ChatOptions,
    messages: &[ChatMessage],
) -> serde_json::Value {
    let mut body = serde_json::json!({
        "model": model,
        "messages": messages,
        "stream": true,
    });
    let mut opts = serde_json::Map::new();
    if let Some(t) = options.temperature {
        opts.insert("temperature".to_string(), serde_json::json!(t));
    }
    if let Some(n) = options.num_ctx {
        opts.insert("num_ctx".to_string(), serde_json::json!(n));
    }
    if !opts.is_empty() {
        body["options"] = serde_json::Value::Object(opts);
    }
    body
}

/// Extract the native context window from an `/api/show` response body: scan `model_info` for
/// the architecture-prefixed `<arch>.context_length` key (e.g. `qwen3.context_length`, or a bare
/// `context_length`). Pure and socket-free so it can be unit-tested against fixtures.
pub(crate) fn context_length_from_show(body: &serde_json::Value) -> Option<usize> {
    body.get("model_info")?
        .as_object()?
        .iter()
        .find_map(|(key, value)| {
            if key == "context_length" || key.ends_with(".context_length") {
                value.as_u64().map(|v| v as usize)
            } else {
                None
            }
        })
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

    #[test]
    fn context_length_from_arch_prefixed_key() {
        let body = serde_json::json!({
            "model_info": {
                "general.architecture": "qwen3",
                "qwen3.context_length": 40960,
                "qwen3.embedding_length": 2560,
            }
        });
        assert_eq!(context_length_from_show(&body), Some(40_960));
    }

    #[test]
    fn context_length_from_bare_key() {
        let body = serde_json::json!({ "model_info": { "context_length": 8192 } });
        assert_eq!(context_length_from_show(&body), Some(8_192));
    }

    #[test]
    fn context_length_missing_or_malformed_is_none() {
        assert_eq!(context_length_from_show(&serde_json::json!({})), None);
        assert_eq!(
            context_length_from_show(&serde_json::json!({ "model_info": {} })),
            None
        );
        // A non-numeric value must not be misread; a suffix-only lookalike must not match.
        assert_eq!(
            context_length_from_show(
                &serde_json::json!({ "model_info": { "qwen3.context_length": "big" } })
            ),
            None
        );
        assert_eq!(
            context_length_from_show(
                &serde_json::json!({ "model_info": { "noncontext_length": 4096 } })
            ),
            None
        );
    }

    #[test]
    fn chat_body_carries_options_only_when_set() {
        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: "hi".to_string(),
        }];

        // Both set → both land in `options`.
        let body = build_chat_body(
            "llama3.2",
            ChatOptions {
                temperature: Some(0.7),
                num_ctx: Some(8192),
            },
            &messages,
        );
        assert_eq!(body["options"]["num_ctx"], serde_json::json!(8192));
        assert!((body["options"]["temperature"].as_f64().unwrap() - 0.7).abs() < 1e-6);

        // num_ctx only → temperature absent.
        let body = build_chat_body(
            "llama3.2",
            ChatOptions {
                temperature: None,
                num_ctx: Some(4096),
            },
            &messages,
        );
        assert_eq!(body["options"]["num_ctx"], serde_json::json!(4096));
        assert!(body["options"].get("temperature").is_none());

        // Nothing set → no `options` object at all (server defaults).
        let body = build_chat_body("llama3.2", ChatOptions::default(), &messages);
        assert!(body.get("options").is_none());
    }
}
