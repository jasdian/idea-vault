//! Chat route (docs/09-web-ui.md D17 R9): one discussion turn, run as a background job.
//!
//! The model call is a *detached* task, not the request future — so navigating away (or a dropped
//! connection) can't cancel the generation, and the reply lands in `conversation.md` regardless of
//! who is watching (`web::jobs`). The user turn is persisted up front so it survives navigation and
//! shows beneath the "thinking" indicator; the assistant turn is appended when the task completes.
//!
//! One job per idea, but a Send while busy is **not** dropped: it joins a per-idea FIFO
//! (`web::jobs` queue) and returns `202 Accepted`. The poll loop drains the next queued message
//! whenever the idea goes idle (`start_next_queued`), and the owner can remove any queued message
//! before it runs (`remove_queued`).

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::Form;
use chrono::Utc;
use serde::Deserialize;

use crate::ai::ollama::ChatMessage;
use crate::domain::{Idea, IdeaState};
use crate::memory;
use crate::vault::store;
use crate::web::jobs;
use crate::web::routes::ideas::{render_queue_panel, respond_with_transcript};
use crate::web::routes::{reindex_logged, related_block_logged, scoped_llm};
use crate::web::state::AppState;
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

/// R9 — `POST /idea/{slug}/chat` — start one discussion turn, or queue it if the idea is busy.
///
/// Idle idea → claim the slot, persist the turn, spawn the job, return the transcript with the
/// "thinking" indicator (`200`). Busy idea → append the message to the per-idea FIFO and return
/// `202 Accepted` with the queue panel refreshed. Either way the message is accepted (never
/// dropped), so the composer clears on any 2xx.
pub async fn chat(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    Form(form): Form<ChatForm>,
) -> Result<Response, WebError> {
    let message = form.message.trim().to_string();
    if message.is_empty() {
        return Err(WebError::BadRequest("message must not be empty".into()));
    }

    let vault_dir = state.config.vault_dir.clone();
    let idea = store::read_idea(&vault_dir, &slug)?; // 404 if missing
    if idea.frontmatter.state == IdeaState::Stored {
        return Err(WebError::BadRequest(
            "idea is stored — reopen it before chatting".into(),
        ));
    }

    // Idle: start now. `try_claim_idle` refuses a Running/Failed/Notice slot, so a message sent
    // while a job is in flight (or a still-unshown outcome sits in the slot) falls through to the
    // queue instead of racing the drainer.
    if jobs::try_claim_idle(&state.jobs, &slug) {
        spawn_chat_turn(&state, &slug, idea, &message)?;
        return Ok(respond_with_transcript(&state, &slug)?.into_response());
    }

    // Busy: queue the message rather than forcing a wait or dropping it. The running job's poll
    // will drain it (`start_next_queued`) when the idea goes idle.
    match jobs::enqueue(&state.queues, &slug, &message) {
        Some(_id) => {
            let body = respond_with_transcript(&state, &slug)?;
            // 202 Accepted: queued, not yet sent. (StatusCode first overrides the inner 200.)
            Ok((StatusCode::ACCEPTED, body).into_response())
        }
        None => Err(WebError::BadRequest(format!(
            "the queue is full ({} pending) — let some send before adding more",
            jobs::MAX_QUEUED
        ))),
    }
}

