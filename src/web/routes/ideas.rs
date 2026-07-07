//! Ideas route group (docs/09-web-ui.md D17): the idea list (R1), a single idea view (R2),
//! creation (R3), and full-text search (R8).

use axum::extract::{Path, Query, State};
use axum::Form;
use chrono::Utc;
use serde::Deserialize;

use crate::app::AppState;
use crate::domain::{slug as domain_slug, Idea, IdeaFrontmatter, IdeaState};
use crate::index::{queries, reindex};
use crate::vault::store;
use crate::web::templates::{IdeaPage, IdeaRow, ListPage, SearchResults};
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

/// Form body for R3 (the `list.html` new-idea form posts `title`; a seed body is optional).
#[derive(Debug, Deserialize)]
pub struct CreateIdeaForm {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub body: String,
}

/// R3 — `POST /ideas` — create a new Draft idea (D10) and return its list row partial.
///
/// D10 sequence: validate title non-empty → slugify + collision-check against the vault (D22)
/// → write `idea.md` (state=draft) + empty `conversation.md` (truth first) → index upsert →
/// the `_idea_row.html` partial the list form swaps in.
pub async fn create_idea(
    State(state): State<AppState>,
    Form(form): Form<CreateIdeaForm>,
) -> Result<IdeaRow, WebError> {
    let title = form.title.trim();
    if title.is_empty() {
        return Err(WebError::BadRequest("title must not be empty".into()));
    }

    // D22: slug generated once at creation; the collision check is the atomic directory claim
    // inside `create_idea` — a raced duplicate loses with SlugTaken and we retry with the next
    // candidate, so existing truth can never be silently overwritten.
    let vault_dir = state.config.vault_dir.clone();
    let base = domain_slug::slugify(title);
    let now = Utc::now();
    let mut idea = Idea {
        frontmatter: IdeaFrontmatter {
            title: title.to_string(),
            slug: String::new(),
            state: IdeaState::Draft,
            tags: Vec::new(),
            created: now,
            updated: now,
        },
        body: if form.body.trim().is_empty() {
            String::new()
        } else {
            format!("{}\n", form.body.trim())
        },
    };

    // Truth first: markdown on disk (idea.md + an empty conversation.md per D10), then index.
    let slug = loop {
        let candidate =
            domain_slug::disambiguate(&base, |candidate| vault_dir.join(candidate).is_dir());
        idea.frontmatter.slug = candidate.clone();
        match store::create_idea(&vault_dir, &idea) {
            Ok(()) => break candidate,
            Err(crate::vault::VaultError::SlugTaken(_)) => continue,
            Err(e) => return Err(e.into()),
        }
    };

    // Index upsert. Full transactional rebuild is the canonical correct path (ADR-0002); a
    // per-idea incremental upsert is a future optimization once vault sizes warrant it. An
    // index failure is logged, not surfaced: the markdown truth already landed and the next
    // reindex reconciles (docs/03 "Consistency & failure model").
    {
        let mut conn = state
            .db
            .lock()
            .map_err(|e| WebError::Internal(format!("db mutex poisoned: {e}")))?;
        if let Err(e) = reindex::reindex(&mut conn, &vault_dir) {
            tracing::warn!(error = %e, slug, "index upsert after create failed; truth intact");
        }
    }

    Ok(IdeaRow {
        idea: queries::IdeaSummary {
            slug,
            title: idea.frontmatter.title.clone(),
            state: idea.frontmatter.state.as_str().to_string(),
            updated_at: now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        },
    })
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
