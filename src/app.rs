//! Application wiring: shared [`AppState`] and the axum [`build_router`] route map
//! (docs/01-architecture.md D25, docs/09-web-ui.md D16/D17).
//!
//! `AppState` is the cloneable bundle injected into every handler: config, the SQLite index
//! connection (behind a mutex), the LLM backend (Ollama or claude-code, docs/adr/0009), and the
//! single process-wide AI concurrency semaphore (ADR-0006 — chat and swarm share one bound).

use std::sync::{Arc, Mutex};

use axum::routing::{get, post};
use axum::Router;
use tokio::sync::Semaphore;
use tower_http::trace::TraceLayer;

use crate::config::Config;
use crate::web::routes::{admin, artifacts, chat, compact, ideas, mcp, memory, settings};

/// Cloneable shared state injected into handlers (docs/01-architecture.md "Cross-cutting concerns").
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: Arc<Mutex<rusqlite::Connection>>,
    pub llm: crate::ai::LlmBackend,
    pub ai_semaphore: Arc<Semaphore>,
    /// Built-in skill registry, populated at boot (docs/06-concepts/skills.md "Registry").
    pub skills: Arc<crate::concepts::skills::SkillRegistry>,
    /// In-flight background AI jobs, one per idea, so a slow model call survives the browser
    /// navigating away (`web::jobs`).
    pub jobs: crate::web::jobs::Jobs,
    /// Persistent MCP server registry (`mcp` module doc). The same `Arc` is handed to the LLM
    /// backend via `with_mcp`, so a registry edit here is live on the next model turn.
    pub mcp: Arc<crate::mcp::McpRegistry>,
}

/// Build the full axum router (D17 route map) with the tracing middleware layer (D16).
pub fn build_router(state: AppState) -> Router {
    Router::new()
        // Full pages (ideas group).
        .route("/", get(ideas::list_page))
        .route("/idea/{slug}", get(ideas::idea_page))
        // Idea create + lifecycle actions.
        .route("/ideas", post(ideas::create_idea))
        // Rename (title only — not a D9 transition; legal in every state, slug never changes).
        .route("/idea/{slug}/rename", post(ideas::rename_idea))
        .route("/idea/{slug}/tags", post(ideas::set_tags))
        .route("/idea/{slug}/store", post(memory::store_idea))
        .route("/idea/{slug}/reopen", post(memory::reopen_idea))
        .route("/idea/{slug}/skill/{name}", post(memory::run_skill))
        .route("/idea/{slug}/swarm", post(memory::run_swarm))
        .route("/idea/{slug}/workflow/{name}", post(memory::run_workflow))
        .route(
            "/idea/{slug}/turn/{index}/delete",
            post(memory::delete_turn),
        )
        .route(
            "/idea/{slug}/memory/{fact}/delete",
            post(memory::delete_memory_fact),
        )
        // Knowledge extraction + the per-idea artifact files it produces (docs/adr/0015).
        .route("/idea/{slug}/extract", post(artifacts::run_extract))
        .route(
            "/idea/{slug}/artifact/{name}",
            get(artifacts::view_artifact),
        )
        .route(
            "/idea/{slug}/artifact/{name}/delete",
            post(artifacts::delete_artifact),
        )
        // Chat + the background-job poll endpoint (D11 async model call).
        .route("/idea/{slug}/chat", post(chat::chat))
        .route("/idea/{slug}/pending", get(ideas::pending))
        // Cancel a running background job (abort the detached task; nothing partial is saved).
        .route("/idea/{slug}/cancel", post(ideas::cancel_job))
        // Manual auto-compact fold (docs/adr/0012).
        .route("/idea/{slug}/compact", post(compact::compact))
        // The "btw" history view + fork-to-new-idea.
        .route("/idea/{slug}/history", get(ideas::history_page))
        .route("/idea/{slug}/fork", post(ideas::fork_idea))
        .route("/idea/{slug}/delete", post(ideas::delete_idea))
        // Search.
        .route("/search", get(ideas::search))
        // Live LLM settings (backend toggle + params).
        .route("/settings", get(settings::settings_page))
        .route("/settings", post(settings::update_settings))
        // MCP server management (owner-configured tool endpoints, `crate::mcp`).
        .route("/mcp", get(mcp::mcp_page))
        .route("/mcp/add", post(mcp::add_server))
        .route("/mcp/{name}/toggle", post(mcp::toggle_server))
        .route("/mcp/{name}/delete", post(mcp::delete_server))
        .route("/mcp/{name}/probe", post(mcp::probe_server))
        .route("/mcp/{name}/edit", get(mcp::edit_server_form))
        .route("/mcp/{name}/view", get(mcp::view_server_row))
        .route("/mcp/{name}/update", post(mcp::update_server))
        // Admin.
        .route("/admin/health", get(admin::health))
        .route("/admin/reindex", post(admin::reindex))
        // Embedded static assets (htmx, css).
        .route("/static/{*path}", get(admin::static_asset))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
