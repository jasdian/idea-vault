//! Chat route (docs/09-web-ui.md D17 R9): the discussion turn that streams AI tokens over SSE
//! (docs/adr/0004, D11). Chat shares `web::sse` and the process-wide AI semaphore with swarm.

use axum::extract::{Path, State};
use axum::response::sse::Event;
use axum::Form;
use chrono::Utc;
use serde::Deserialize;

use crate::ai::budget::ContextBudget;
use crate::ai::ollama::ChatMessage;
use crate::app::AppState;
use crate::domain::IdeaState;
use crate::index::reindex;
use crate::memory;
use crate::vault::store;
use crate::web::sse::{pump_tokens, sse_response, EventSender};
use crate::web::WebError;

/// The rigorous-foil persona for free chat (CLAUDE.md: steelman, then stress-test).
const FOIL_INSTRUCTION: &str = "You are a rigorous ideation foil. Engage with the idea below: \
steelman the owner's latest point first, then stress-test it from the angle they are not \
looking at. Be concrete and brief.";

/// Byte budget for one chat prompt (D21). Sized for small local models; the idea body always
/// survives, memory and older turns trim first.
const CHAT_BUDGET_BYTES: usize = 16 * 1024;

#[derive(Debug, Deserialize)]
pub struct ChatForm {
    #[serde(default)]
    pub message: String,
}

/// Rebuild the index, logging instead of failing the request — truth already landed and the
/// next reindex reconciles (docs/03 "Consistency & failure model").
fn reindex_logged(state: &AppState) {
    match state.db.lock() {
        Ok(mut conn) => {
            if let Err(e) = reindex::reindex(&mut conn, &state.config.vault_dir) {
                tracing::warn!(error = %e, "reindex after chat write failed; truth intact");
            }
        }
        Err(e) => tracing::warn!(error = %e, "db mutex poisoned; skipping reindex"),
    }
}

/// R9 — `POST /idea/{slug}/chat` — one discussion turn, streamed token-by-token over SSE (D11).
///
/// Persist boundaries: the user turn is appended (and a Draft transitions to InDiscussion)
/// BEFORE the stream opens; the assistant turn is appended only AFTER the model stream
/// completes — an aborted/timed-out/disconnected stream persists nothing of the reply.
pub async fn chat(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    Form(form): Form<ChatForm>,
) -> Result<axum::response::Response, WebError> {
    use axum::response::IntoResponse;

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

    // Truth first, before any streaming (D11): user turn (heading-escaped), then the D9
    // transition — the first turn moves Draft→InDiscussion; Reopened/InDiscussion stay put.
    store::append_turn(&vault_dir, &slug, "user", &message)?;
    if idea.frontmatter.state == IdeaState::Draft {
        idea.frontmatter.state = IdeaState::InDiscussion;
        idea.frontmatter.updated = Utc::now();
        store::write_idea(&vault_dir, &idea)?;
    }
    reindex_logged(&state);

    // Budgeted context (D21) — includes the just-appended user turn as the newest turn.
    let context =
        memory::load::load_context(&vault_dir, &slug, ContextBudget::new(CHAT_BUDGET_BYTES))?;
    let messages = vec![ChatMessage {
        role: "user".to_string(),
        content: format!("{FOIL_INSTRUCTION}\n\n{}", context.text),
    }];

    // Stream: headers go out immediately; the spawned task acquires the shared semaphore
    // (ADR-0006), pumps tokens, and persists the assistant turn only on completion.
    let (tx, rx): (EventSender, _) = tokio::sync::mpsc::channel(32);
    let ollama = state.ollama.clone();
    let semaphore = state.ai_semaphore.clone();
    let task_state = state.clone();
    let task_slug = slug.clone();

    tokio::spawn(async move {
        // The permit covers exactly the model call (ADR-0006); it is released before any
        // persistence/reindex work so a queued request never waits on unrelated disk latency.
        let pump_result = {
            let Ok(_permit) = semaphore.acquire().await else {
                tracing::warn!("ai semaphore closed during chat");
                return;
            };
            match ollama.chat_stream(messages).await {
                Ok(tokens) => pump_tokens(tokens, &tx).await,
                Err(e) => {
                    tracing::warn!(error = %e, slug = %task_slug, "chat stream failed to open");
                    let event = Event::default()
                        .event("error")
                        .data("the model is unavailable; nothing was saved");
                    let _ = tx.send(Ok(event)).await;
                    return;
                }
            }
        };

        match pump_result {
            Ok(Some(full)) => {
                let reply = full.trim();
                if reply.is_empty() {
                    tracing::warn!(slug = %task_slug, "model returned empty chat reply");
                } else if let Err(e) =
                    store::append_turn(&vault_dir, &task_slug, "assistant", reply)
                {
                    tracing::error!(error = %e, slug = %task_slug, "failed to persist assistant turn");
                    let event = Event::default()
                        .event("error")
                        .data("failed to save the reply");
                    let _ = tx.send(Ok(event)).await;
                    return;
                }
                reindex_logged(&task_state);
                let _ = tx
                    .send(Ok(Event::default().event("done").data("done")))
                    .await;
            }
            Ok(None) => {
                // Client disconnected: model call aborted by drop, nothing persisted (D11).
                tracing::debug!(slug = %task_slug, "client disconnected mid-stream");
            }
            Err(e) => {
                // pump already emitted the error event; the partial reply is discarded.
                tracing::warn!(error = %e, slug = %task_slug, "chat stream aborted mid-reply");
            }
        }
    });

    Ok(sse_response(rx).into_response())
}
