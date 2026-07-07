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

/// Split one transcript turn into (role, markdown content): the first line's `## <role>`
/// heading names the speaker; text without a heading reads as a bare note.
fn turn_role_and_content(turn: &str) -> (String, String) {
    match turn.split_once('\n') {
        Some((first, rest)) if first.starts_with("## ") => (
            first.trim_start_matches("## ").trim().to_string(),
            rest.to_string(),
        ),
        _ => ("note".to_string(), turn.to_string()),
    }
}

/// Build the discussion pane for any discussion-state idea: rendered transcript turns plus the
/// D20 availability state with its per-state remedy copy. Shared with the reopen route (R5),
/// which returns this partial directly.
pub(crate) fn build_discussion(
    slug: &str,
    conversation: &str,
    health: crate::ai::AiHealth,
    model: &str,
    can_store: bool,
    skill_names: Vec<String>,
) -> Result<crate::web::templates::Discussion, WebError> {
    use askama::Template as _;

    // D20 per-state remedy copy (docs/05-ai-integration.md).
    let (ai_available, unavailable_hint) = match health {
        crate::ai::AiHealth::Available => (true, String::new()),
        crate::ai::AiHealth::ModelMissing => {
            (false, format!("pull a model: `ollama pull {model}`"))
        }
        crate::ai::AiHealth::Unreachable => (
            false,
            "Ollama is not reachable — start it with `ollama serve`".to_string(),
        ),
    };

    let turns_html = store::split_turns(conversation)
        .iter()
        .map(|turn| {
            let (role, content) = turn_role_and_content(turn);
            crate::web::templates::Turn {
                role,
                content_html: crate::web::templates::render_markdown(&content),
            }
            .render()
            .map_err(|e| WebError::Internal(format!("template render: {e}")))
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(crate::web::templates::Discussion {
        slug: slug.to_string(),
        ai_available,
        can_store,
        unavailable_hint,
        skill_names,
        turns_html,
    })
}

/// Render the state-dependent lower panel: `_stored.html` for a Stored idea (reopen button),
/// `_discussion.html` (transcript + compose box, disabled when AI is unavailable — D20) for
/// every discussion state. Pre-rendered so the partials stay the single source of truth for
/// both this full page and the HTMX swaps that replace `#discussion` later.
fn render_panel(
    idea: &Idea,
    conversation: &str,
    health: crate::ai::AiHealth,
    model: &str,
    skill_names: Vec<String>,
) -> Result<String, WebError> {
    use askama::Template as _;

    if idea.frontmatter.state == IdeaState::Stored {
        return crate::web::templates::Stored {
            slug: idea.frontmatter.slug.clone(),
            body_html: crate::web::templates::render_markdown(&idea.body),
        }
        .render()
        .map_err(|e| WebError::Internal(format!("template render: {e}")));
    }

    // Store is legal only from InDiscussion/Reopened (D9) — a Draft page must not offer it.
    let can_store = idea.frontmatter.state != IdeaState::Draft;
    build_discussion(
        &idea.frontmatter.slug,
        conversation,
        health,
        model,
        can_store,
        skill_names,
    )?
    .render()
    .map_err(|e| WebError::Internal(format!("template render: {e}")))
}

/// R2 — `GET /idea/{slug}` — one idea's view: rendered body, memory panel, and the
/// state-dependent discussion/stored panel (docs/09-web-ui.md).
pub async fn idea_page(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> Result<IdeaPage, WebError> {
    let vault_dir = &state.config.vault_dir;
    let idea = store::read_idea(vault_dir, &slug)?; // IdeaNotFound → 404
    let conversation = store::read_conversation(vault_dir, &slug)?;
    let memory_entries = store::read_memory_index(vault_dir, &slug)?.entries;

    // D20: the compose box is disabled (with a per-state remedy banner) unless the model is
    // ready; probing is bounded by the client's 1s hard timeout, so a down Ollama costs at
    // most that per page view.
    let health = state.ollama.probe().await;

    let skill_names = state.skills.list().iter().map(|s| s.name.clone()).collect();
    let panel_html = render_panel(
        &idea,
        &conversation,
        health,
        state.ollama.model(),
        skill_names,
    )?;
    Ok(IdeaPage {
        title: idea.frontmatter.title.clone(),
        slug: idea.frontmatter.slug.clone(),
        state: idea.frontmatter.state.as_str().to_string(),
        body_html: crate::web::templates::render_markdown(&idea.body),
        memory_entries,
        panel_html,
    })
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
