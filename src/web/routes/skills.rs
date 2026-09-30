//! The skill book (`GET /skills`, `POST /skills/reload`, docs/adr/0022): every registered move
//! grouped by spine stage with its use-when / not-when guidance, where it came from (built-in or
//! the owner's `vault/.skills/`), and any owner file that failed to load. Below the moves, the
//! workflow book (ADR-0035): every workflow with its stages and worst-case cost, its own issues
//! banner, and a detail page per workflow (R49). Reload re-reads both folders with no restart and
//! swaps the `#skills` panel.

use askama::Template as _;
use axum::extract::{Path, State};

use crate::concepts::skills::{Skill, SkillRegistry};
use crate::concepts::workflows::{Stage, Workflow, WorkflowRegistry, WorkflowStep};
use crate::domain::{OutputContract, SkillRole, SkillStage};
use crate::web::state::AppState;
use crate::web::templates::{
    RubricLine, SkillCard, SkillGroup, SkillsList, SkillsPage, StageLine, WorkflowCard,
    WorkflowDetailPage,
};
use crate::web::WebError;

/// The skill book's stage order and the one-line description under each heading.
const STAGES: [(SkillStage, &str); 6] = [
    (
        SkillStage::Steelman,
        "Make the strongest honest case first, so the attack has something worth breaking.",
    ),
    (
        SkillStage::Attack,
        "Try to break it: how it fails, the cheapest disproof, the hostile argument.",
    ),
    (
        SkillStage::Consequence,
        "Ground what survives: constraints, precedents, knock-on effects, size.",
    ),
    (
        SkillStage::Converge,
        "Fold the findings into one position (swarm, workflows and extraction converge too).",
    ),
    (
        SkillStage::Capstone,
        "Turn a settled idea into something to act on.",
    ),
    (
        SkillStage::Extract,
        "Orchestrator-only harvest lenses behind ⛏ extract knowledge — not offered as moves.",
    ),
];

fn role_str(role: SkillRole) -> &'static str {
    match role {
        SkillRole::Critic => "critic",
        SkillRole::Researcher => "researcher",
        SkillRole::Advocate => "advocate",
        SkillRole::Harvester => "harvester",
        SkillRole::Synthesizer => "synthesizer",
    }
}

fn contract_str(contract: OutputContract) -> &'static str {
    match contract {
        OutputContract::Free => "free text",
        OutputContract::BulletsOrEmpty => "bullets or nothing",
        OutputContract::RankedList => "ranked list",
        OutputContract::FencedMarkdown => "one fenced block",
        OutputContract::BuildPlan => "sectioned build plan",
        OutputContract::GroundClaims => "anchored claims",
        OutputContract::Proposal => "one proposal",
        OutputContract::Scorecard => "rubric scores",
        OutputContract::SkillDraft => "skill draft + evidence",
    }
}

fn card(skill: &Skill) -> SkillCard {
    SkillCard {
        name: skill.name.clone(),
        description: skill.description.clone(),
        use_when: skill.use_when.clone(),
        avoid_when: skill.avoid_when.clone(),
        role: role_str(skill.role),
        contract: contract_str(skill.contract),
        source: skill.source.as_str(),
        digest: skill.digest.clone(),
        hidden: skill.hidden,
        origin: skill.origin.clone(),
    }
}

fn groups(registry: &SkillRegistry) -> Vec<SkillGroup> {
    STAGES
        .iter()
        .map(|(stage, blurb)| SkillGroup {
            stage: stage.as_str(),
            blurb,
            skills: registry
                .list()
                .iter()
                .filter(|s| s.stage == *stage)
                .map(card)
                .collect(),
        })
        .collect()
}

