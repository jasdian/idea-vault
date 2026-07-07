//! Manual compaction route (auto-compact, docs/adr/0012): `POST /idea/{slug}/compact`.
//!
//! The owner-triggered sibling of the automatic phase-0 fold in `chat.rs`. It runs as its own
//! one-shot background job (claim-guarded like chat/skill/swarm, so it can never race the writer
//! of `compacted.md`), ignores the auto-compact toggle and the threshold (always folds toward the
//! tail target), and is refused on a `Stored` idea (reopen first). Returns the transcript with the
//! "thinking" indicator, which polls `/pending` to completion.

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
    tokio::spawn(async move {
        // force = true: ignore the toggle/threshold, always fold toward the tail target.
        let r = compact::run_compaction(
            &ts.llm,
            &ts.ai_semaphore,
            &ts.config.vault_dir,
            &tslug,
            true,
        )
        .await;
        match r {
            Ok(()) => jobs::mark_done(&ts.jobs, &tslug),
            Err(m) => jobs::mark_failed(&ts.jobs, &tslug, m),
        }
    });

    respond_with_transcript(&state, &slug)
}