/// Persist a user turn on an **already-claimed** slot and spawn the detached reply job. Shared by
/// the direct send path, the queue drainer, and the inbound MCP `chat` tool
/// (`web::mcp_server::tasks`, ADR-0024). On a persist failure it releases the slot (so the idea
/// isn't wedged busy) and returns the error.
pub(crate) fn spawn_chat_turn(
    state: &AppState,
    slug: &str,
    mut idea: Idea,
    message: &str,
) -> Result<(), WebError> {
    let vault_dir = state.config.vault_dir.clone();
    // Persist the user turn now (survives navigation, shows under the indicator) and make the D9
    // Draft→InDiscussion transition. If this fails, release the slot so the idea isn't stuck busy.
    if let Err(e) = store::append_turn(&vault_dir, slug, "user", message) {
        jobs::mark_done(&state.jobs, slug);
        return Err(e.into());
    }
    if idea.frontmatter.state == IdeaState::Draft {
        idea.frontmatter.state = IdeaState::InDiscussion;
        idea.frontmatter.updated = Utc::now();
        let _ = store::write_idea(&vault_dir, &idea);
    }
    reindex_logged(state);

    // Detached: the reply outlives this request.
    let task_state = state.clone();
    let task_slug = slug.to_string();
    let abort = jobs::spawn_job(&state.jobs, slug, async move {
        // Phase 0: pre-emptive, best-effort compaction (auto-compact, docs/adr/0012). It runs
        // BEFORE the reply so the very turn that tripped the threshold is answered off the freshly
        // compacted context — but a compaction failure NEVER fails the turn: it is logged and the
        // reply proceeds with the fallback (uncompacted) context. The note shows this phase; a
        // no-op compaction (threshold not met) just flips straight to "thinking".
        jobs::set_note(&task_state.jobs, &task_slug, "compacting older turns…");
        if let Err(e) = memory::compact::maybe_run_compaction(
            &crate::web::routes::idea_llm(&task_state, &task_slug),
            &task_state.ai_semaphore,
            &task_state.config.vault_dir,
            &task_slug,
        )
        .await
        {
            tracing::warn!(slug = %task_slug, "compaction skipped (fallback context): {e}");
        }
        // Phase 1: the reply, off the (possibly) freshly-compacted context.
        jobs::set_note(&task_state.jobs, &task_slug, "thinking");
        match run_chat(&task_state, &task_slug).await {
            Ok(()) => jobs::mark_done(&task_state.jobs, &task_slug),
            Err(msg) => jobs::mark_failed(&task_state.jobs, &task_slug, msg),
        }
    });
    jobs::set_abort(&state.jobs, slug, abort);
    Ok(())
}

/// Drain the next queued message if the idea is idle. The poll loop calls this: when a running job
/// finishes (or is cancelled) and a message is waiting, it claims the freed slot and starts the
/// turn, so the next `/pending` render carries a fresh "thinking" indicator and the queue advances
/// one message per completion. Returns `true` if a turn was started.
///
/// `try_claim_idle` is the concurrency gate: only one racing poll wins the slot, and a still-unshown
/// `Failed`/`Notice` outcome blocks the claim so an error is seen before the queue rolls on.
pub(crate) fn start_next_queued(state: &AppState, slug: &str) -> bool {
    if !jobs::try_claim_idle(&state.jobs, slug) {
        return false;
    }
    let Some(msg) = jobs::dequeue(&state.queues, slug) else {
        // Nothing queued — release the slot we just took so it stays honestly idle.
        jobs::mark_done(&state.jobs, slug);
        return false;
    };
    // The idea may have been stored/deleted between enqueue and now — drop the stale message.
    let idea = match store::read_idea(&state.config.vault_dir, slug) {
        Ok(idea) if idea.frontmatter.state != IdeaState::Stored => idea,
        _ => {
            jobs::mark_done(&state.jobs, slug);
            return false;
        }
    };
    // A persist failure already released the slot inside spawn_chat_turn.
    spawn_chat_turn(state, slug, idea, &msg.text).is_ok()
}

/// `POST /idea/{slug}/queue/{id}/delete` — remove one still-pending message before it runs. Returns
/// the refreshed queue panel (the remove form swaps `#queue`).
pub async fn remove_queued(
    State(state): State<AppState>,
    Path((slug, id)): Path<(String, u64)>,
) -> Result<Html<String>, WebError> {
    store::read_idea(&state.config.vault_dir, &slug)?; // 404 if the idea is gone
    jobs::remove_queued(&state.queues, &slug, id);
    let items = jobs::list_queued(&state.queues, &slug);
    Ok(Html(render_queue_panel(&slug, items, false)?))
}

