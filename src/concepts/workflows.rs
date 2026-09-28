//! Workflows: deterministic multi-stage orchestrations over an idea — a fixed pipeline of
//! fan-out, chained, audit, and synthesis stages, as opposed to free-form chat
//! (docs/06-concepts/workflows.md D19, D32).
//!
//! Script-driven, not model-driven: the control flow (which stages, in which order) is fixed by
//! the workflow definition; only stage *content* is generated. A chained step's output is carried
//! forward as a `## Prior stage` block into every later stage, so a workflow can steelman an idea
//! and then attack the steelman, or harvest findings and then fold them into a build prompt. The
//! parallel stage delegates to `swarm`'s bounded fan-out primitive; a failed fan-out agent drops
//! to a null result the judge skips; only the final stage's output is persisted as a turn
//! (intermediates stay out of truth).

use std::path::Path;

use tokio::sync::Semaphore;

use crate::ai::budget::ContextBudget;
use crate::ai::LlmBackend;
use crate::concepts::agents::{build_prompt, AgentResult, AgentRole, AgentTask};
use crate::concepts::audit::{self, AuditReport, Finding};
use crate::concepts::skills::{ask_on_contract, hydrate_context, SkillRegistry};
use crate::concepts::swarm::{fan_out, judge, synthesize};
use crate::concepts::ConceptError;
use crate::domain::OutputContract;
use crate::vault::store;

/// One agent in a workflow: a role, optionally through a skill lens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkflowStep {
    pub role: AgentRole,
    pub skill: Option<&'static str>,
}

/// One stage of a workflow (D32).
#[derive(Debug, Clone, Copy)]
pub enum Stage {
    /// Run these steps in parallel over the same context (the swarm fan-out primitive); their
    /// answers become findings for a later `Audit` / `Synthesize` / `Chain`.
    FanOut(&'static [WorkflowStep]),
    /// Run one step alone. Mid-workflow, its output is carried forward to every later stage; as
    /// the last stage, its output is the workflow's result.
    Chain(WorkflowStep),
    /// The factored audit over the findings gathered so far (skipped when the Settings toggle is
    /// off, docs/adr/0023).
    Audit,
    /// Converge the findings gathered so far into one position.
    Synthesize,
}

impl Stage {
    /// The agents this stage runs (none for the audit and synthesis stages).
    pub fn steps(&self) -> &[WorkflowStep] {
        match self {
            Stage::FanOut(steps) => steps,
            Stage::Chain(step) => std::slice::from_ref(step),
            Stage::Audit | Stage::Synthesize => &[],
        }
    }
}

/// A named, fixed workflow definition.
#[derive(Debug, Clone)]
pub struct Workflow {
    pub name: &'static str,
    pub description: &'static str,
    pub stages: &'static [Stage],
}

const fn step(role: AgentRole, skill: &'static str) -> WorkflowStep {
    WorkflowStep {
        role,
        skill: Some(skill),
    }
}

/// The canonical "interrogate an idea" fan-out — the D19 node list: diverse critics + a
/// researcher.
const INTERROGATE_STEPS: &[WorkflowStep] = &[
    step(AgentRole::Critic, "premortem"),
    step(AgentRole::Critic, "cheapest-disproof"),
    step(AgentRole::Researcher, "constraints"),
    step(AgentRole::Critic, "second-order-effects"),
];

/// The attack that follows a steelman.
const ATTACK_STEPS: &[WorkflowStep] = &[
    step(AgentRole::Critic, "premortem"),
    step(AgentRole::Critic, "cheapest-disproof"),
    step(AgentRole::Critic, "devils-advocate"),
];

/// The five knowledge-harvest lenses (`concepts::knowledge::LENSES`), as a fan-out.
const HARVEST_STEPS: &[WorkflowStep] = &[
    step(AgentRole::Harvester, "extract-key-decisions"),
    step(AgentRole::Harvester, "extract-durable-facts"),
    step(AgentRole::Harvester, "extract-open-questions"),
    step(AgentRole::Harvester, "extract-risks-assumptions"),
    step(AgentRole::Harvester, "extract-next-actions"),
];

/// The built-in workflow definitions shipping with the binary — the skill book's named recipes.
pub fn builtin_workflows() -> &'static [Workflow] {
    const WORKFLOWS: &[Workflow] = &[
        Workflow {
            name: "interrogate",
            description: "Fan out diverse critics + a researcher, audit the findings, synthesize \
                          one position (the canonical D19 run-it-into-the-ground pass)",
            stages: &[
                Stage::FanOut(INTERROGATE_STEPS),
                Stage::Audit,
                Stage::Synthesize,
            ],
        },
        Workflow {
            name: "steelman-then-attack",
            description: "Build the strongest case for the idea first, then send three critics at \
                          that steelman, audit what they find, and synthesize",
            stages: &[
                Stage::Chain(step(AgentRole::Advocate, "steelman")),
                Stage::FanOut(ATTACK_STEPS),
                Stage::Audit,
                Stage::Synthesize,
            ],
        },
        Workflow {
            name: "ready-to-build",
            description: "Harvest what the discussion settled, audit it, then fold the survivors \
                          into a ready-to-paste build prompt for a coding agent",
            stages: &[
                Stage::FanOut(HARVEST_STEPS),
                Stage::Audit,
                Stage::Chain(step(AgentRole::Synthesizer, "build-prompt")),
            ],
        },
    ];
    WORKFLOWS
}

