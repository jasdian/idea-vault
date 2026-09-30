//! The `ai` boundary: the *only* module allowed to speak to Ollama (docs/05-ai-integration.md).
//!
//! `ai` depends on `domain` plus the dependency-free `mcp` config registry — never on
//! `vault`/`index` (docs/02-module-reference.md D4). Callers (web routes, `concepts::swarm`)
//! assemble prompts and hand them in; `ai` does not read the vault itself.
//!
//! Submodules:
//! - [`ollama`] — HTTP client + health probe against `http://localhost:11434` (never hardcoded
//!   outside `config.rs`; the base URL is always passed in).
//! - [`claude_code`] — a second backend that shells out to the local `claude` CLI and streams its
//!   `stream-json` output (docs/adr/0009). Brings agentic file tools to the foil.
//! - [`backend`] — the [`LlmBackend`] router struct that lets callers target either backend behind
//!   one API.
//! - [`stream`] — adapts a backend's token stream into SSE events (D11).
//! - [`budget`] — assembles a prompt within the model's context limit (D21).
//! - [`contract`] — pure output-shape checks + repair for skill answers (docs/adr/0023).
//! - [`untrusted`] — the fence every tool result passes through before it reaches the Ollama
//!   tool loop as a `role: "tool"` message (ADR-0039).
//! - [`web`] — keyless web-search + page-fetch tool leaves (ADR-0017), executed by the router's
//!   bounded Ollama tool loop; claude-code uses its own WebSearch/WebFetch instead.
//! - [`sources`] — deterministic read-only tool leaves (`source_list`/`source_grep`/`source_read`)
//!   over the owner's attached reference sources (DRT: the model picks *which* source by name;
//!   code resolves the name to a canonical root via [`crate::sources`] and enforces containment).
//! - [`mcp`] — Streamable-HTTP client for the Model Context Protocol: a third, owner-configured
//!   source of tools (arbitrary MCP servers) alongside `web`'s hardcoded leaves. Wire client only
//!   — WHICH servers exist/are enabled is the top-level [`crate::mcp`] registry's business, and
//!   [`backend`] is the bridge that combines the two per turn (see its module doc for why the
//!   split keeps the graph acyclic).

pub mod backend;
pub mod budget;
pub mod claude_code;
pub mod contract;
pub mod mcp;
pub mod ollama;
pub mod sources;
pub mod stream;
pub mod untrusted;
pub mod web;

pub use backend::{LlmBackend, LlmBackendKind, LlmSettings, RoleProfile};
pub use claude_code::ClaudeCodeClient;
pub use ollama::{AiHealth, OllamaClient};

/// Errors produced at the `ai` boundary (docs/05-ai-integration.md D24 — AI errors degrade,
/// they do not crash the request).
#[derive(Debug, thiserror::Error)]
pub enum AiError {
    #[error("not implemented: {0}")]
    NotImplemented(&'static str),

    #[error("http error talking to Ollama: {0}")]
    Http(#[from] reqwest::Error),

    /// Hard timeout (D20): no token/response activity within the configured window. The caller
    /// aborts, surfaces the degraded state, and must NOT persist a partial assistant turn.
    #[error("ollama timed out (no activity within the hard-timeout window)")]
    Timeout,

    /// Ollama spoke something other than the expected NDJSON chat protocol (e.g. the stream
    /// ended before `done: true`). Treated like an aborted call — nothing partial becomes truth.
    #[error("ollama protocol error: {0}")]
    Protocol(String),

    /// A non-Ollama backend (e.g. the `claude` CLI) failed to spawn, exited abnormally, failed to
    /// authenticate, or produced output that did not parse. Terminal like the others — a partial
    /// reply is never persisted (D24 degrade-not-crash).
    #[error("llm backend error: {0}")]
    Backend(String),
}