/// Byte cap on the skill book carried in every chat prompt.
const SKILL_BOOK_BYTES: usize = 1024;

fn compose_prompt(book: &str, related: &str, own: &str) -> String {
    format!("{FOIL_INSTRUCTION}\n\n{book}\n{related}{own}")
}

/// The background half: assemble the budgeted context (which already includes the just-persisted
/// user turn), call the model under the shared semaphore, and append the assistant turn. Returns a
/// human-readable message on failure for the indicator to surface.
async fn run_chat(state: &AppState, slug: &str) -> Result<(), String> {
    let vault_dir = &state.config.vault_dir;
    // One scoped view for the whole turn (ADR-0021): the budget the context is assembled
    // against and the backend that answers must agree on the sources riding the window.
    let llm = scoped_llm(state, slug);
    // The skill book rides along (ADR-0022) so the foil can recommend a move by name; its bytes
    // come out of the context budget rather than on top of it.
    let book = crate::concepts::coverage::skill_book(&state.skills.snapshot(), SKILL_BOOK_BYTES);
    let budget = crate::ai::budget::ContextBudget::new(
        llm.context_budget().max_bytes.saturating_sub(book.len()),
    );
    let context = memory::load::load_context(vault_dir, slug, budget).map_err(|e| e.to_string())?;
    let allowance = crate::ai::budget::related_allowance(budget, context.text.len());
    let related = related_block_logged(state, slug, allowance);
    let prompt = compose_prompt(&book, &related, &context.text);

    let reply = {
        let _permit = state
            .ai_semaphore
            .acquire()
            .await
            .map_err(|_| "the AI queue is shutting down".to_string())?;
        llm.chat(vec![ChatMessage {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::budget::{related_allowance, ContextBudget};
    use crate::domain::IdeaFrontmatter;
    use chrono::TimeZone;

    fn write_idea(vault: &std::path::Path, slug: &str, title: &str, body: &str) {
        let at = Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap();
        store::write_idea(
            vault,
            &Idea {
                frontmatter: IdeaFrontmatter {
                    title: title.into(),
                    slug: slug.into(),
                    state: IdeaState::InDiscussion,
                    tags: vec![],
                    sources: vec![],
                    created: at,
                    updated: at,
                },
                body: body.into(),
            },
        )
        .unwrap();
    }

    #[test]
    fn own_context_byte_identical_with_related() {
        let tmp = tempfile::tempdir().unwrap();
        write_idea(tmp.path(), "alpha", "Own", "Alpha links [[beta]].\n");
        write_idea(tmp.path(), "beta", "Neighbour", "Beta stands alone.\n");
        store::append_turn(tmp.path(), "alpha", "user", "push it").unwrap();
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::index::schema::apply_schema(&conn).unwrap();
        crate::index::reindex::reindex(&mut conn, tmp.path()).unwrap();

        let book = "Moves the owner can run:\n- premortem — imagine it failed\n";
        let budget = ContextBudget::new(16_000);
        let own = memory::load::load_context(tmp.path(), "alpha", budget).unwrap();
        let allowance = related_allowance(budget, own.text.len());
        let related = memory::related::related_block(&conn, "alpha", allowance).unwrap();
        assert!(!related.is_empty(), "fixture has a related neighbour");

        let with = compose_prompt(book, &related, &own.text);
        assert!(with.contains(&own.text));
        assert!(with.find(&related).unwrap() < with.find(&own.text).unwrap());

        conn.execute("DELETE FROM edges", []).unwrap();
        let none = memory::related::related_block(&conn, "alpha", allowance).unwrap();
        let without = compose_prompt(book, &none, &own.text);
        assert_eq!(
            without,
            format!("{FOIL_INSTRUCTION}\n\n{book}\n{}", own.text)
        );
        assert_eq!(with.replacen(&related, "", 1), without);
    }
}
