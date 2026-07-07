//! Idea lifecycle + concept actions (docs/09-web-ui.md D17): Store (R4), Reopen (R5), run a skill
//! (R6), and run a swarm (R7). These drive the state machine (docs/04-state-machine.md D9) and the
//! harness concepts (docs/06-concepts). Grouped here per D17's `memory`/idea-actions bucket.

use axum::extract::{Path, State};

use crate::app::AppState;
use crate::web::templates::{Discussion, Stored, Turn};
use crate::web::WebError;

/// R4 — `POST /idea/{slug}/store` — consolidate + extract memory, transition to `Stored` (D12).
pub async fn store_idea(
    State(_state): State<AppState>,
    Path(_slug): Path<String>,
) -> Result<Stored, WebError> {
    // TODO(D12): see docs/04-state-machine.md D12 — produce the consolidated writeup, extract memory
    // via `memory::extract`, persist frontmatter `state: stored`, then render `_stored.html`.
    Err(WebError::NotImplemented("web::routes::memory::store_idea"))
}

/// R5 — `POST /idea/{slug}/reopen` — re-enter discussion with memory loaded as context (D13).
pub async fn reopen_idea(
    State(_state): State<AppState>,
    Path(_slug): Path<String>,
) -> Result<Discussion, WebError> {
    // TODO(D13): see docs/04-state-machine.md D13 — load MEMORY.md via `memory::load`, persist
    // frontmatter `state: reopened`, then render the `_discussion.html` pane.
    Err(WebError::NotImplemented("web::routes::memory::reopen_idea"))
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
