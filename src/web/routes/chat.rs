//! Chat route (docs/09-web-ui.md D17 R9): one discussion turn, run as a background job.
//!
//! The model call is a *detached* task, not the request future — so navigating away (or a dropped
//! connection) can't cancel the generation, and the reply lands in `conversation.md` regardless of
//! who is watching (`web::jobs`). The user turn is persisted up front so it survives navigation and
//! shows beneath the "thinking" indicator; the assistant turn is appended when the task completes.
//! One job per idea: a second Send while busy just re-shows the in-flight state.

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
use crate::web::jobs;
use crate::web::routes::ideas::respond_with_transcript;
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

/// R9 — `POST /idea/{slug}/chat` — start one discussion turn; returns the transcript with the
/// "thinking" indicator, which polls `/pending` to completion.
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

    // Busy already: don't queue a second turn — just re-show the in-flight state.
    if !jobs::try_claim(&state.jobs, &slug) {
        return respond_with_transcript(&state, &slug);
    }

    // Persist the user turn now (survives navigation, shows under the indicator) and make the D9
    // Draft→InDiscussion transition. If this fails, release the slot so the idea isn't stuck busy.
    if let Err(e) = store::append_turn(&vault_dir, &slug, "user", &message) {
        jobs::mark_done(&state.jobs, &slug);
        return Err(e.into());
    }
    if idea.frontmatter.state == IdeaState::Draft {
        idea.frontmatter.state = IdeaState::InDiscussion;
        idea.frontmatter.updated = Utc::now();
        let _ = store::write_idea(&vault_dir, &idea);
    }
    reindex_logged(&state);

    // Detached: the reply outlives this request.
    let task_state = state.clone();
    let task_slug = slug.clone();
    tokio::spawn(async move {
        // Phase 0: pre-emptive, best-effort compaction (auto-compact, docs/adr/0012). It runs
        // BEFORE the reply so the very turn that tripped the threshold is answered off the freshly
        // compacted context — but a compaction failure NEVER fails the turn: it is logged and the
        // reply proceeds with the fallback (uncompacted) context.
        if let Err(e) = memory::compact::maybe_run_compaction(
            &task_state.llm,
            &task_state.ai_semaphore,
            &task_state.config.vault_dir,
            &task_slug,
        )
        .await
        {
            tracing::warn!(slug = %task_slug, "compaction skipped (fallback context): {e}");
        }
        // Phase 1: the reply, off the (possibly) freshly-compacted context.
        match run_chat(&task_state, &task_slug).await {
            Ok(()) => jobs::mark_done(&task_state.jobs, &task_slug),
            Err(msg) => jobs::mark_failed(&task_state.jobs, &task_slug, msg),
        }
    });

    respond_with_transcript(&state, &slug)
}

/// The background half: assemble the budgeted context (which already includes the just-persisted
/// user turn), call the model under the shared semaphore, and append the assistant turn. Returns a
/// human-readable message on failure for the indicator to surface.
async fn run_chat(state: &AppState, slug: &str) -> Result<(), String> {
    let vault_dir = &state.config.vault_dir;
    let context = memory::load::load_context(vault_dir, slug, ContextBudget::new(AI_BUDGET_BYTES))
        .map_err(|e| e.to_string())?;
    let prompt = format!("{FOIL_INSTRUCTION}\n\n{}", context.text);

    let reply = {
        let _permit = state
            .ai_semaphore
            .acquire()
            .await
            .map_err(|_| "the AI queue is shutting down".to_string())?;
        state
            .llm
            .chat(vec![ChatMessage {
                role: "user".to_string(),
                content: prompt,
            }])
            .await
            .map_err(|e| e.to_string())?
    };

    let reply = reply.trim();
    if reply.is_empty() {
        return Err("the model returned an empty reply — try again".to_string());
    }
    store::append_turn(vault_dir, slug, "assistant", reply).map_err(|e| e.to_string())?;
    reindex_logged(state);
    Ok(())
}
