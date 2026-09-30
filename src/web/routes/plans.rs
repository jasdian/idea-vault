//! The plan workbench route group (docs/adr/0032, docs/09-web-ui.md R46–R48): answer a build
//! plan's open questions and owner-held tasks into its next version (R46), jump to the lineage
//! head (R47), and re-plan with the model around the answers so far (R48).
//!
//! Answering is deterministic: no model call, no job slot, no semaphore permit — like rename
//! (R23) and tags (R42) it writes synchronously, refused while a model job runs so its turns can
//! never interleave with that job's write-back. Re-planning is an AI turn, so it keeps the
//! claim → spawn → poll shape of every other model call (ADR-0010).

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Form;
use chrono::Utc;

use crate::ai::journal::RunKind;
use crate::concepts::build_plan::lineage;
use crate::concepts::build_plan::workbench::{
    self, AnswerChannel, AnswerRequest, PlanView, WorkbenchError,
};
use crate::concepts::workflows::READY_TO_BUILD;
use crate::domain::{slug as domain_slug, IdeaState};
use crate::vault::store;
use crate::web::jobs;
use crate::web::routes::memory::{
    guard_discussion_state, guard_skill, guard_workflow, spawn_skill_job, spawn_workflow_job,
};
use crate::web::routes::{reindex_logged, scoped_llm};
use crate::web::state::AppState;
use crate::web::templates::{LineageStep, PlanBlock, PlanQuestion, PlanWorkView};
use crate::web::WebError;

/// The capstone skill a quick re-plan runs.
const QUICK_PLANNER: &str = "build-prompt";

/// Why R46 refuses while a job runs: the job appends its own turns when it lands, and the answer
/// turns must not interleave with them.
const BUSY: &str = "the foil is thinking — answer when it finishes";

/// Map a workbench refusal onto the web statuses (shared with the MCP `answer_plan` tool's
/// callers that need a `WebError`): a superseded base is a retryable conflict naming the head,
/// a missing plan a 404, and every answer-validation refusal the owner's input to fix.
pub(crate) fn workbench_error(e: WorkbenchError) -> WebError {
    match e {
        WorkbenchError::Superseded { .. } => WebError::Conflict(e.to_string()),
        WorkbenchError::NotFound(_) | WorkbenchError::NotAPlan(_) => {
            WebError::NotFound(e.to_string())
        }
        WorkbenchError::UnknownId(_)
        | WorkbenchError::NotAnswerable(_)
        | WorkbenchError::TooShort(_)
        | WorkbenchError::TooLong(_)
        | WorkbenchError::NotOwnWords(_)
        | WorkbenchError::DuplicateId(_)
        | WorkbenchError::NothingToAnswer => WebError::BadRequest(e.to_string()),
        WorkbenchError::Concept(c) => WebError::Concept(c),
    }
}

/// The id a validation refusal is about, to sit the message beside that field; `None` for a
/// refusal of the whole submission, or one that is not about the owner's input.
fn refused_field(e: &WorkbenchError) -> Option<&str> {
    match e {
        WorkbenchError::UnknownId(id)
        | WorkbenchError::NotAnswerable(id)
        | WorkbenchError::TooShort(id)
        | WorkbenchError::TooLong(id)
        | WorkbenchError::NotOwnWords(id)
        | WorkbenchError::DuplicateId(id) => Some(id),
        WorkbenchError::NothingToAnswer
        | WorkbenchError::NotAPlan(_)
        | WorkbenchError::NotFound(_)
        | WorkbenchError::Superseded { .. }
        | WorkbenchError::Concept(_) => None,
    }
}

/// Whether a refusal is the owner's input to fix (R46 answers it with the re-rendered form).
fn is_validation(e: &WorkbenchError) -> bool {
    matches!(e, WorkbenchError::NothingToAnswer) || refused_field(e).is_some()
}

