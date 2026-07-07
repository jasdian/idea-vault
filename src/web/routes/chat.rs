//! Chat route (docs/09-web-ui.md D17 R9): one discussion turn.
//!
//! Design note (2026-07): the original SSE token-streaming approach never worked in a browser —
//! the htmx SSE extension wasn't vendored, so a plain `hx-post` received a `text/event-stream`
//! it couldn't render. This handler is a normal blocking POST that returns the re-rendered
//! transcript HTML (the same model the skill/swarm buttons use). No orphan turns: the model is
//! called *before* anything is persisted, so a failed call saves nothing (the D11 "partial turn
//! never becomes truth" boundary, achieved by ordering rather than streaming).

use axum::extract::{Path, State};
use axum::response::Html;
use axum::Form;
use chrono::Utc;
use serde::Deserialize;

use crate::ai::budget::ContextBudget;
use crate::ai::ollama::ChatMessage;
use crate::app::AppState;
use crate::domain::IdeaState;
use crate::memory;
use crate::vault::store;
use crate::web::routes::ideas::render_transcript;
use crate::web::routes::{reindex_logged, AI_BUDGET_BYTES};
use crate::web::WebError;

/// The rigorous-foil persona for free chat (CLAUDE.md: steelman, then stress-test).
const FOIL_INSTRUCTION: &str = "You are a rigorous ideation foil. Engage with the idea below: \
steelman the owner's latest point first, then stress-test it from the angle they are not \
looking at. Be concrete and brief.";

#[derive(Debug, Deserialize)]
pub struct ChatForm {
    #[serde(default)]
    pub message: String,
}

/// R9 — `POST /idea/{slug}/chat` — one discussion turn; returns the re-rendered transcript.
///
/// Persist ordering (no orphans): the model is called first with the assembled context plus the
/// new message; only on success are the user turn and the assistant turn appended together and
/// the Draft→InDiscussion transition made. A model failure persists nothing and returns 503.
pub async fn chat(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    Form(form): Form<ChatForm>,
) -> Result<Html<String>, WebError> {
    let message = form.message.trim().to_string();
    if message.is_empty() {
        return Err(WebError::BadRequest("message must not be empty".into()));
    }

    let vault_dir = state.config.vault_dir.clone();
    let mut idea = store::read_idea(&vault_dir, &slug)?; // 404 if missing
    if idea.frontmatter.state == IdeaState::Stored {
        return Err(WebError::BadRequest(
            "idea is stored — reopen it before chatting".into(),
        ));
    }

    // Budgeted context (D21) from the transcript so far, plus the owner's new message appended —
    // nothing is written yet, so a failed model call leaves no orphan user turn.
    let context =
        memory::load::load_context(&vault_dir, &slug, ContextBudget::new(AI_BUDGET_BYTES))?;
    let prompt = format!(
        "{FOIL_INSTRUCTION}\n\n{}\n\n## Owner's latest message\n{message}",
        context.text
    );

    let reply = {
        let _permit = state
            .ai_semaphore
            .acquire()
            .await
            .map_err(|_| WebError::Internal("ai semaphore closed".into()))?;
        state
            .llm
            .chat(vec![ChatMessage {
                role: "user".to_string(),
                content: prompt,
            }])
            .await?
        // permit released before the vault writes below
    };
    let reply = reply.trim();

    // Success — now (and only now) persist both turns and make the D9 transition.
    store::append_turn(&vault_dir, &slug, "user", &message)?;
    if !reply.is_empty() {
        store::append_turn(&vault_dir, &slug, "assistant", reply)?;
    } else {
        tracing::warn!(slug, "model returned an empty reply");
    }
    if idea.frontmatter.state == IdeaState::Draft {
        idea.frontmatter.state = IdeaState::InDiscussion;
        idea.frontmatter.updated = Utc::now();
        store::write_idea(&vault_dir, &idea)?;
    }
    reindex_logged(&state);

    render_transcript(&vault_dir, &slug)
}
