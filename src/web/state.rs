//! The shared handler state (docs/01-architecture.md "Cross-cutting concerns"). It lives in `web`
//! so handlers never reach up into `app`; `app` builds the router from it (D4: `app → web` only).

use std::sync::{Arc, Mutex};

use tokio::sync::Semaphore;

use crate::config::Config;

/// Cloneable shared state injected into handlers (docs/01-architecture.md "Cross-cutting concerns").
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: Arc<Mutex<rusqlite::Connection>>,
    pub llm: crate::ai::LlmBackend,
    pub ai_semaphore: Arc<Semaphore>,
    /// The live skill registry: built-ins plus the owner's `vault/.skills/` (docs/adr/0022).
    /// Handlers take one `snapshot()` per request/job so a reload never changes a run mid-flight.
    pub skills: Arc<crate::concepts::skills::LiveSkills>,
    /// In-flight background AI jobs, one per idea, so a slow model call survives the browser
    /// navigating away (`web::jobs`).
    pub jobs: crate::web::jobs::Jobs,
    /// Per-idea FIFO of chat messages sent while a job was already running — drained by the poll
    /// loop as the idea goes idle, instead of dropping the message (`web::jobs` queue).
    pub queues: crate::web::jobs::Queues,
    /// Persistent MCP server registry (`mcp` module doc). The same `Arc` is handed to the LLM
    /// backend via `with_mcp`, so a registry edit here is live on the next model turn.
    pub mcp: Arc<crate::mcp::McpRegistry>,
    /// Persistent named-source registry (`sources` module doc). Live like `mcp`: a Sources-page
    /// edit is visible to the very next model turn with no restart.
    pub sources: Arc<crate::sources::SourceRegistry>,
}