/// The workbench partial's view of one plan version. `submitted` refills the fields after a
/// refused submission and `refusal` sits its message beside the field it names; the hedge hint
/// reads whatever answer a field holds, so a hedged answer G2 re-opened warns on the next view.
pub(crate) fn plan_work_view(
    view: PlanView,
    slug: &str,
    state: IdeaState,
    submitted: &[(String, String)],
    refusal: Option<&WorkbenchError>,
) -> PlanWorkView {
    let field_error = |id: &str| {
        refusal
            .filter(|e| refused_field(e) == Some(id))
            .map(ToString::to_string)
    };
    let submitted_value = |id: &str| {
        submitted
            .iter()
            .find(|(k, _)| k == id)
            .map(|(_, v)| v.clone())
    };
    let hedge = |value: &str| {
        (!value.trim().is_empty())
            .then(|| workbench::hedge_warning(value))
            .flatten()
    };
    let open: Vec<PlanQuestion> = view
        .open
        .into_iter()
        .map(|q| {
            // A hedged owner answer G2 re-opened still carries the owner's words: offer them back.
            let carried = view
                .plan
                .open
                .iter()
                .find(|item| item.id == q.id && item.field("answers").is_some())
                .and_then(|item| item.field("quote"))
                .map(str::to_string);
            let value = submitted_value(&q.id).or(carried).unwrap_or_default();
            PlanQuestion {
                error: field_error(&q.id),
                hedge: hedge(&value),
                id: q.id,
                text: q.text,
                markers: q.markers,
                blocks: q.blocks,
                value,
            }
        })
        .collect();
    let blocked = view
        .blocked
        .into_iter()
        .map(|t| {
            let value = submitted_value(&t.id).unwrap_or_default();
            PlanBlock {
                error: field_error(&t.id),
                hedge: hedge(&value),
                id: t.id,
                text: t.text,
                blocked_by: t.blocked_by,
                reasons: t.reasons,
                answerable: t.answerable,
                value,
            }
        })
        .collect::<Vec<_>>();
    let placed = refusal
        .and_then(refused_field)
        .is_some_and(|id| open.iter().any(|q| q.id == id) || blocked.iter().any(|t| t.id == id));
    PlanWorkView {
        slug: slug.to_string(),
        stem: view.stem,
        version: view.version,
        revises: view.revises,
        answered: view.answered,
        superseded_by: view.superseded_by,
        head: view.head,
        is_head: view.is_head,
        lineage: view
            .lineage
            .into_iter()
            .map(|(stem, version)| LineageStep { stem, version })
            .collect(),
        mode: view.header.mode,
        open,
        blocked,
        can_answer: matches!(state, IdeaState::InDiscussion | IdeaState::Reopened),
        form_error: refusal.filter(|_| !placed).map(ToString::to_string),
    }
}

/// `Q6`/`T4`-shaped: the only form keys R46 reads.
fn is_answer_key(key: &str) -> bool {
    let mut chars = key.chars();
    matches!(chars.next(), Some('Q' | 'T'))
        && !chars.as_str().is_empty()
        && chars.all(|c| c.is_ascii_digit())
}

/// The submitted answers in id order (questions, then tasks, each by number); empty fields are
/// the ones the owner left for later.
fn answers_in_id_order(form: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut answers: Vec<(String, String)> = form
        .into_iter()
        .filter(|(k, v)| is_answer_key(k) && !v.trim().is_empty())
        .collect();
    answers.sort_by_key(|(k, _)| {
        let (letter, number) = k.split_at(1);
        (
            letter.to_string(),
            number.parse::<u32>().unwrap_or(u32::MAX),
        )
    });
    answers
}

/// A plan stem from the URL, checked before any path join (PFC-3).
fn plan_stem(stem: &str) -> Result<&str, WebError> {
    if domain_slug::is_valid(stem) {
        Ok(stem)
    } else {
        Err(WebError::NotFound(format!("build plan {stem}")))
    }
}

/// R46 — `POST /idea/{slug}/plan/{stem}/answer` — answer open questions and owner-held tasks on
/// the plan `stem`, making its next version (docs/adr/0032). Each answer lands as the owner's
/// own turn; the new version re-runs the gates without a model call. Success is an `HX-Redirect`
/// to the new version; refused answers come back as a 422 re-render of the workbench.
pub async fn answer_plan(
    State(state): State<AppState>,
    Path((slug, stem)): Path<(String, String)>,
    Form(form): Form<Vec<(String, String)>>,
) -> Result<Response, WebError> {
    let vault_dir = state.config.vault_dir.clone();
    let idea = store::read_idea(&vault_dir, &slug)?; // 404 if missing
    let stem = plan_stem(&stem)?.to_string();
    guard_discussion_state(idea.frontmatter.state)?;
    if jobs::is_running(&state.jobs, &slug) {
        return Err(WebError::Conflict(BUSY.to_string()));
    }

    let answers = answers_in_id_order(form);
    let probe = scoped_llm(&state, &slug).source_probe();
    let (tslug, tstem, tanswers) = (slug.clone(), stem.clone(), answers.clone());
    let tvault = vault_dir.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        workbench::answer(AnswerRequest {
            vault_dir: &tvault,
            idea_slug: &tslug,
            base: &tstem,
            answers: &tanswers,
            probe: &probe,
            now: Utc::now(),
            via: AnswerChannel::Web,
        })
    })
    .await
    .map_err(|e| WebError::Internal(format!("answer task: {e}")))?;

    match outcome {
        Ok(version) => {
            if !version.reused {
                reindex_logged(&state);
            }
            Ok((
                [(
                    "HX-Redirect",
                    format!("/idea/{slug}/artifact/{}.md#work", version.stem),
                )],
                StatusCode::OK,
            )
                .into_response())
        }
        Err(e) if is_validation(&e) => {
            use askama::Template as _;
            let view =
                workbench::plan_view(&vault_dir, &slug, Some(&stem)).map_err(workbench_error)?;
            let html = plan_work_view(view, &slug, idea.frontmatter.state, &answers, Some(&e))
                .render()
                .map_err(|e| WebError::Internal(format!("template render: {e}")))?;
            Err(WebError::Unprocessable(html))
        }
        Err(e) => Err(workbench_error(e)),
    }
}

