//! Workflows: deterministic multi-step orchestrations over an idea — a fixed DAG of skill/agent
//! steps, as opposed to free-form chat (docs/06-concepts/workflows.md D19).
//!
//! Script-driven, not model-driven: the control flow (which steps, in which graph) is fixed by
//! the workflow definition; only step *content* is generated. The parallel stage delegates to
//! `swarm`'s bounded fan-out primitive; a failed step drops to a null result the judge skips;
//! only the final synthesis is persisted as a turn (intermediates stay out of truth).

use std::path::Path;

use tokio::sync::Semaphore;

use crate::ai::budget::ContextBudget;
use crate::ai::LlmBackend;
use crate::concepts::agents::{AgentResult, AgentRole, AgentTask};
use crate::concepts::skills::{hydrate_context, SkillRegistry};
use crate::concepts::swarm::{fan_out, judge, synthesize};
use crate::concepts::ConceptError;
use crate::vault::store;

/// One fixed node of a workflow's fan-out stage: a role, optionally through a skill lens.
#[derive(Debug, Clone)]
pub struct WorkflowStep {
    pub role: AgentRole,
    pub skill: Option<&'static str>,
}

/// A named, fixed workflow definition. The step list IS the DAG's fan-out stage; judge and
/// synthesizer are the fixed downstream nodes (D19's canonical shape).
#[derive(Debug, Clone)]
pub struct Workflow {
    pub name: &'static str,
    pub description: &'static str,
    pub steps: &'static [WorkflowStep],
}

/// The canonical "interrogate an idea" workflow — the exact D19 fan-out node list
/// (docs/06-concepts/workflows.md): diverse critics + a researcher, judged, then synthesized.
const INTERROGATE_STEPS: &[WorkflowStep] = &[
    WorkflowStep {
        role: AgentRole::Critic,
        skill: Some("premortem"),
    },
    WorkflowStep {
        role: AgentRole::Critic,
        skill: Some("cheapest-disproof"),
    },
    WorkflowStep {
        role: AgentRole::Researcher,
        skill: Some("constraints"),
    },
    WorkflowStep {
        role: AgentRole::Critic,
        skill: Some("second-order-effects"),
    },
];

/// The built-in workflow definitions shipping with the binary.
pub fn builtin_workflows() -> &'static [Workflow] {
    const WORKFLOWS: &[Workflow] = &[Workflow {
        name: "interrogate",
        description: "Fan out diverse critics + a researcher, judge the findings, synthesize \
                      one position (the canonical D19 run-it-into-the-ground pass)",
        steps: INTERROGATE_STEPS,
    }];
    WORKFLOWS
}

/// Look up a built-in workflow by name.
pub fn get_workflow(name: &str) -> Option<&'static Workflow> {
    builtin_workflows().iter().find(|w| w.name == name)
}

/// What a workflow run produced: the synthesis plus per-step raw results (`None` = failed step,
/// skipped by the judge).
#[derive(Debug)]
pub struct WorkflowOutcome {
    pub workflow: &'static str,
    pub synthesis: String,
    pub step_results: Vec<Option<AgentResult>>,
}

/// Run a named workflow against `idea_slug` (D19): hydrate one budgeted context, execute the
/// fixed fan-out stage (bounded by the shared semaphore — this orchestrator holds no permit),
/// judge, synthesize, and append the single result as an assistant turn. Deterministic control
/// flow: the same workflow takes the same path every run; only step outputs vary.
pub async fn run_workflow(
    ollama: &LlmBackend,
    ai_semaphore: &Semaphore,
    registry: &SkillRegistry,
    vault_dir: &Path,
    idea_slug: &str,
    name: &str,
    budget: ContextBudget,
) -> Result<WorkflowOutcome, ConceptError> {
    let workflow = get_workflow(name).ok_or_else(|| ConceptError::UnknownWorkflow(name.into()))?;

    // Fail fast if a step names a skill the registry doesn't have — before any AI call.
    for step in workflow.steps {
        if let Some(skill) = step.skill {
            if registry.get(skill).is_none() {
                return Err(ConceptError::UnknownSkill(skill.to_string()));
            }
        }
    }

    let context = hydrate_context(vault_dir, idea_slug, budget)?;
    let tasks = workflow
        .steps
        .iter()
        .map(|step| AgentTask {
            role: step.role,
            skill: step.skill.map(str::to_string),
            context: context.text.clone(),
        })
        .collect();

    // Workflows don't surface per-step progress (the swarm route does) — pass a no-op reporter.
    let step_results = fan_out(ollama, ai_semaphore, registry, tasks, &|_, _, _| {}).await;
    let shortlist = judge(&step_results);
    if shortlist.is_empty() {
        return Err(ConceptError::NothingToSynthesize);
    }
    let synthesis = synthesize(ollama, ai_semaphore, registry, &shortlist).await?;

    // Persist boundary: only the final synthesis becomes truth, as one labelled turn.
    if synthesis.is_empty() {
        tracing::warn!(
            workflow = workflow.name,
            idea_slug,
            "workflow synthesizer returned empty output; nothing persisted"
        );
    } else {
        // append_turn owns the heading grammar and escapes embedded "## " lines (no forged
        // turn boundaries from model output).
        store::append_turn(
            vault_dir,
            idea_slug,
            &format!("assistant (workflow: {})", workflow.name),
            &synthesis,
        )?;
    }

    Ok(WorkflowOutcome {
        workflow: workflow.name,
        synthesis,
        step_results,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_interrogate_dag_is_fixed_and_matches_d19() {
        let wf = get_workflow("interrogate").expect("built-in exists");
        let shape: Vec<(AgentRole, Option<&str>)> =
            wf.steps.iter().map(|s| (s.role, s.skill)).collect();
        assert_eq!(
            shape,
            vec![
                (AgentRole::Critic, Some("premortem")),
                (AgentRole::Critic, Some("cheapest-disproof")),
                (AgentRole::Researcher, Some("constraints")),
                (AgentRole::Critic, Some("second-order-effects")),
            ]
        );
    }

    #[test]
    fn unknown_workflow_lookup_is_none() {
        assert!(get_workflow("does-not-exist").is_none());
    }
}
