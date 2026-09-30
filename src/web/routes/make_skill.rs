//! The make-skill button (docs/adr/0042, D42): R51 distils a discussion into a `skill_draft`
//! artifact as a background job (ADR-0010), and R52 saves the owner-reviewed draft into the
//! skill book, synchronously and with no model call (the plan-workbench precedent, ADR-0032).

use askama::Template as _;
use axum::extract::{Path, State};
use axum::Form;

use crate::ai::journal::RunKind;
use crate::concepts::make_skill::{self, DiffLine, Draft, Placement, SaveRefusal};
use crate::concepts::skills::check_candidate;
use crate::domain::frontmatter::parse_skill;
use crate::domain::{ArtifactKind, Idea, IdeaState};
use crate::vault::store;
use crate::web::jobs;
use crate::web::routes::artifacts::split_artifact_name;
use crate::web::routes::ideas::respond_discussion_or_stored;
use crate::web::routes::memory::progress_sink;
use crate::web::routes::{idea_llm, open_run, reindex_logged};
use crate::web::state::AppState;
use crate::web::templates::{DiffRow, EvidenceRow, SkillDraftPanel};
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

/// Form body for R52: the (possibly edited) skill file, and the digest of the owner file the
/// panel showed when the draft is an update.
#[derive(Debug, serde::Deserialize)]
pub struct SaveSkillForm {
    pub raw: String,
    #[serde(default)]
    pub base_digest: String,
}

/// R52 — `POST /idea/{slug}/artifact/{name}/save-skill` — save a reviewed skill draft into the
/// skill book: revalidate the text by the loader's rules against the live registry, write
/// `vault/.skills/<name>.md` atomically, and reload the skills and workflows. Synchronous, no
/// model call and no job slot. The draft artifact is never modified. Grounding never refuses a
/// Save (owner decision D3).
pub async fn save_skill(
    State(state): State<AppState>,
    Path((slug, name)): Path<(String, String)>,
    Form(form): Form<SaveSkillForm>,
) -> Result<axum::response::Html<String>, WebError> {
    store::read_idea(&state.config.vault_dir, &slug)?; // 404 if missing
    let draft = read_draft(&state, &slug, &name)?;
    // A browser submits a textarea with CRLF line ends; the file and its digest use LF.
    let raw = form.raw.replace("\r\n", "\n");
    let base = Some(form.base_digest.as_str()).filter(|d| !d.is_empty());
    let registry = state.skills.snapshot();
    match make_skill::save_check(&raw, &registry, state.skills.dir(), base) {
        Ok((_, skill)) => {
            store::write_owner_skill(state.skills.dir(), &skill.name, &raw)?;
            state.workflows.reload(&state.skills);
            let panel =
                skill_draft_panel(&state, &slug, &name, &draft, &raw, None, Some(skill.name))?;
            Ok(axum::response::Html(panel))
        }
        Err(refusal @ SaveRefusal::Superseded) => Err(WebError::Conflict(refusal.to_string())),
        Err(
            refusal @ (SaveRefusal::Invalid(_)
            | SaveRefusal::BuiltInName(_)
            | SaveRefusal::InternalName(_)),
        ) => Err(WebError::Unprocessable(skill_draft_panel(
            &state,
            &slug,
            &name,
            &draft,
            &raw,
            Some(refusal.to_string()),
            None,
        )?)),
    }
}

/// The skill draft artifact `name` (`<stem>.md`) of idea `slug`, parsed; a missing file, any
/// other kind, or an unreadable body is a 404.
fn read_draft(state: &AppState, slug: &str, name: &str) -> Result<Draft, WebError> {
    let not_found = || WebError::NotFound(format!("skill draft: {name}"));
    let (stem, ext) = split_artifact_name(name).ok_or_else(not_found)?;
    if ext != store::ArtifactExt::Md {
        return Err(not_found());
    }
    let artifact = store::read_artifact(&state.config.vault_dir, slug, stem)?;
    if artifact.frontmatter.kind != ArtifactKind::SkillDraft {
        return Err(not_found());
    }
    make_skill::parse_draft_body(&artifact.body).ok_or_else(not_found)
}