/// The worst-case cost of one run, shown before it starts (ADR-0034, owner decision 3): the exact
/// call ceiling, and the `⌈width/K⌉` waves its widest stage waits through at the shared model
/// bound K (ADR-0006) — on a small local model, waves are what the owner actually waits for.
pub(crate) fn cost_line(workflow: &Workflow, k: usize) -> String {
    let k = k.max(1);
    let calls = workflow.call_ceiling();
    let width = workflow.width();
    let waves = width.div_ceil(k);
    format!(
        "up to {calls} model call{} · widest stage {width} → {waves} wave{} at K={k}",
        plural(calls as usize),
        plural(waves)
    )
}

/// The hint a Ground workflow's chip carries on an idea with no attached sources: the run still
/// works, Ground is just skipped at no cost (ADR-0034, Ground step 0).
pub(crate) const NO_SOURCES_HINT: &str = "no sources attached, so its ground stage is skipped";

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// `role·skill`, `role·"angle"`, or the bare role — how a stage line names one agent.
fn step_label(step: &WorkflowStep) -> String {
    let role = step.role.as_str();
    match (&step.skill, &step.angle) {
        (Some(skill), _) => format!("{role}·{skill}"),
        (None, Some(angle)) => format!("{role}·“{angle}”"),
        (None, None) => role.to_string(),
    }
}

fn step_list(steps: &[WorkflowStep]) -> String {
    steps.iter().map(step_label).collect::<Vec<_>>().join(", ")
}

fn stage_detail(stage: &Stage) -> String {
    match stage {
        Stage::FanOut(steps) => format!("in parallel: {}", step_list(steps)),
        Stage::Chain(step) => format!("{}, carried into every later stage", step_label(step)),
        Stage::Audit => {
            "each finding CONFIRMED / UNCERTAIN / REFUTED against the discussion".to_string()
        }
        Stage::Synthesize => "converge the findings into one position".to_string(),
        Stage::Ground(g) if g.readers == 0 => {
            "code map only, no readers; skipped with no sources".to_string()
        }
        Stage::Ground(g) => format!(
            "{} reader{} × {} tool round{}, anchors verified in code; skipped with no sources",
            g.readers,
            plural(g.readers),
            g.tool_rounds,
            plural(g.tool_rounds)
        ),
        Stage::Panel(p) => format!(
            "{} proposers ({}), each scored alone by {} auditor{}; winner and grafts chosen in code",
            p.proposers.len(),
            step_list(&p.proposers),
            p.judges,
            plural(p.judges)
        ),
        Stage::Loop(l) => format!(
            "rounds of {} until {} dry round{} · ≤{} rounds · ≤{} calls",
            step_list(&l.steps),
            l.dry_rounds,
            plural(l.dry_rounds),
            l.max_rounds,
            l.max_calls
        ),
        Stage::Refine(r) => format!(
            "{} rewrites REFUTED / UNCERTAIN findings, then re-audit · ≤{} round{}",
            step_label(&r.step),
            r.max_rounds,
            plural(r.max_rounds)
        ),
    }
}

fn stage_line(stage: &Stage) -> StageLine {
    let rubric = match stage {
        Stage::Panel(p) => p
            .criteria
            .iter()
            .map(|c| RubricLine {
                name: c.name.clone(),
                weight: c.weight,
                zero: c.zero.clone(),
                two: c.two.clone(),
            })
            .collect(),
        _ => Vec::new(),
    };
    StageLine {
        kind: stage.kind().as_str(),
        detail: stage_detail(stage),
        calls: stage.call_ceiling(),
        rubric,
    }
}

fn workflow_card(workflow: &Workflow, k: usize) -> WorkflowCard {
    WorkflowCard {
        name: workflow.name.clone(),
        description: workflow.description.clone(),
        use_when: workflow.use_when.clone(),
        avoid_when: workflow.avoid_when.clone(),
        source: workflow.source.as_str(),
        digest: workflow.digest.clone(),
        hidden: workflow.hidden,
        capstone: workflow.capstone,
        cost: cost_line(workflow, k),
        needs_sources: workflow.needs_sources(),
        stages: workflow.stages.iter().map(stage_line).collect(),
    }
}

