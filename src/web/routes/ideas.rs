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

/// Turn the stored role heading into a display label + whether it's the owner's turn:
/// `user` → `you`; `assistant` → `foil`; `assistant (skill: premortem)` → `foil · premortem`;
/// `assistant (swarm)` → `foil · swarm`; `assistant (workflow: x)` → `foil · workflow x`.
fn turn_label(role: &str) -> (String, bool) {
    if role == "user" {
        return ("you".to_string(), true);
    }
    if let Some(rest) = role.strip_prefix("assistant") {
        let rest = rest.trim();
        if rest.is_empty() {
            return ("foil".to_string(), false);
        }
        let inner = rest.trim_start_matches('(').trim_end_matches(')');
        let lens = inner
            .split_once(':')
            .map(|(_, v)| v.trim())
            .unwrap_or(inner);
        return (format!("foil · {lens}"), false);
    }
    (role.to_string(), false)
}

/// Render each turn of a transcript to HTML. Shared by the discussion pane (which has the text)
/// and [`render_transcript`] (which reads it) so chat/skill/swarm/delete re-render identically.
fn turns_to_html(slug: &str, conversation: &str) -> Result<Vec<String>, WebError> {
    use askama::Template as _;
    store::split_turns(conversation)
        .iter()
        .enumerate()
        .map(|(index, turn)| {
            let (role, content) = turn_role_and_content(turn);
            let (label, is_user) = turn_label(&role);
            crate::web::templates::Turn {
                label,
                is_user,
                content_html: crate::web::templates::render_markdown(&content),
                slug: slug.to_string(),
                index,
            }
            .render()
            .map_err(|e| WebError::Internal(format!("template render: {e}")))
        })
        .collect()
}

/// Minimal HTML-escape for text dropped into server-built markup (error text, model name).
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The quiet usage line: turns, approximate context vs budget, and the model in use. Answers
/// "what is being sent and to whom" at a glance.
fn meter_line(model: &str, turns: usize, bytes: usize) -> String {
    let kb = bytes.div_ceil(1024);
    let budget_kb = crate::web::routes::AI_BUDGET_BYTES / 1024;
    let plural = if turns == 1 { "" } else { "s" };
    format!(
        r#"<div class="meter">{turns} turn{plural} · ~{kb} KB of ~{budget_kb} KB context · {model}</div>"#,
        model = esc(model)
    )
}

/// The server-driven "thinking" indicator. It self-polls `/pending` 1.5s after it lands in the
/// DOM, so it keeps refreshing (and the elapsed count keeps climbing) until the job finishes —
/// and it re-appears on a fresh page load while a job runs, so navigating away never loses it.
fn pending_block(slug: &str, secs: u64) -> String {
    format!(
        r##"<div class="foil-pending" role="status" aria-live="polite" hx-get="/idea/{slug}/pending" hx-trigger="load delay:1500ms" hx-target="#transcript" hx-swap="innerHTML"><span class="dots" aria-hidden="true"><i></i><i></i><i></i></span><span>the foil is thinking — {secs}s</span></div>"##
    )
}

fn error_block(message: &str) -> String {
    format!(
        r#"<div class="foil-error" role="alert"><strong>The foil could not respond.</strong> {}</div>"#,
        esc(message)
    )
}

/// The complete inner HTML of `#transcript`: the turns, then a job indicator / error block if a
/// job is active, then the usage meter. This is the single renderer every transcript response
/// goes through — the idea page, the poll endpoint, and chat/skill/swarm/delete all emit it, so
/// the view is identical whether freshly loaded or swapped in.
pub(crate) fn transcript_inner(
    slug: &str,
    model: &str,
    conversation: &str,
    pending: crate::web::jobs::Pending,
) -> Result<String, WebError> {
    use crate::web::jobs::Pending;
    let turns = turns_to_html(slug, conversation)?;
    let mut html = String::new();
    if turns.is_empty() && matches!(pending, Pending::Idle) {
        html.push_str(
            r#"<p class="empty-thread">No exchange yet. Push the idea below and let the foil break it.</p>"#,
        );
    }
    html.push_str(&turns.concat());
    match pending {
        Pending::Running(secs) => html.push_str(&pending_block(slug, secs)),
        Pending::Failed(msg) => html.push_str(&error_block(&msg)),
        Pending::Idle => {}
    }
    html.push_str(&meter_line(
        model,
        store::split_turns(conversation).len(),
        conversation.len(),
    ));
    Ok(html)
}

