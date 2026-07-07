//! Idea lifecycle + concept actions (docs/09-web-ui.md D17): Store (R4), Reopen (R5), run a skill
//! (R6), and run a swarm (R7). These drive the state machine (docs/04-state-machine.md D9) and the
//! harness concepts (docs/06-concepts). Grouped here per D17's `memory`/idea-actions bucket.

use axum::extract::{Path, State};
use chrono::Utc;

use crate::ai::budget::ContextBudget;
use crate::app::AppState;
use crate::concepts;
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
    let skill_names = state.skills.list().iter().map(|s| s.name.clone()).collect();
    build_discussion(
        &slug,
        &conversation,
        health,
        state.ollama.model(),
        true,
        skill_names,
    )
}

/// Concept actions run only in the two active discussion states (D9 has no skill/swarm edge
/// for `Draft` or `Stored`). Exhaustive match: a future state must make an explicit decision
/// here rather than falling through to "allowed".
fn guard_discussion_state(state: IdeaState) -> Result<(), WebError> {
    match state {
        IdeaState::InDiscussion | IdeaState::Reopened => Ok(()),
        IdeaState::Draft => Err(WebError::BadRequest(
            "idea is a draft — open the discussion with a first chat turn before running moves"
                .into(),
        )),
        IdeaState::Stored => Err(WebError::BadRequest(
            "idea is stored — reopen it before running moves".into(),
        )),
    }
}

/// R6 — `POST /idea/{slug}/skill/{name}` — apply a named ideation skill, returning a turn (D18).
///
/// Stateless: the output appends as an assistant turn (inside `invoke`, post-completion) and
/// idea state never changes. `invoke` gates its own AI call on the shared semaphore.
pub async fn run_skill(
    State(state): State<AppState>,
    Path((slug, name)): Path<(String, String)>,
) -> Result<Turn, WebError> {
    let vault_dir = state.config.vault_dir.clone();
    let idea = store::read_idea(&vault_dir, &slug)?; // 404 if missing
    guard_discussion_state(idea.frontmatter.state)?;

    let Some(skill) = state.skills.get(&name) else {
        return Err(WebError::NotFound(format!("skill: {name}")));
    };

    let output = concepts::skills::invoke(
        &state.ollama,
        &state.ai_semaphore,
        &vault_dir,
        &slug,
        skill,
        ContextBudget::new(AI_BUDGET_BYTES),
    )
    .await?;
    reindex_logged(&state);

    let content = if output.is_empty() {
        "*(the model returned nothing)*".to_string()
    } else {
        output
    };
    Ok(Turn {
        role: format!("assistant (skill: {name})"),
        content_html: render_markdown(&content),
    })
}

/// Form body for R7: optional comma-separated angle list; defaults to the canonical D14 set.
#[derive(Debug, serde::Deserialize)]
pub struct SwarmForm {
    #[serde(default)]
    pub angles: String,
}

/// The canonical D14 angle set (docs/06-concepts/swarm.md: "swarm(idea, angles=[premortem,
/// disproof, constraints, 2nd-order])").
/// Upper bound on one swarm request's fan-out: the semaphore bounds concurrency (K in
/// flight), this bounds total queued work N so a single request cannot monopolize the shared
/// AI budget for every other route (ADR-0006 spirit: bounded latency, not just bounded rate).
const MAX_ANGLES: usize = 8;

const DEFAULT_ANGLES: [&str; 4] = [
    "premortem",
    "cheapest-disproof",
    "constraints",
    "second-order-effects",
];

/// R7 — `POST /idea/{slug}/swarm` — fan out subagents, converge, return one turn (D14).
///
/// The swarm orchestrator bounds itself on the shared semaphore (max K in flight, N−K queue)
/// and persists only the converged synthesis; this route renders it as the returned turn.
pub async fn run_swarm(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    axum::Form(form): axum::Form<SwarmForm>,
) -> Result<Turn, WebError> {
    let vault_dir = state.config.vault_dir.clone();
    let idea = store::read_idea(&vault_dir, &slug)?; // 404 if missing
    guard_discussion_state(idea.frontmatter.state)?;

    let angles: Vec<String> = if form.angles.trim().is_empty() {
        DEFAULT_ANGLES.iter().map(|a| a.to_string()).collect()
    } else {
        form.angles
            .split(',')
            .map(|a| a.trim().to_string())
            .filter(|a| !a.is_empty())
            .collect()
    };
    if angles.len() > MAX_ANGLES {
        return Err(WebError::BadRequest(format!(
            "too many angles: {} (max {MAX_ANGLES})",
            angles.len()
        )));
    }

    let outcome = concepts::swarm::swarm(
        &state.ollama,
        &state.ai_semaphore,
        &state.skills,
        &vault_dir,
        &slug,
        angles,
        ContextBudget::new(AI_BUDGET_BYTES),
    )
    .await?;
    reindex_logged(&state);

    // Parity with run_skill: an Ok-but-empty synthesis is surfaced as a placeholder turn
    // (nothing was persisted for it — a reload drops the bubble, which the text explains).
    let content = if outcome.synthesis.is_empty() {
        "*(the model returned nothing)*".to_string()
    } else {
        outcome.synthesis
    };
    Ok(Turn {
        role: "assistant (swarm)".to_string(),
        content_html: render_markdown(&content),
    })
}