/// R47 — `GET /idea/{slug}/plan/latest` — the lineage head's workbench: a 302 to its artifact
/// page, so the idea page's plan chip always lands on the version that takes answers.
pub async fn latest_plan(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> Result<Response, WebError> {
    let vault_dir = &state.config.vault_dir;
    store::read_idea(vault_dir, &slug)?; // 404 if missing
    let plans = lineage::list_plans(vault_dir, &slug)?;
    let head =
        lineage::head(&plans).ok_or_else(|| WebError::NotFound("no build plan yet".to_string()))?;
    Ok((
        StatusCode::FOUND,
        [(
            axum::http::header::LOCATION,
            format!("/idea/{slug}/artifact/{}.md#work", head.stem),
        )],
    )
        .into_response())
}

/// Form body for R48: `audited=1` runs the audited ready-to-build workflow, anything else the
/// one-call quick planner.
#[derive(Debug, serde::Deserialize)]
pub struct ReplanForm {
    #[serde(default)]
    pub audited: String,
}

/// R48 — `POST /idea/{slug}/plan/{stem}/replan` — re-plan with the model as a background job
/// (ADR-0010). The run joins the lineage in `finish` (docs/adr/0032): it revises the head and
/// carries every owner answer so far, so an answered question is not asked again. Redirects to
/// the idea page, where the thinking poll takes over; a lost claim redirects the same way, onto
/// the job already running.
pub async fn replan(
    State(state): State<AppState>,
    Path((slug, stem)): Path<(String, String)>,
    Form(form): Form<ReplanForm>,
) -> Result<Response, WebError> {
    let vault_dir = &state.config.vault_dir;
    let idea = store::read_idea(vault_dir, &slug)?; // 404 if missing
    let stem = plan_stem(&stem)?;
    if !lineage::list_plans(vault_dir, &slug)?
        .iter()
        .any(|p| p.stem == stem)
    {
        return Err(WebError::NotFound(format!("build plan {stem}")));
    }
    let audited = form.audited == "1";
    if audited {
        let book = guard_workflow(&state, &idea, READY_TO_BUILD)?;
        if jobs::try_claim(&state.jobs, &slug) {
            spawn_workflow_job(
                &state,
                &slug,
                READY_TO_BUILD.to_string(),
                book,
                RunKind::Replan,
            );
        }
    } else {
        let skill = guard_skill(&state, &idea, QUICK_PLANNER)?;
        if jobs::try_claim(&state.jobs, &slug) {
            spawn_skill_job(&state, &slug, skill, RunKind::Replan);
        }
    }
    Ok(([("HX-Redirect", format!("/idea/{slug}"))], StatusCode::OK).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answer_keys_are_q_or_t_numbers_only() {
        assert!(is_answer_key("Q6") && is_answer_key("T12"));
        assert!(!is_answer_key("Q") && !is_answer_key("S1") && !is_answer_key("Q6x"));
    }

    #[test]
    fn answers_sort_by_letter_then_number_and_drop_empties() {
        let form = vec![
            ("T4".to_string(), "pick the paper account".to_string()),
            ("Q10".to_string(), "ten".to_string()),
            ("Q2".to_string(), "two".to_string()),
            ("Q3".to_string(), "   ".to_string()),
            ("audited".to_string(), "1".to_string()),
        ];
        let ids: Vec<String> = answers_in_id_order(form)
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(ids, ["Q2", "Q10", "T4"]);
    }
}
