//! Idea lifecycle + concept actions (docs/09-web-ui.md D17): Store (R4), Reopen (R5), run a skill
//! (R6), and run a swarm (R7). These drive the state machine (docs/04-state-machine.md D9) and the
//! harness concepts (docs/06-concepts). Grouped here per D17's `memory`/idea-actions bucket.

use axum::extract::{Path, State};
use chrono::Utc;

use crate::app::AppState;
use crate::concepts;
use crate::domain::{Idea, IdeaState};
use crate::memory;
use crate::vault::store;
use crate::web::jobs;
use crate::web::routes::ideas::{build_discussion, respond_with_transcript, state_badge_oob};
use crate::web::routes::{reindex_logged, related_block_logged, scoped_llm};

use crate::web::WebError;
use askama::Template as _;

/// R4 — `POST /idea/{slug}/store` — consolidate + extract memory as a background job (D12),
/// transitioning to `Stored` when it lands.
///
/// Guards (D9) stay synchronous: only `InDiscussion`/`Reopened` can store; an `InDiscussion`
/// store needs at least one turn. The two AI calls then run as a detached, claim-guarded job
/// like every other model-calling route (ADR-0010) — the immediate response is the transcript
/// with the "thinking" indicator, and once the job flips truth to `Stored` the `/pending` poll
/// swaps `_stored.html` into `#discussion` (HX-Retarget, see `respond_discussion_or_stored`).
pub async fn store_idea(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> Result<axum::response::Html<String>, WebError> {
    let vault_dir = state.config.vault_dir.clone();
    let idea = store::read_idea(&vault_dir, &slug)?; // 404 if missing
    guard_can_store(&vault_dir, &slug, &idea)?;

    // Busy already: don't queue a second job — just re-show the in-flight state.
    if !jobs::try_claim(&state.jobs, &slug) {
        return respond_with_transcript(&state, &slug);
    }
    let ts = state.clone();
    let tslug = slug.clone();
    let abort = jobs::spawn_job(&state.jobs, &slug, async move {
        jobs::set_note(
            &ts.jobs,
            &tslug,
            "consolidating the idea + extracting memory…",
        );
        match run_store_work(&ts, &tslug).await {
            Ok(None) => jobs::mark_done(&ts.jobs, &tslug),
            Ok(Some(notice)) => jobs::mark_notice(&ts.jobs, &tslug, notice),
            Err(m) => jobs::mark_failed(&ts.jobs, &tslug, m),
        }
    });
    jobs::set_abort(&state.jobs, &slug, abort);

    respond_with_transcript(&state, &slug)
}

/// The D9 store guard, synchronous and side-effect-free apart from the one read it needs
/// (`InDiscussion` must have at least one turn). Shared by the HTTP handler and the inbound MCP
/// `store_idea` tool (`web::mcp_server::tasks`, ADR-0024), so both surfaces reject a bad store
/// request identically before any job is claimed.
pub(crate) fn guard_can_store(
    vault_dir: &std::path::Path,
    slug: &str,
    idea: &Idea,
) -> Result<(), WebError> {
    match idea.frontmatter.state {
        IdeaState::Stored => Err(WebError::BadRequest("idea is already stored".into())),
        IdeaState::Draft => Err(WebError::BadRequest(
            "nothing to store yet — discuss the idea first".into(),
        )),
        IdeaState::InDiscussion => {
            let conversation = store::read_conversation(vault_dir, slug)?;
            if store::split_turns(&conversation).is_empty() {
                return Err(WebError::BadRequest(
                    "store needs at least one discussion turn (D9)".into(),
                ));
            }
            Ok(())
        }
        IdeaState::Reopened => Ok(()), // re-store merges memory, no turn guard (D9 table)
    }
}

/// The background half of Store: the extraction pipeline (which acquires the shared permit
/// itself, scoped to exactly its two AI calls, ADR-0006 — this task must not hold one around
/// it) followed by the log-not-fail reindex. Truth is only touched after both calls succeed,
/// so an abort mid-run persists nothing partial. `Ok(Some(_))` is a notice for the owner (facts
/// quarantined, context truncated) shown under the stored panel. Shared by the HTTP handler and
/// the inbound MCP `store_idea` task (`web::mcp_server::tasks`, ADR-0024).
pub(crate) async fn run_store_work(state: &AppState, slug: &str) -> Result<Option<String>, String> {
    let outcome = memory::extract::extract_and_store(
        &state.llm,
        &state.ai_semaphore,
        &state.config.vault_dir,
        slug,
        state.llm.context_budget(),
    )
    .await
    .map_err(|e| e.to_string())?;
    reindex_logged(state);
    tracing::info!(
        slug,
        new_facts = outcome.new_facts,
        updated_facts = outcome.updated_facts,
        quarantined = outcome.quarantined,
        "idea stored"
    );
    Ok(store_notice(&outcome))
}

/// The one-shot line shown under the stored panel when a store did something the owner should
/// know about: facts held back by the evidence gate, or a discussion too long to read in full.
fn store_notice(outcome: &memory::extract::StoreOutcome) -> Option<String> {
    let mut parts = Vec::new();
    if outcome.quarantined > 0 {
        parts.push(format!(
            "{} extracted fact{} had no supporting quote in the discussion and went to the \
             quarantined-facts artifact instead of memory",
            outcome.quarantined,
            if outcome.quarantined == 1 { "" } else { "s" }
        ));
    }
    if outcome.context_truncated {
        parts.push(
            "the discussion was longer than the model's context, so its oldest turns were not \
             read while storing"
                .to_string(),
        );
    }
    (!parts.is_empty()).then(|| format!("Stored — but {}.", parts.join("; and ")))
}

/// The guard + state-flip half of Reopen, with no HTML rendering — shared by the HTTP handler and
/// the inbound MCP `reopen_idea` tool (`web::mcp_server::tools`, ADR-0024). Returns the
/// now-`Reopened` idea.
pub(crate) async fn reopen_idea_core(state: &AppState, slug: &str) -> Result<Idea, WebError> {
    let vault_dir = state.config.vault_dir.clone();
    let mut idea = store::read_idea(&vault_dir, slug)?; // 404 if missing
    if idea.frontmatter.state != IdeaState::Stored {
        return Err(WebError::BadRequest(
            "only a stored idea can be reopened".into(),
        ));
    }

    // D13: MEMORY.md always, fact bodies under budget — the next chat turn (D11) reassembles
    // the same context; loading here validates it and surfaces inclusion counts.
    let loaded = memory::load::load_context(&vault_dir, slug, state.llm.context_budget())?;
    tracing::info!(
        slug,
        included_memory = loaded.included_memory,
        included_turns = loaded.included_turns,
        truncated = loaded.truncated,
        "reopen context loaded"
    );

    idea.frontmatter.state = IdeaState::Reopened;
    idea.frontmatter.updated = Utc::now();
    store::write_idea(&vault_dir, &idea)?;
    reindex_logged(state);
    Ok(idea)
}

/// R5 — `POST /idea/{slug}/reopen` — re-enter discussion with memory loaded as context (D13).
///
/// Truth-idempotent apart from the state flip: memory context is loaded (index first, bodies
/// under budget) and the frontmatter flips `stored → reopened`; body and memory are untouched.
pub async fn reopen_idea(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> Result<axum::response::Html<String>, WebError> {
    reopen_idea_core(&state, &slug).await?;

    let vault_dir = state.config.vault_dir.clone();
    let conversation = store::read_conversation(&vault_dir, &slug)?;
    let health = state.llm.probe().await;
    let skills = state.skills.snapshot();
    let pending = crate::web::jobs::peek(&state.jobs, &slug);
    let queued_items = crate::web::jobs::list_queued(&state.queues, &slug);
    // The reopen form swaps `#discussion` (buttons come back with it); the subhead badge sits
    // outside, so carry an out-of-band badge flip alongside.
    let mut html = build_discussion(
        &vault_dir,
        &slug,
        &conversation,
        health,
        state.llm.settings().backend,
        &state.llm.model(),
        true,
        &skills,
        pending,
        queued_items,
        state.llm.context_budget().max_bytes,
        state.llm.tool_context_bytes(),
    )?
    .render()
    .map_err(|e| WebError::Internal(format!("template render: {e}")))?;
    html.push_str(&state_badge_oob(IdeaState::Reopened));
    Ok(axum::response::Html(html))
}

/// Concept actions run only in the two active discussion states (D9 has no skill/swarm edge
/// for `Draft` or `Stored`). Exhaustive match: a future state must make an explicit decision
/// here rather than falling through to "allowed".
pub(crate) fn guard_discussion_state(state: IdeaState) -> Result<(), WebError> {
    match state {
        IdeaState::InDiscussion | IdeaState::Reopened => Ok(()),
        IdeaState::Draft => Err(WebError::BadRequest(
            "idea is a draft — open the discussion with a first chat turn before running moves"
                .into(),
        )),
        IdeaState::Stored => Err(WebError::BadRequest(
            "idea is stored — reopen it before running moves".into(),
        )),
    }
}

/// Build the live-progress sink for a background job: a closure the orchestrators call to advance
/// the job's note (surfaced in the "thinking" indicator). Routed through `jobs::set_note` so every
/// slot mutation stays behind the `web::jobs` API (D4: `concepts` stays free of `web` — it only
/// sees a plain `Fn(&str)`), and it is a no-op once the slot is gone (cancelled/finished).
pub(crate) fn progress_sink(state: &AppState, slug: &str) -> impl Fn(&str) + Send + Sync {
    let jobs = state.jobs.clone();
    let slug = slug.to_string();
    move |note: &str| jobs::set_note(&jobs, &slug, note)
}

/// R6 — `POST /idea/{slug}/skill/{name}` — apply a named ideation skill as a background job (D18).
/// Stateless: `invoke` appends the assistant turn post-completion and does not change idea state;
/// it gates its own AI call on the shared semaphore. Returns the transcript with the indicator.
pub async fn run_skill(
    State(state): State<AppState>,
    Path((slug, name)): Path<(String, String)>,
) -> Result<axum::response::Html<String>, WebError> {
    let vault_dir = state.config.vault_dir.clone();
    let idea = store::read_idea(&vault_dir, &slug)?; // 404 if missing
    guard_discussion_state(idea.frontmatter.state)?;

    let Some(skill) = state.skills.snapshot().get(&name).cloned() else {
        return Err(WebError::NotFound(format!("skill: {name}")));
    };

    if !jobs::try_claim(&state.jobs, &slug) {
        return respond_with_transcript(&state, &slug);
    }
    let ts = state.clone();
    let tslug = slug.clone();
    let abort = jobs::spawn_job(&state.jobs, &slug, async move {
        match run_skill_work(&ts, &tslug, skill).await {
            Ok(()) => jobs::mark_done(&ts.jobs, &tslug),
            Err(m) => jobs::mark_failed(&ts.jobs, &tslug, m),
        }
    });
    jobs::set_abort(&state.jobs, &slug, abort);
    respond_with_transcript(&state, &slug)
}

async fn run_skill_work(
    state: &AppState,
    slug: &str,
    skill: concepts::skills::Skill,
) -> Result<(), String> {
    let progress = progress_sink(state, slug);
    // Scoped once per job (ADR-0021): the skill turn sees the idea's attached sources.
    let llm = scoped_llm(state, slug);
    let out = concepts::skills::invoke(
        &llm,
        &state.ai_semaphore,
        &state.config.vault_dir,
        slug,
        &skill,
        concepts::skills::ContextSlot {
            budget: llm.context_budget(),
            related: &|max| related_block_logged(state, slug, max),
        },
        &progress,
    )
    .await
    .map_err(|e| e.to_string())?;
    if out.trim().is_empty() {
        return Err("the foil returned nothing — try again".to_string());
    }
    reindex_logged(state);
    Ok(())
}

/// Form body for R7: optional comma-separated angle list; defaults to the canonical D14 set.
#[derive(Debug, serde::Deserialize)]
pub struct SwarmForm {
    #[serde(default)]
    pub angles: String,
}

/// Upper bound on one swarm request's fan-out: the semaphore bounds concurrency (K in
/// flight), this bounds total queued work N so a single request cannot monopolize the shared
/// AI budget for every other route (ADR-0006 spirit: bounded latency, not just bounded rate).
/// The idea page's angle picker renders this as its selection cap; this check stays authoritative.
pub const MAX_ANGLES: usize = 8;

/// R7 — `POST /idea/{slug}/swarm` — fan out subagents, converge, as a background job (D14). The
/// swarm bounds itself on the shared semaphore and persists only the converged synthesis.
pub async fn run_swarm(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    axum::Form(form): axum::Form<SwarmForm>,
) -> Result<axum::response::Html<String>, WebError> {
    let vault_dir = state.config.vault_dir.clone();
    let idea = store::read_idea(&vault_dir, &slug)?; // 404 if missing
    guard_discussion_state(idea.frontmatter.state)?;

    let angles: Vec<String> = if form.angles.trim().is_empty() {
        crate::concepts::swarm::DEFAULT_ANGLES
            .iter()
            .map(|a| a.to_string())
            .collect()
    } else {
        form.angles
            .split(',')
            .map(|a| a.trim().to_string())
            .filter(|a| !a.is_empty())
            .collect()
    };
    if angles.len() > MAX_ANGLES {
        return Err(WebError::BadRequest(format!(
            "too many angles: {} (max {MAX_ANGLES})",
            angles.len()
        )));
    }
    // Reject unknown angles synchronously (they map to skills) — `swarm` checks this too, but that
    // now runs in the background task, so validate here to keep a bad request a 400 not an error turn.
    // A capstone (build-prompt) folds the whole discussion into one deliverable; it is not an angle.
    // One snapshot for the whole job, so a skill-book reload can't change the angles mid-run.
    let skills = state.skills.snapshot();
    for angle in &angles {
        match skills.get(angle) {
            None => return Err(WebError::BadRequest(format!("unknown angle: {angle}"))),
            Some(s) if s.stage == crate::domain::SkillStage::Capstone => {
                return Err(WebError::BadRequest(format!(
                    "{angle} is a capstone, not a swarm angle"
                )))
            }
            Some(_) => {}
        }
    }

    if !jobs::try_claim(&state.jobs, &slug) {
        return respond_with_transcript(&state, &slug);
    }
    let ts = state.clone();
    let tslug = slug.clone();
    let abort = jobs::spawn_job(&state.jobs, &slug, async move {
        match run_swarm_work(&ts, &tslug, &skills, angles).await {
            Ok(()) => jobs::mark_done(&ts.jobs, &tslug),
            Err(m) => jobs::mark_failed(&ts.jobs, &tslug, m),
        }
    });
    jobs::set_abort(&state.jobs, &slug, abort);
    respond_with_transcript(&state, &slug)
}

/// R22 — `POST /idea/{slug}/workflow/{name}` — run a deterministic workflow as a background job
/// (D19/D32). Script-driven control flow (a fixed sequence of fan-out / chained / audit /
/// synthesis stages), as opposed to the free-form swarm: the same workflow takes the same path
/// every run; only stage content varies. Only the final output is persisted, as one turn.
pub async fn run_workflow(
    State(state): State<AppState>,
    Path((slug, name)): Path<(String, String)>,
) -> Result<axum::response::Html<String>, WebError> {
    let vault_dir = state.config.vault_dir.clone();
    let idea = store::read_idea(&vault_dir, &slug)?; // 404 if missing
    guard_discussion_state(idea.frontmatter.state)?;
    // Unknown name is a synchronous 404, not an error turn (run_workflow checks again, but that
    // now runs in the background task).
    if concepts::workflows::get_workflow(&name).is_none() {
        return Err(WebError::NotFound(format!("workflow: {name}")));
    }

    if !jobs::try_claim(&state.jobs, &slug) {
        return respond_with_transcript(&state, &slug);
    }
    let ts = state.clone();
    let tslug = slug.clone();
    let abort = jobs::spawn_job(&state.jobs, &slug, async move {
        match run_workflow_work(&ts, &tslug, &name).await {
            Ok(()) => jobs::mark_done(&ts.jobs, &tslug),
            Err(m) => jobs::mark_failed(&ts.jobs, &tslug, m),
        }
    });
    jobs::set_abort(&state.jobs, &slug, abort);
    respond_with_transcript(&state, &slug)
}

async fn run_workflow_work(state: &AppState, slug: &str, name: &str) -> Result<(), String> {
    let progress = progress_sink(state, slug);
    // Scoped once per job (ADR-0021): every step turn sees the idea's attached sources.
    let llm = scoped_llm(state, slug);
    let skills = state.skills.snapshot();
    let outcome = concepts::workflows::run_workflow(
        &llm,
        &state.ai_semaphore,
        &skills,
        &state.config.vault_dir,
        slug,
        name,
        llm.context_budget(),
        llm.settings().audit_findings,
        &|max| related_block_logged(state, slug, max),
        &progress,
    )
    .await
    .map_err(|e| e.to_string())?;
    if outcome.synthesis.trim().is_empty() {
        return Err("the workflow produced nothing — try again".to_string());
    }
    reindex_logged(state);
    Ok(())
}

async fn run_swarm_work(
    state: &AppState,
    slug: &str,
    skills: &concepts::skills::SkillRegistry,
    angles: Vec<String>,
) -> Result<(), String> {
    let progress = progress_sink(state, slug);
    // One scoped clone, shared across the whole fan-out (ADR-0021): every angle's agent turn
    // carries the same resolved sources — resolved once, not once per subagent.
    let llm = scoped_llm(state, slug);
    let outcome = concepts::swarm::swarm(
        &llm,
        &state.ai_semaphore,
        skills,
        &state.config.vault_dir,
        slug,
        angles,
        llm.context_budget(),
        llm.settings().audit_findings,
        &|max| related_block_logged(state, slug, max),
        &progress,
    )
    .await
    .map_err(|e| e.to_string())?;
    if outcome.synthesis.trim().is_empty() {
        return Err("the swarm produced nothing — try again".to_string());
    }
    reindex_logged(state);
    Ok(())
}

/// `POST /idea/{slug}/memory/{fact}/delete` — delete one accumulated memory fact (cleanup to
/// shrink the context a reopen reloads); returns the re-rendered memory panel.
pub async fn delete_memory_fact(
    State(state): State<AppState>,
    Path((slug, fact)): Path<(String, String)>,
) -> Result<axum::response::Html<String>, WebError> {
    store::delete_memory_fact(&state.config.vault_dir, &slug, &fact)?; // 404 if idea missing
    reindex_logged(&state);
    let entries = store::read_memory_index(&state.config.vault_dir, &slug)?.entries;
    Ok(axum::response::Html(
        crate::web::routes::ideas::render_memory_panel(&slug, entries)?,
    ))
}

/// `POST /idea/{slug}/turn/{index}/delete` — remove one transcript turn (the deliberate-edit
/// exception to append-only, see `vault::store::delete_turn`); returns the re-rendered transcript.
pub async fn delete_turn(
    State(state): State<AppState>,
    Path((slug, index)): Path<(String, usize)>,
) -> Result<axum::response::Html<String>, WebError> {
    store::delete_turn(&state.config.vault_dir, &slug, index)?; // 404 if the idea is missing
    reindex_logged(&state);
    respond_with_transcript(&state, &slug)
}
