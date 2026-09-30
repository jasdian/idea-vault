//! The make-skill button (docs/adr/0042, D42): R51 distils a discussion into a `skill_draft`
//! artifact as a background job (ADR-0010), and R52 saves the owner-reviewed draft into the
//! skill book, synchronously and with no model call (the plan-workbench precedent, ADR-0032).

use axum::extract::{Path, State};

use crate::ai::journal::RunKind;
use crate::concepts::make_skill;
use crate::domain::{Idea, IdeaState};
use crate::vault::store;
use crate::web::jobs;
use crate::web::routes::ideas::respond_discussion_or_stored;
use crate::web::routes::memory::progress_sink;
use crate::web::routes::{idea_llm, open_run, reindex_logged};
use crate::web::state::AppState;
use crate::web::WebError;

/// R51 — `POST /idea/{slug}/make-skill` — distil the discussion into a draft skill as a
/// background job. Returns the transcript with the thinking indicator, or on a Stored idea the
/// stored view with the same visible indicator (owner decision D1).
pub async fn make_skill(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> Result<axum::response::Response, WebError> {
    let idea = store::read_idea(&state.config.vault_dir, &slug)?; // 404 if missing
    let conversation = store::read_conversation(&state.config.vault_dir, &slug)?;
    guard_make_skill(&idea, &conversation)?;
    // A lost claim re-renders the in-progress view (HND-6).
    if jobs::try_claim(&state.jobs, &slug) {
        spawn_make_skill_job(&state, &slug);
    }
    respond_discussion_or_stored(&state, &slug)
}

/// R51's synchronous guards, shared with the MCP `make_skill` tool (HND-2, HND-10): any state
/// but Draft (a Stored idea is distilled without reopening it, D1), and a discussion with
/// something in it to distil, so a paid call never runs on an empty idea.
pub(crate) fn guard_make_skill(idea: &Idea, conversation: &str) -> Result<(), WebError> {
    match idea.frontmatter.state {
        IdeaState::Draft => Err(WebError::BadRequest(
            "idea is a draft — open the discussion with a first chat turn before making a skill"
                .into(),
        )),
        IdeaState::InDiscussion | IdeaState::Reopened | IdeaState::Stored => {
            make_skill::distillable(conversation).map_err(|e| WebError::BadRequest(e.to_string()))
        }
    }
}

/// Spawn R51's detached distil job on an already-claimed slot (ADR-0010), journaled as
/// `make-skill` (ADR-0037). The caller owns the claim, so the web route and the MCP tool can each
/// answer a lost claim their own way (HND-6).
pub(crate) fn spawn_make_skill_job(state: &AppState, slug: &str) {
    let ts = state.clone();
    let tslug = slug.to_string();
    let run = open_run(state, slug, RunKind::MakeSkill);
    let abort = jobs::spawn_job(&state.jobs, slug, run, async move {
        jobs::set_note(&ts.jobs, &tslug, "make skill · reading the discussion");
        match make_skill_work(&ts, &tslug).await {
            Ok(notice) => jobs::mark_notice(&ts.jobs, &tslug, notice),
            Err(m) => jobs::mark_failed(&ts.jobs, &tslug, m),
        }
    });
    jobs::set_abort(&state.jobs, slug, abort);
}

/// The background half of R51: the distil (which takes its own permit, ADR-0006) and the
/// log-not-fail reindex that makes the draft searchable. Returns the owner's one-shot notice.
/// Source-free (`idea_llm`): distilling reads the discussion, not the attached code.
pub(crate) async fn make_skill_work(state: &AppState, slug: &str) -> Result<String, String> {
    let progress = progress_sink(state, slug);
    let llm = idea_llm(state, slug);
    let outcome = make_skill::distill(
        &llm,
        &state.ai_semaphore,
        &state.config.vault_dir,
        slug,
        &state.skills.snapshot(),
        llm.context_budget(),
        &progress,
    )
    .await
    .map_err(|e| e.to_string())?;
    reindex_logged(state);
    Ok(draft_notice(&outcome))
}

/// The one-shot line a finished distil leaves: where the draft is, and (D3, warn only) how many
/// of its evidence quotes were not found in the discussion.
pub(crate) fn draft_notice(outcome: &make_skill::DistillOutcome) -> String {
    let mut notice = format!(
        "skill draft ready: {} — open {} under Artifacts to review and save it",
        outcome.name, outcome.artifact_slug
    );
    if outcome.ungrounded > 0 {
        notice.push_str(&format!(
            " ({} evidence quote{} not found in the discussion, marked ✗)",
            outcome.ungrounded,
            if outcome.ungrounded == 1 { "" } else { "s" }
        ));
    }
    notice
}