/// Render the review panel for `draft` with `text` in the textarea (the draft itself, or the
/// owner's edit): where the named skill would land, the diff against an owner file it would
/// update, similar skills, and the evidence marks. Shared by R19 and R52.
pub(crate) fn skill_draft_panel(
    state: &AppState,
    slug: &str,
    file_name: &str,
    draft: &Draft,
    text: &str,
    message: Option<String>,
    saved: Option<String>,
) -> Result<String, WebError> {
    let registry = state.skills.snapshot();
    let name = parse_skill(text).ok().map(|(fm, _)| fm.name);
    let placement = name
        .as_deref()
        .map(|n| make_skill::placement(n, &registry, state.skills.dir()));
    let (placement_kind, placement_line, base_digest, diff) = match (&name, placement) {
        (Some(n), Some(Placement::Add)) => (
            "add",
            format!("ADD {n} — a new skill in your skill book"),
            None,
            Vec::new(),
        ),
        (
            Some(n),
            Some(Placement::Update {
                current_raw,
                current_digest,
            }),
        ) => {
            let diff = make_skill::line_diff(&current_raw, text);
            let changed = diff.iter().any(|d| !matches!(d, DiffLine::Same(_)));
            (
                "update",
                if changed {
                    format!("UPDATE {n} — replaces your skill file; the diff shows what changes")
                } else {
                    format!("UPDATE {n} — your skill file already reads exactly this")
                },
                Some(current_digest),
                if changed { diff_rows(diff) } else { Vec::new() },
            )
        }
        (_, Some(Placement::Rename { reason })) => (
            "rename",
            format!("RENAME REQUIRED — {reason}: edit the name: line"),
            None,
            Vec::new(),
        ),
        _ => (
            "unknown",
            "this text does not parse as a skill file yet".to_string(),
            None,
            Vec::new(),
        ),
    };
    let similar = check_candidate(text)
        .map(|candidate| {
            make_skill::similar(&candidate, &registry)
                .into_iter()
                .map(|s| {
                    format!(
                        "{} ({}.{:02})",
                        s.name,
                        s.score_milli / 1000,
                        s.score_milli % 1000 / 10
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let evidence: Vec<EvidenceRow> = draft
        .evidence
        .iter()
        .map(|e| match (e.grounded, e.owner) {
            (true, true) => EvidenceRow {
                mark: "✓ owner",
                kind: "owner",
                quote: e.quote.clone(),
            },
            (true, false) => EvidenceRow {
                mark: "✓",
                kind: "grounded",
                quote: e.quote.clone(),
            },
            (false, _) => EvidenceRow {
                mark: "✗",
                kind: "ungrounded",
                quote: e.quote.clone(),
            },
        })
        .collect();
    SkillDraftPanel {
        slug: slug.to_string(),
        file_name: file_name.to_string(),
        raw: text.to_string(),
        placement_kind,
        placement_line,
        base_digest,
        diff,
        similar,
        ungrounded: draft.evidence.iter().filter(|e| !e.grounded).count(),
        evidence,
        message,
        saved,
    }
    .render()
    .map_err(|e| WebError::Internal(format!("template render: {e}")))
}

fn diff_rows(diff: Vec<DiffLine>) -> Vec<DiffRow> {
    diff.into_iter()
        .map(|d| match d {
            DiffLine::Same(text) => DiffRow {
                kind: "same",
                sign: " ",
                text,
            },
            DiffLine::Added(text) => DiffRow {
                kind: "added",
                sign: "+",
                text,
            },
            DiffLine::Removed(text) => DiffRow {
                kind: "removed",
                sign: "-",
                text,
            },
        })
        .collect()
}
