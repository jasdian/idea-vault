//! Admin routes (docs/09-web-ui.md D17): the Ollama health probe (R11), the reindex trigger (R10),
//! and the embedded static-asset handler. Health is always `200` — Ollama absence is a valid state
//! (D20), and the Docker HEALTHCHECK must pass on a model-less stack.

use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::ai::AiHealth;
use crate::app::AppState;
use crate::web::WebError;

/// R11 — `GET /admin/health` — probe Ollama; always `200`, encoding health in the body (D20).
pub async fn health(State(state): State<AppState>) -> Json<serde_json::Value> {
    let ollama = match state.ollama.probe().await {
        AiHealth::Available => "ok",
        AiHealth::ModelMissing => "model-missing",
        AiHealth::Unreachable => "unreachable",
    };
    Json(json!({ "status": "ok", "ollama": ollama }))
}

/// R10 — `POST /admin/reindex` — rebuild the derived index from the vault (D15).
pub async fn reindex(State(_state): State<AppState>) -> Result<Json<serde_json::Value>, WebError> {
    // TODO(D15): see docs/03-data-model.md D15 — run `index::reindex::reindex` against the vault
    // under the db lock and return the resulting counts.
    Err(WebError::NotImplemented("web::routes::admin::reindex"))
}

/// Embedded static assets (single-binary — ADR-0001): `static/` is baked into the binary.
#[derive(rust_embed::Embed)]
#[folder = "static/"]
struct StaticAssets;

/// `GET /static/{*path}` — serve a vendored asset (htmx, css) with a content type by extension.
pub async fn static_asset(Path(path): Path<String>) -> Response {
    match StaticAssets::get(&path) {
        Some(file) => {
            let content_type = match path.rsplit('.').next() {
                Some("js") => "application/javascript",
                Some("css") => "text/css",
                _ => "text/plain",
            };
            (
                [(header::CONTENT_TYPE, content_type)],
                file.data.into_owned(),
            )
                .into_response()
        }
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}