/// Look up a built-in workflow by name.
pub fn get_workflow(name: &str) -> Option<&'static Workflow> {
    builtin_workflows().iter().find(|w| w.name == name)
}

/// What a workflow run produced: the final stage's output plus every fan-out agent's raw result
/// (`None` = failed agent, skipped by the judge) and the audit, if one ran.
#[derive(Debug)]
pub struct WorkflowOutcome {
    pub workflow: &'static str,
    pub synthesis: String,
    pub step_results: Vec<Option<AgentResult>>,
    pub audit: Option<AuditReport>,
}

/// The findings as a carried-forward block for a chained step, verdicts included when audited.
fn findings_block(findings: &[Finding], report: Option<&AuditReport>) -> String {
    let lines = findings
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let verdict = report
                .and_then(|r| r.verdicts.get(i))
                .map(|v| format!(" [{}]", v.label.as_str()))
                .unwrap_or_default();
            format!("- {}{verdict} ({})", f.text, f.provenance())
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("## Prior stage: findings\n{lines}")
}

/// The findings a run's fan-outs produced, judged, deduped and capped — or
/// [`ConceptError::NothingToSynthesize`] when no agent produced anything usable.
fn gather(results: &[Option<AgentResult>]) -> Result<Vec<Finding>, ConceptError> {
    let shortlist = judge(results);
    if shortlist.is_empty() {
        return Err(ConceptError::NothingToSynthesize);
    }
    Ok(audit::findings_from(&shortlist, audit::MAX_AUDIT_FINDINGS))
}

/// The context a fan-out or chained stage sees: the carried-forward blocks, then the idea,
/// memory and discussion hydrated under whatever budget the carried blocks leave.
fn stage_context(
    vault_dir: &Path,
    idea_slug: &str,
    budget: ContextBudget,
    carried: &[String],
) -> Result<String, ConceptError> {
    let carried = carried.join("\n\n");
    let rest = ContextBudget::new(budget.max_bytes.saturating_sub(carried.len()));
    let base = hydrate_context(vault_dir, idea_slug, rest)?;
    Ok(if carried.is_empty() {
        base.text
    } else {
        format!("{carried}\n\n{}", base.text)
    })
}

