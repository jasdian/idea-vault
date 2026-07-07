//! The LLM backend seam (docs/adr/0009). Callers hold an [`LlmBackend`] and never care which
//! concrete client answers — the persist boundaries, the shared concurrency semaphore (ADR-0006),
//! and the SSE pump all sit *above* this enum, so they are identical for either backend.
//!
//! An enum (not a `dyn` trait) keeps this zero-cost and dependency-free: the backend set is closed
//! and small, and `async fn`-in-trait behind `dyn` would need `async-trait`'s boxing. Each method
//! matches and delegates to the concrete client, which already exposes exactly these four methods.

use crate::ai::claude_code::ClaudeCodeClient;
use crate::ai::ollama::{ChatMessage, OllamaClient, TokenStream};
use crate::ai::{AiError, AiHealth};

/// The active LLM backend, selected at boot from `IDEA_VAULT_LLM_BACKEND` (`config.rs`).
#[derive(Clone)]
pub enum LlmBackend {
    /// Local Ollama over HTTP (offline, no file access).
    Ollama(OllamaClient),
    /// The local `claude` CLI (agentic — can read the owner's vaults/artifacts).
    ClaudeCode(ClaudeCodeClient),
}

impl LlmBackend {
    /// Health probe for the degraded-AI UI (D20).
    pub async fn probe(&self) -> AiHealth {
        match self {
            LlmBackend::Ollama(c) => c.probe().await,
            LlmBackend::ClaudeCode(c) => c.probe().await,
        }
    }

    /// A human-facing model label (used in the "pull a model" degraded hint and logs).
    pub fn model(&self) -> &str {
        match self {
            LlmBackend::Ollama(c) => c.model(),
            LlmBackend::ClaudeCode(c) => c.model(),
        }
    }

    /// Non-streaming completion (extraction, skills, agents).
    pub async fn chat(&self, messages: Vec<ChatMessage>) -> Result<String, AiError> {
        match self {
            LlmBackend::Ollama(c) => c.chat(messages).await,
            LlmBackend::ClaudeCode(c) => c.chat(messages).await,
        }
    }

    /// Streaming completion (the SSE chat turn, D11). The returned stream is terminal on error and
    /// aborts its backend when dropped, so a partial reply is never persisted.
    pub async fn chat_stream(&self, messages: Vec<ChatMessage>) -> Result<TokenStream, AiError> {
        match self {
            LlmBackend::Ollama(c) => c.chat_stream(messages).await,
            LlmBackend::ClaudeCode(c) => c.chat_stream(messages).await,
        }
    }
}
