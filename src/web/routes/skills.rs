//! The skill book (`GET /skills`, `POST /skills/reload`, docs/adr/0022): every registered move
//! grouped by spine stage with its use-when / not-when guidance, where it came from (built-in or
//! the owner's `vault/.skills/`), and any owner file that failed to load. Reload re-reads the
//! folder with no restart and swaps the `#skills` panel.

use askama::Template as _;
use axum::extract::State;

use crate::concepts::skills::{Skill, SkillRegistry};
use crate::domain::{OutputContract, SkillRole, SkillStage};
use crate::web::state::AppState;
use crate::web::templates::{SkillCard, SkillGroup, SkillsList, SkillsPage};
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
        hidden: skill.hidden,
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

fn render_list(state: &AppState) -> Result<String, WebError> {
    SkillsList {
        dir: state.skills.dir().display().to_string(),
        issues: state.skills.issues(),
        groups: groups(&state.skills.snapshot()),
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
}