/// Run a named workflow against `idea_slug` (D19/D32): execute its stages in order, holding no
/// semaphore permit of its own (every model call takes one), and append the final stage's output
/// — plus the audit appendix, if an audit ran — as one assistant turn. Deterministic control
/// flow: the same workflow takes the same path every run; only stage outputs vary.
///
/// Degradation: failed fan-out agents are skipped; a failed middle chained step is skipped with
/// nothing carried forward; a failed final stage, or a fan-out with no usable result before an
/// audit/synthesis, fails the run with nothing persisted.
#[allow(clippy::too_many_arguments)]
pub async fn run_workflow(
    ollama: &LlmBackend,
    ai_semaphore: &Semaphore,
    registry: &SkillRegistry,
    vault_dir: &Path,
    idea_slug: &str,
    name: &str,
    budget: ContextBudget,
    audit_findings: bool,
    progress: &(dyn Fn(&str) + Sync),
) -> Result<WorkflowOutcome, ConceptError> {
    let workflow = get_workflow(name).ok_or_else(|| ConceptError::UnknownWorkflow(name.into()))?;

    // Fail fast if a step names a skill the registry doesn't have — before any AI call.
    for stage in workflow.stages {
        for step in stage.steps() {
            if let Some(skill) = step.skill {
                if registry.get(skill).is_none() {
                    return Err(ConceptError::UnknownSkill(skill.to_string()));
                }
            }
        }
    }

    let total = workflow.stages.len();
    let mut carried: Vec<String> = Vec::new();
    let mut step_results: Vec<Option<AgentResult>> = Vec::new();
    let mut findings: Option<Vec<Finding>> = None;
    let mut report: Option<AuditReport> = None;
    let mut output = String::new();

    for (i, stage) in workflow.stages.iter().enumerate() {
        let last = i + 1 == total;
        let note = |what: &str| progress(&format!("workflow · {name} · {}/{total}: {what}", i + 1));
        match stage {
            Stage::FanOut(steps) => {
                note(&format!("fanning out {} angles", steps.len()));
                let context = stage_context(vault_dir, idea_slug, budget, &carried)?;
                let tasks = steps
                    .iter()
                    .map(|s| AgentTask {
                        role: s.role,
                        skill: s.skill.map(str::to_string),
                        context: context.clone(),
                    })
                    .collect();
                let on_done = |done: usize, of: usize, angle: &str| {
                    note(&format!("fanned out {done}/{of} {angle}"));
                };
                step_results.extend(fan_out(ollama, ai_semaphore, registry, tasks, &on_done).await);
                findings = None;
            }
            Stage::Chain(step) => {
                let label = step.skill.unwrap_or(step.role.as_str());
                note(label);
                // A chained step after a fan-out reads its findings (with verdicts, if audited).
                if !step_results.is_empty() {
                    if findings.is_none() {
                        findings = gather(&step_results).ok();
                    }
                    if let Some(f) = &findings {
                        carried.push(findings_block(f, report.as_ref()));
                    }
                }
                let task = AgentTask {
                    role: step.role,
                    skill: step.skill.map(str::to_string),
                    context: stage_context(vault_dir, idea_slug, budget, &carried)?,
                };
                let contract = step
                    .skill
                    .and_then(|s| registry.get(s))
                    .map_or(OutputContract::Free, |s| s.contract);
                let prompt = build_prompt(registry, &task)?;
                match ask_on_contract(ollama, ai_semaphore, prompt, contract, label, progress).await
                {
                    Ok(answer) if last => output = answer,
                    Ok(answer) => carried.push(format!("## Prior stage: {label}\n{answer}")),
                    Err(e) if last => return Err(e),
                    Err(ConceptError::SemaphoreClosed) => {
                        return Err(ConceptError::SemaphoreClosed)
                    }
                    Err(e) => tracing::warn!(
                        workflow = name,
                        step = label,
                        error = %e,
                        "chained step failed; continuing without it"
                    ),
                }
            }
            Stage::Audit => {
                if !audit_findings {
                    continue;
                }
                if findings.is_none() {
                    findings = Some(gather(&step_results)?);
                }
                let f = findings.as_deref().unwrap_or_default();
                note(&format!("auditing {} findings", f.len()));
                report = Some(
                    audit::audit(
                        ollama,
                        ai_semaphore,
                        registry,
                        vault_dir,
                        idea_slug,
                        f,
                        budget,
                    )
                    .await?,
                );
            }
            Stage::Synthesize => {
                if findings.is_none() {
                    findings = Some(gather(&step_results)?);
                }
                let f = findings.as_deref().unwrap_or_default();
                note(&format!("converging {} findings", f.len()));
                let statement = store::read_idea(vault_dir, idea_slug)?.body;
                let synthesis = synthesize(
                    ollama,
                    ai_semaphore,
                    registry,
                    &statement,
                    f,
                    report.as_ref(),
                    budget,
                )
                .await?;
                if last {
                    output = synthesis;
                } else {
                    carried.push(format!("## Prior stage: synthesis\n{synthesis}"));
                }
            }
        }
    }

    // Persist boundary: only the final output becomes truth, as one labelled turn.
    if output.trim().is_empty() {
        tracing::warn!(
            workflow = workflow.name,
            idea_slug,
            "workflow final stage returned empty output; nothing persisted"
        );
    } else {
        let appendix = match (&report, &findings) {
            (Some(r), Some(f)) => audit::appendix(f, r),
            _ => String::new(),
        };
        // append_turn owns the heading grammar and escapes embedded "## " lines (no forged
        // turn boundaries from model output).
        store::append_turn(
            vault_dir,
            idea_slug,
            &format!("assistant (workflow: {})", workflow.name),
            &format!("{output}{appendix}"),
        )?;
    }

    Ok(WorkflowOutcome {
        workflow: workflow.name,
        synthesis: output,
        step_results,
        audit: report,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_interrogate_is_fan_out_audit_synthesize_over_the_d19_nodes() {
        let wf = get_workflow("interrogate").expect("built-in exists");
        assert!(matches!(
            wf.stages,
            [Stage::FanOut(_), Stage::Audit, Stage::Synthesize]
        ));
        let shape: Vec<(AgentRole, Option<&str>)> = wf.stages[0]
            .steps()
            .iter()
            .map(|s| (s.role, s.skill))
            .collect();
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
    fn every_builtin_step_names_a_registered_skill() {
        let registry = SkillRegistry::builtin();
        for wf in builtin_workflows() {
            for stage in wf.stages {
                for s in stage.steps() {
                    let skill = s.skill.expect("built-in steps are lenses");
                    assert!(registry.get(skill).is_some(), "{}: {skill}", wf.name);
                }
            }
        }
    }

    #[test]
    fn harvest_steps_mirror_the_knowledge_lenses() {
        let skills: Vec<&str> = HARVEST_STEPS.iter().filter_map(|s| s.skill).collect();
        assert_eq!(skills, crate::concepts::knowledge::LENSES);
    }

    #[test]
    fn unknown_workflow_lookup_is_none() {
        assert!(get_workflow("does-not-exist").is_none());
    }
}
