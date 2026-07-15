//! Admin routes (docs/09-web-ui.md D17): the health probe (R11), the reindex trigger (R10), and
//! the embedded static-asset handler.
//!
//! Health draws one line, and it is not "is everything perfect" (ADR-0019): an absent MODEL is a
//! valid state (D20 — the Docker HEALTHCHECK must pass on a model-less stack, so the LLM never
//! fails the probe), but an unusable VAULT is not a valid state at all — without it the app cannot
//! read or write the truth it exists to serve, so that alone returns `503` and turns the container
//! red. Body shape is unchanged and always present; only the status code moves.

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::ai::AiHealth;
use crate::app::AppState;
use crate::vault::VaultHealth;
use crate::web::WebError;

/// R11 — `GET /admin/health` — probe the LLM backend and the vault. `200` when the vault is
/// usable (whatever the model is doing), `503` when it is not.
pub async fn health(State(state): State<AppState>) -> Response {
    let llm = match state.llm.probe().await {
        AiHealth::Available => "ok",
        AiHealth::ModelMissing => "model-missing",
        AiHealth::Unreachable => "unreachable",
    };
    let backend = match state.llm.settings().backend {
        crate::config::LlmBackendKind::Ollama => "ollama",
        crate::config::LlmBackendKind::ClaudeCode => "claude-code",
    };

    let vault_dir = &state.config.vault_dir;
    let vault = match crate::vault::probe_vault(vault_dir) {
        VaultHealth::Ok => "ok",
        VaultHealth::Unreadable => "unreadable",
        VaultHealth::Unwritable => "unwritable",
    };
    // Advisory only — a marker-less vault is reported but never fails the probe. A brand-new vault
    // made by hand (and every test that mkdir's a tempdir) legitimately has no marker yet.
    let marked = vault_dir.join(crate::vault::VAULT_MARKER).is_file();

    let (status, overall) = if vault == "ok" {
        (StatusCode::OK, "ok")
    } else {
        tracing::error!(dir = %vault_dir.display(), vault, "vault unusable; reporting 503");
        (StatusCode::SERVICE_UNAVAILABLE, "vault-unusable")
    };

    (
        status,
        Json(json!({
            "status": overall,
            "backend": backend,
            "llm": llm,
            "vault": vault,
            "vault_marked": marked,
        })),
    )
        .into_response()
}

/// Query for [`reindex`] — `?force=1` to override the empty-vault guard.
#[derive(Debug, Deserialize)]
pub struct ReindexQuery {
    #[serde(default)]
    force: Option<String>,
}

impl ReindexQuery {
    fn forced(&self) -> bool {
        matches!(self.force.as_deref(), Some("1" | "true" | "yes"))
    }
}

/// R10 — `POST /admin/reindex` — rebuild the derived index from the vault (D15), returning the
/// counts for verification. This is the manual reconcile for edits the boot drift-check cannot
/// see (hand-edited conversations/memory files); the index is always rebuildable (ADR-0002).
pub async fn reindex(
    State(state): State<AppState>,
    Query(query): Query<ReindexQuery>,
) -> Result<Json<serde_json::Value>, WebError> {
    let forced = query.forced();
    let counts = {
        let mut conn = state
            .db
            .lock()
            .map_err(|e| WebError::Internal(format!("db mutex poisoned: {e}")))?;
        if forced {
            tracing::warn!(
                vault = %state.config.vault_dir.display(),
                "forced reindex: the empty-vault guard was bypassed by an explicit ?force=1"
            );
            crate::index::reindex::reindex_forced(&mut conn, &state.config.vault_dir)?
        } else {
            crate::index::reindex::reindex(&mut conn, &state.config.vault_dir)?
        }
    };
    tracing::info!(
        ideas = counts.ideas,
        facts = counts.facts,
        links = counts.links,
        "manual reindex complete"
    );
    Ok(Json(json!({
        "ideas": counts.ideas,
        "facts": counts.facts,
        "links": counts.links,
    })))
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
