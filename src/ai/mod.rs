//! The `ai` boundary: the *only* module allowed to speak to Ollama (docs/05-ai-integration.md).
//!
//! `ai` depends solely on `domain` — never on `vault`/`index` (docs/02-module-reference.md D4).
//! Callers (web routes, `concepts::swarm`) assemble prompts and hand them in; `ai` does not read
//! the vault itself.
//!
//! Submodules:
//! - [`ollama`] — HTTP client + health probe against `http://localhost:11434` (never hardcoded
//!   outside `config.rs`; the base URL is always passed in).
//! - [`stream`] — adapts Ollama's NDJSON token stream into SSE events (D11).
//! - [`budget`] — assembles a prompt within the local model's context limit (D21).

pub mod budget;
pub mod ollama;
pub mod stream;

pub use ollama::{AiHealth, OllamaClient};

/// Errors produced at the `ai` boundary (docs/05-ai-integration.md D24 — AI errors degrade,
/// they do not crash the request).
#[derive(Debug, thiserror::Error)]
pub enum AiError {
    #[error("not implemented: {0}")]
    NotImplemented(&'static str),

    #[error("http error talking to Ollama: {0}")]
    Http(#[from] reqwest::Error),
}
