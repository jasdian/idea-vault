//! HTTP client for the local Ollama server (`GET /api/tags`, `POST /api/chat`) — the only place
//! in the crate that is allowed to speak Ollama's wire protocol
//! (docs/05-ai-integration.md "Ollama client contract").
//!
//! The base URL is always injected by the caller (ultimately `config::Config::ollama_url`,
//! itself env-driven) — this module never hardcodes `localhost:11434` (docs/12-deployment.md).

use std::time::Duration;

use serde::Deserialize;

use crate::ai::AiError;

/// Overall wall-clock budget for the boot/health probe (D25: boot must not hang on Ollama).
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// Connect timeout applied to every request made by an [`OllamaClient`].
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);

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

/// Client for the local Ollama server. Cheap to clone (wraps a pooled `reqwest::Client`).
#[derive(Clone)]
pub struct OllamaClient {
    http: reqwest::Client,
    base_url: String,
    model: String,
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
        })
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

    /// Stream a chat completion from Ollama (`POST /api/chat`, `stream: true`).
    ///
    /// TODO(chat): see docs/05-ai-integration.md D11 — POST /api/chat with stream:true, decode
    /// the NDJSON `{message: {content: "..."}}` chunks token-by-token until `{done: true}`,
    /// acquiring the process-wide concurrency semaphore (ADR-0006) before issuing the request and
    /// releasing it on completion or client disconnect.
    pub async fn chat_stream(&self, _messages: Vec<ChatMessage>) -> Result<(), AiError> {
        Err(AiError::NotImplemented("ai::ollama::chat_stream"))
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
