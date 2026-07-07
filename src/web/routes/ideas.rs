//! Ideas route group (docs/09-web-ui.md D17): the idea list (R1), a single idea view (R2),
//! creation (R3), and full-text search (R8).

use axum::extract::{Path, Query, State};
use serde::Deserialize;

use crate::app::AppState;
use crate::index::queries;
use crate::web::templates::{IdeaPage, ListPage, SearchResults};
use crate::web::WebError;

/// R1 — `GET /` — the vault overview: every idea, most-recently-updated first.
pub async fn list_page(State(state): State<AppState>) -> Result<ListPage, WebError> {
    let ideas = {
        let conn = state
            .db
            .lock()
            .map_err(|e| WebError::Internal(format!("db mutex poisoned: {e}")))?;
        queries::list_ideas(&conn)?
    };
    Ok(ListPage { ideas })
}

/// R2 — `GET /idea/{slug}` — one idea's view (body, conversation, memory).
pub async fn idea_page(
    State(_state): State<AppState>,
    Path(_slug): Path<String>,
) -> Result<IdeaPage, WebError> {
    // TODO(D10/D9): see docs/09-web-ui.md D17 & docs/04-state-machine.md — read the idea via
    // `vault::store::read_idea`, render its markdown body to sanitized HTML, and template it.
    Err(WebError::NotImplemented("web::routes::ideas::idea_page"))
}

/// R3 — `POST /ideas` — create a new Draft idea (D10) and return its list row.
pub async fn create_idea(State(_state): State<AppState>) -> Result<IdeaPage, WebError> {
    // TODO(D10): see docs/04-state-machine.md D10 — slugify the title, create the vault folder,
    // write idea.md in `Draft`, upsert the index, and return the `_idea_row.html` partial.
    Err(WebError::NotImplemented("web::routes::ideas::create_idea"))
}

/// Query string for R8 search.
#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    #[serde(default)]
    pub q: String,
}

/// R8 — `GET /search?q=` — full-text search results fragment.
pub async fn search(
    State(_state): State<AppState>,
    Query(_query): Query<SearchQuery>,
) -> Result<SearchResults, WebError> {
    // TODO(R8): see docs/09-web-ui.md D17 & docs/03-data-model.md D6 — run `index::queries::search`
    // and render the `_search_results.html` partial.
    Err(WebError::NotImplemented("web::routes::ideas::search"))
}
