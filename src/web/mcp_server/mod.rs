//! Inbound MCP server (docs/adr/0024): exposes idea-vault itself as a Model Context Protocol
//! server, mounted at `/api/mcp` on the same axum app the web UI runs on — reusing the live
//! `AppState` directly rather than a separate process. This is the mirror image of `crate::mcp`
//! (the *outbound* registry of external MCP servers idea-vault calls) and `crate::ai::mcp` (the
//! *outbound* client) — three same-topic, opposite-direction modules, each already following the
//! crate's protocol/registry/bridge split.
//!
//! Built against the pattern in the owner's sibling `mcp-server` repo: `rmcp`'s
//! `ServerHandler` trait over the `StreamableHttpService` transport, with a Tower `AuthLayer`
//! gating the mount. Unlike that multi-tenant server, idea-vault is solo, so the gate is a single
//! static Bearer token (`auth`), not per-user credentials — `handler`'s `ServerHandler` never
//! needs to resolve a caller identity.
//!
//! Long-running tools (`chat`, `store_idea`) are bridged onto idea-vault's existing
//! background-job machinery (`web::jobs`, ADR-0010) via the MCP **Tasks** primitive (SEP-1686,
//! `rmcp` 1.8+) rather than holding an SSE connection open for the whole model call — see
//! `tasks` for the bridge and docs/adr/0024 for why this, not progress-notification streaming,
//! matches ADR-0010's reasoning.

mod auth;
mod handler;
mod prompts;
mod tasks;
mod tools;

use std::sync::Arc;

use axum::Router;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};

use crate::app::AppState;
use auth::AuthLayer;
use handler::IdeaVaultMcpServer;

/// Mount `/api/mcp`, gated by a single-token Bearer [`AuthLayer`]. Callers (`app::build_router`)
/// only invoke this when a token is configured — an unauthenticated tool surface is not a safe
/// default, so the absence of a token means this function is never called, not that it mounts
/// open.
pub fn router(state: AppState, token: String) -> Router {
    let handler = IdeaVaultMcpServer::new(state);
    let service = StreamableHttpService::new(
        move || Ok(handler.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );

    Router::new()
        .route_service("/api/mcp", service)
        .layer(AuthLayer::new(token))
}
