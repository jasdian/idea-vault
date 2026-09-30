//! Application wiring: the axum [`build_router`] route map over the shared [`AppState`]
//! (docs/01-architecture.md D25, docs/09-web-ui.md D16/D17).
//!
//! `AppState` itself lives in `web::state` (D4: only `app → web`) and is re-exported here, so
//! `idea_vault::app::AppState` keeps working for `main.rs` and the tests.

use axum::routing::{get, post};
use axum::Router;
use tower_http::trace::TraceLayer;

use crate::web::routes::{
    admin, artifacts, chat, compact, ideas, make_skill, mcp, memory, plans, runs, settings, skills,
    sources,
};
pub use crate::web::state::AppState;

/// Build the full axum router (D17 route map) with the tracing middleware layer (D16).
///
/// If [`Config::mcp_server_token`](crate::config::Config::mcp_server_token) is set, the inbound
/// MCP server (docs/adr/0024) is additionally mounted at `/api/mcp` — a separate path from the
/// outbound `/mcp*` registry-management UI above, gated by its own Bearer `AuthLayer`
/// (`web::mcp_server`). Unset: not mounted at all.
pub fn build_router(state: AppState) -> Router {
    let router = Router::new()
        // Full pages (ideas group).
        .route("/", get(ideas::list_page))
        .route("/idea/{slug}", get(ideas::idea_page))
        // Idea create + lifecycle actions.
        .route("/ideas", post(ideas::create_idea))
        // Rename (title only — not a D9 transition; legal in every state, slug never changes).
        .route("/idea/{slug}/rename", post(ideas::rename_idea))
        .route("/idea/{slug}/tags", post(ideas::set_tags))
        // Per-idea attached reference sources (ADR-0021; frontmatter `sources:` is truth).
        .route("/idea/{slug}/sources", post(ideas::set_sources))
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
        // Make skill (docs/adr/0042): R51 drafts as a job; R52 saves a reviewed draft.
        .route("/idea/{slug}/make-skill", post(make_skill::make_skill))
        // The plan workbench (docs/adr/0032): answer into a new version, the lineage head, and
        // a model re-plan as a background job.
        .route("/idea/{slug}/plan/latest", get(plans::latest_plan))
        .route("/idea/{slug}/plan/{stem}/answer", post(plans::answer_plan))
        .route("/idea/{slug}/plan/{stem}/replan", post(plans::replan))
        // Chat + the background-job poll endpoint (D11 async model call).
        .route("/idea/{slug}/chat", post(chat::chat))
        // Remove a message still waiting in the per-idea send queue.
        .route("/idea/{slug}/queue/{id}/delete", post(chat::remove_queued))
        .route("/idea/{slug}/pending", get(ideas::pending))
        // Cancel a running background job (abort the detached task; nothing partial is saved).
        .route("/idea/{slug}/cancel", post(ideas::cancel_job))
        // Manual auto-compact fold (docs/adr/0012).
        .route("/idea/{slug}/compact", post(compact::compact))
        // The "btw" history view + fork-to-new-idea.
        .route("/idea/{slug}/history", get(ideas::history_page))
        // R50: the read-only run inspector over one run journal (ADR-0037).
        .route("/idea/{slug}/runs/{run_id}", get(runs::run_page))
        .route("/idea/{slug}/fork", post(ideas::fork_idea))
        .route("/idea/{slug}/delete", post(ideas::delete_idea))
        // Search.
        .route("/search", get(ideas::search))
        // Live LLM settings (backend toggle + params).
        .route("/settings", get(settings::settings_page))
        .route("/settings", post(settings::update_settings))
        // The skill book: built-in + owner skills by spine stage, with live reload (ADR-0022).
        .route("/skills", get(skills::skills_page))
        .route("/skills/reload", post(skills::reload_skills))
        // R49: one workflow in full — stages, cost, rubric and source (ADR-0035).
        .route("/skills/workflow/{name}", get(skills::workflow_page))
        // MCP server management (owner-configured tool endpoints, `crate::mcp`).
        .route("/mcp", get(mcp::mcp_page))
        .route("/mcp/add", post(mcp::add_server))
        .route("/mcp/{name}/toggle", post(mcp::toggle_server))
        .route("/mcp/{name}/delete", post(mcp::delete_server))
        .route("/mcp/{name}/probe", post(mcp::probe_server))
        .route("/mcp/{name}/edit", get(mcp::edit_server_form))
        .route("/mcp/{name}/view", get(mcp::view_server_row))
        .route("/mcp/{name}/update", post(mcp::update_server))
        // Named reference sources (owner-registered read-only lookups, `crate::sources`).
        .route("/sources", get(sources::sources_page))
        .route("/sources/add", post(sources::add_source))
        .route("/sources/{name}/edit", get(sources::edit_source_form))
        .route("/sources/{name}/view", get(sources::view_source_row))
        .route("/sources/{name}/update", post(sources::update_source))
        .route("/sources/{name}/delete", post(sources::delete_source))
        // Admin.
        .route("/admin/health", get(admin::health))
        .route("/admin/reindex", post(admin::reindex))
        // Embedded static assets (htmx, css).
        .route("/static/{*path}", get(admin::static_asset))
        .layer(TraceLayer::new_for_http())
        .with_state(state.clone());

    match state.config.mcp_server_token.clone() {
        Some(token) => router.merge(crate::web::mcp_server::router(state, token)),
        None => router,
    }
}