fn workflow_cards(registry: &WorkflowRegistry, k: usize) -> Vec<WorkflowCard> {
    registry
        .list()
        .iter()
        .map(|w| workflow_card(w, k))
        .collect()
}

fn render_list(state: &AppState) -> Result<String, WebError> {
    // One snapshot for both halves of the book, so it never lists workflows validated against
    // skills other than the ones shown above them (ADR-0035).
    let book = state.workflows.snapshot();
    SkillsList {
        dir: state.skills.dir().display().to_string(),
        issues: state.skills.issues(),
        groups: groups(&book.skills),
        workflow_dir: state.workflows.dir().display().to_string(),
        workflow_issues: state.workflows.issues(),
        workflows: workflow_cards(&book.workflows, state.config.ai_concurrency),
    }
    .render()
    .map_err(|e| WebError::Internal(format!("template render: {e}")))
}

/// `GET /skills` — the skill book.
pub async fn skills_page(State(state): State<AppState>) -> Result<SkillsPage, WebError> {
    Ok(SkillsPage {
        list_html: render_list(&state)?,
    })
}

/// R49 — `GET /skills/workflow/{name}` — one workflow in full (ADR-0035). Reads the live snapshot,
/// so a reloaded owner file shows at once; a name that is not registered (including one present
/// only as an invalid file) is a 404, the same answer the run route gives.
pub async fn workflow_page(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<WorkflowDetailPage, WebError> {
    let book = state.workflows.snapshot();
    let workflow = book
        .workflows
        .get(&name)
        .ok_or_else(|| WebError::NotFound(format!("workflow '{name}'")))?;
    Ok(WorkflowDetailPage {
        card: workflow_card(workflow, state.config.ai_concurrency),
        body_html: crate::web::templates::render_markdown(&workflow.body),
        raw: workflow.raw.clone(),
        dir: state.workflows.dir().display().to_string(),
    })
}

/// `POST /skills/reload` — re-read the owner skills folder, then revalidate the workflows against
/// those fresh skills as one pair (ADR-0035), and return the refreshed panel. Synchronous: a
/// handful of small file reads, no model call.
pub async fn reload_skills(
    State(state): State<AppState>,
) -> Result<axum::response::Html<String>, WebError> {
    state.workflows.reload(&state.skills);
    Ok(axum::response::Html(render_list(&state)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_stage_is_listed_once_and_every_builtin_lands_in_a_group() {
        let registry = SkillRegistry::builtin();
        let groups = groups(&registry);
        assert_eq!(groups.len(), STAGES.len());
        let placed: usize = groups.iter().map(|g| g.skills.len()).sum();
        assert_eq!(placed, registry.list().len());
    }

    #[test]
    fn cost_line_counts_waves_from_the_widest_stage() {
        let book = crate::concepts::workflows::Book::builtin();
        let panel = book.workflows.get("design-panel").unwrap();
        // The panel's 3 scorers are the widest stage: 3 at K=2 is 2 waves, at K=1 it is 3.
        assert_eq!(
            cost_line(panel, 2),
            "up to 12 model calls · widest stage 3 → 2 waves at K=2"
        );
        assert!(cost_line(panel, 1).ends_with("3 waves at K=1"));
        assert!(cost_line(panel, 0).ends_with("at K=1"), "K is at least 1");
    }

    #[test]
    fn every_builtin_stage_renders_a_line_with_its_share_of_the_ceiling() {
        let book = crate::concepts::workflows::Book::builtin();
        for w in book.workflows.list() {
            let card = workflow_card(w, 2);
            assert_eq!(card.stages.len(), w.stages.len());
            let total: u32 = card.stages.iter().map(|s| s.calls).sum();
            assert_eq!(total, w.call_ceiling(), "{}", w.name);
        }
        let panel = workflow_card(book.workflows.get("design-panel").unwrap(), 2);
        assert_eq!(panel.stages[1].rubric.len(), 4);
    }
}
