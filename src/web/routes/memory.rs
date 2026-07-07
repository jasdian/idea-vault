//! Idea lifecycle + concept actions (docs/09-web-ui.md D17): Store (R4), Reopen (R5), run a skill
//! (R6), and run a swarm (R7). These drive the state machine (docs/04-state-machine.md D9) and the
//! harness concepts (docs/06-concepts). Grouped here per D17's `memory`/idea-actions bucket.

use axum::extract::{Path, State};
use chrono::Utc;

use crate::ai::budget::ContextBudget;
use crate::app::AppState;
use crate::domain::IdeaState;
use crate::memory;
use crate::vault::store;
use crate::web::routes::ideas::build_discussion;
use crate::web::routes::{reindex_logged, AI_BUDGET_BYTES};
use crate::web::templates::{render_markdown, Discussion, Stored, Turn};
use crate::web::WebError;

/// R4 — `POST /idea/{slug}/store` — consolidate + extract memory, transition to `Stored` (D12).
///
/// Guards (D9): only `InDiscussion`/`Reopened` can store; an `InDiscussion` store needs at
/// least one turn. The extraction pipeline runs both AI calls (self-gated by the shared
/// semaphore, scoped to the calls only) before touching truth; the route then reindexes
/// (log-not-fail — truth already landed) and renders `_stored.html`.
pub async fn store_idea(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> Result<Stored, WebError> {
    let vault_dir = state.config.vault_dir.clone();
    let idea = store::read_idea(&vault_dir, &slug)?; // 404 if missing
    match idea.frontmatter.state {
        IdeaState::Stored => {
            return Err(WebError::BadRequest("idea is already stored".into()));
        }
        IdeaState::Draft => {
            return Err(WebError::BadRequest(
                "nothing to store yet — discuss the idea first".into(),
            ));
        }
        IdeaState::InDiscussion => {
            let conversation = store::read_conversation(&vault_dir, &slug)?;
            if store::split_turns(&conversation).is_empty() {
                return Err(WebError::BadRequest(
                    "store needs at least one discussion turn (D9)".into(),
                ));
            }
        }
        IdeaState::Reopened => {} // re-store merges memory, no turn guard (D9 table)
    }

    // The extraction pipeline acquires the shared permit itself, scoped to exactly its two
    // AI calls (ADR-0006) — this route must not hold one around it (deadlock rule).
    let outcome = memory::extract::extract_and_store(
        &state.ollama,
        &state.ai_semaphore,
        &vault_dir,
        &slug,
        ContextBudget::new(AI_BUDGET_BYTES),
    )
    .await?;
    reindex_logged(&state);
    tracing::info!(slug, new_facts = outcome.new_facts, "idea stored");

    Ok(Stored {
        slug,
        body_html: render_markdown(&outcome.consolidated_body),
    })
}

/// R5 — `POST /idea/{slug}/reopen` — re-enter discussion with memory loaded as context (D13).
///
/// Truth-idempotent apart from the state flip: memory context is loaded (index first, bodies
/// under budget) and the frontmatter flips `stored → reopened`; body and memory are untouched.
pub async fn reopen_idea(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> Result<Discussion, WebError> {
    let vault_dir = state.config.vault_dir.clone();
    let mut idea = store::read_idea(&vault_dir, &slug)?; // 404 if missing
    if idea.frontmatter.state != IdeaState::Stored {
        return Err(WebError::BadRequest(
            "only a stored idea can be reopened".into(),
        ));
    }

    // D13: MEMORY.md always, fact bodies under budget — the next chat turn (D11) reassembles
    // the same context; loading here validates it and surfaces inclusion counts.
    let loaded =
        memory::load::load_context(&vault_dir, &slug, ContextBudget::new(AI_BUDGET_BYTES))?;
    tracing::info!(
        slug,
        included_memory = loaded.included_memory,
        included_turns = loaded.included_turns,
        truncated = loaded.truncated,
        "reopen context loaded"
    );

    idea.frontmatter.state = IdeaState::Reopened;
    idea.frontmatter.updated = Utc::now();
    store::write_idea(&vault_dir, &idea)?;
    reindex_logged(&state);

    let conversation = store::read_conversation(&vault_dir, &slug)?;
    let health = state.ollama.probe().await;
    build_discussion(&slug, &conversation, health, state.ollama.model(), true)
}

/// R6 — `POST /idea/{slug}/skill/{name}` — apply a named ideation skill, returning a turn (D18).
pub async fn run_skill(
    State(_state): State<AppState>,
    Path((_slug, _name)): Path<(String, String)>,
) -> Result<Turn, WebError> {
    // TODO(D18): see docs/06-concepts/skills.md D18 — resolve the skill by name, run it through
    // `concepts::skills`, and render the resulting assistant `_turn.html`.
    Err(WebError::NotImplemented("web::routes::memory::run_skill"))
}

/// R7 — `POST /idea/{slug}/swarm` — fan out subagents, converge, return one turn (D14).
pub async fn run_swarm(
    State(_state): State<AppState>,
    Path(_slug): Path<String>,
) -> Result<Turn, WebError> {
    // TODO(D14): see docs/06-concepts/swarm.md D14 — fan out bounded subagents via `concepts::swarm`
    // (sharing the AI semaphore), synthesize, and render the converged `_turn.html`.
    Err(WebError::NotImplemented("web::routes::memory::run_swarm"))
}
