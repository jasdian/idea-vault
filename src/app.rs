//! Application wiring: shared [`AppState`] and the axum [`build_router`] route map
//! (docs/01-architecture.md D25, docs/09-web-ui.md D16/D17).
//!
//! `AppState` is the cloneable bundle injected into every handler: config, the SQLite index
//! connection (behind a mutex), the Ollama client, and the single process-wide AI concurrency
//! semaphore (ADR-0006 — chat and swarm share one bound).

use std::sync::{Arc, Mutex};

use axum::routing::{get, post};
use axum::Router;
use tokio::sync::Semaphore;
use tower_http::trace::TraceLayer;

use crate::config::Config;
use crate::web::routes::{admin, chat, ideas, memory};

/// Cloneable shared state injected into handlers (docs/01-architecture.md "Cross-cutting concerns").
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: Arc<Mutex<rusqlite::Connection>>,
    pub ollama: crate::ai::OllamaClient,
    pub ai_semaphore: Arc<Semaphore>,
}

/// Build the full axum router (D17 route map) with the tracing middleware layer (D16).
pub fn build_router(state: AppState) -> Router {
    Router::new()
        // Full pages (ideas group).
        .route("/", get(ideas::list_page))
        .route("/idea/{slug}", get(ideas::idea_page))
        // Idea create + lifecycle actions.
        .route("/ideas", post(ideas::create_idea))
        .route("/idea/{slug}/store", post(memory::store_idea))
        .route("/idea/{slug}/reopen", post(memory::reopen_idea))
        .route("/idea/{slug}/skill/{name}", post(memory::run_skill))
        .route("/idea/{slug}/swarm", post(memory::run_swarm))
        // Chat (SSE).
        .route("/idea/{slug}/chat", post(chat::chat))
        // Search.
        .route("/search", get(ideas::search))
        // Admin.
        .route("/admin/health", get(admin::health))
        .route("/admin/reindex", post(admin::reindex))
        // Embedded static assets (htmx, css).
        .route("/static/{*path}", get(admin::static_asset))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
