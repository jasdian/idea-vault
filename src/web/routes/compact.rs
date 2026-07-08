//! Manual compaction route (auto-compact, docs/adr/0012): `POST /idea/{slug}/compact`.
//!
//! The owner-triggered sibling of the automatic phase-0 fold in `chat.rs`. It runs as its own
//! one-shot background job (claim-guarded like chat/skill/swarm, so it can never race the writer
//! of `compacted.md`), ignores the auto-compact toggle and the threshold, and folds at the forced
//! targets (zero tail target — everything except the final turn, ADR-0016). Refused on a `Stored`
//! idea (reopen first). Returns the transcript with the "thinking" indicator, which polls
//! `/pending` to completion; a genuine nothing-to-fold surfaces as a one-shot notice, never a
//! silent identical re-render.

use axum::extract::{Path, State};
use axum::response::Html;

use crate::app::AppState;
use crate::domain::IdeaState;
use crate::memory::compact;
use crate::vault::store;
use crate::web::jobs;
use crate::web::routes::ideas::respond_with_transcript;
use crate::web::WebError;

/// `POST /idea/{slug}/compact` — fold the conversation head now, on demand.
pub async fn compact(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> Result<Html<String>, WebError> {
    let idea = store::read_idea(&state.config.vault_dir, &slug)?; // 404 if missing
    if idea.frontmatter.state == IdeaState::Stored {
        return Err(WebError::BadRequest(
            "reopen the idea before compacting".into(),
        ));
    }

    // Busy already (a chat/skill/swarm/compact is running): just re-show the in-flight state — one
    // job per idea means there is never a second writer to compacted.md.
    if !jobs::try_claim(&state.jobs, &slug) {
        return respond_with_transcript(&state, &slug);
    }

    let ts = state.clone();
    let tslug = slug.clone();
    let handle = tokio::spawn(async move {
        jobs::set_note(&ts.jobs, &tslug, "compacting older turns…");
        // force = true: ignore the toggle/threshold and fold at the forced (zero-tail) targets.
        let r = compact::run_compaction(
            &ts.llm,
            &ts.ai_semaphore,
            &ts.config.vault_dir,
            &tslug,
            true,
        )
        .await;
        match r {
            // A real fold: the rewritten summary disclosure + the dropped meter are the feedback.
            Ok(compact::CompactOutcome::Folded) => jobs::mark_done(&ts.jobs, &tslug),
            // An honest no-op must still be visible — otherwise the button reads as broken.
            Ok(compact::CompactOutcome::NothingToFold) => jobs::mark_notice(
                &ts.jobs,
                &tslug,
                "nothing to fold — the conversation is a single turn or already compacted".into(),
            ),
            Err(m) => jobs::mark_failed(&ts.jobs, &tslug, m),
        }
    });
    jobs::set_abort(&state.jobs, &slug, handle.abort_handle());

    respond_with_transcript(&state, &slug)
}