/// Read the current transcript + job state and render it — the response chat/skill/swarm/delete
/// and the poll endpoint return.
pub(crate) fn respond_with_transcript(
    state: &AppState,
    slug: &str,
) -> Result<axum::response::Html<String>, WebError> {
    let conversation = store::read_conversation(&state.config.vault_dir, slug)?;
    let pending = crate::web::jobs::peek(&state.jobs, slug);
    Ok(axum::response::Html(transcript_inner(
        slug,
        &state.llm.model(),
        &conversation,
        pending,
    )?))
}

/// Render the memory panel (`_memory.html`) — the always-on MEMORY.md index with per-fact delete.
/// Shared by the idea page and the fact-delete route (which swaps `#memory`).
pub(crate) fn render_memory_panel(
    idea_slug: &str,
    entries: Vec<crate::domain::memory::MemoryIndexEntry>,
) -> Result<String, WebError> {
    use askama::Template as _;
    crate::web::templates::MemoryPanel {
        idea_slug: idea_slug.to_string(),
        entries,
    }
    .render()
    .map_err(|e| WebError::Internal(format!("template render: {e}")))
}

/// `GET /idea/{slug}/pending` — the poll target: return the current transcript, still carrying the
/// indicator while the job runs, an error once it fails, or the finished transcript when done.
pub async fn pending(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> Result<axum::response::Html<String>, WebError> {
    store::read_idea(&state.config.vault_dir, &slug)?; // 404 if the idea is gone
    respond_with_transcript(&state, &slug)
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
    pending: crate::web::jobs::Pending,
) -> Result<crate::web::templates::Discussion, WebError> {
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

    // The #transcript inner is the one shared renderer — so a fresh page load carries the same
    // in-flight indicator (or error) that the poll endpoint would, and mid-job navigation resumes.
    let transcript_html = transcript_inner(slug, model, conversation, pending)?;

    Ok(crate::web::templates::Discussion {
        slug: slug.to_string(),
        ai_available,
        can_store,
        unavailable_hint,
        skill_names,
        transcript_html,
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
    pending: crate::web::jobs::Pending,
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
        pending,
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
    let memory_html =
        render_memory_panel(&slug, store::read_memory_index(vault_dir, &slug)?.entries)?;

    // D20: the compose box is disabled (with a per-state remedy banner) unless the model is
    // ready; probing is bounded by the client's 1s hard timeout, so a down Ollama costs at
    // most that per page view.
    let health = state.llm.probe().await;

    let skill_names = state.skills.list().iter().map(|s| s.name.clone()).collect();
    // If a background job is running for this idea, this resumes its indicator on the fresh page.
    let pending = crate::web::jobs::peek(&state.jobs, &slug);
    let panel_html = render_panel(
        &idea,
        &conversation,
        health,
        &state.llm.model(),
        skill_names,
        pending,
    )?;
    Ok(IdeaPage {
        title: idea.frontmatter.title.clone(),
        slug: idea.frontmatter.slug.clone(),
        state: idea.frontmatter.state.as_str().to_string(),
        body_html: crate::web::templates::render_markdown(&idea.body),
        memory_html,
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
    State(state): State<AppState>,
    Query(query): Query<SearchQuery>,
) -> Result<SearchResults, WebError> {
    let hits = {
        let conn = state
            .db
            .lock()
            .map_err(|e| WebError::Internal(format!("db mutex poisoned: {e}")))?;
        // queries::search compiles any input to an injection-proof FTS MATCH expression;
        // empty/whitespace input yields no hits (the fragment renders its empty state).
        queries::search(&conn, &query.q)?
    };
    Ok(SearchResults { hits })
}
