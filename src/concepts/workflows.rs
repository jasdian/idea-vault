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

use crate::ai::budget::{related_allowance, ContextBudget};
use crate::ai::AiError;
use crate::ai::LlmBackend;
use crate::concepts::agents::{build_prompt, AgentResult, AgentRole, AgentTask};
use crate::concepts::audit::{self, AuditReport, Finding};
use crate::concepts::build_plan::finish::{finish_as, Finished, PlanInputs, PlanMode};
use crate::concepts::build_plan::gates::AuditView;
use crate::concepts::skills::{ask_on_contract, hydrate_context, RelatedProvider, SkillRegistry};
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

/// What a workflow run produced: the final stage's output (for a workflow ending in a build-plan
/// step, the pointer turn it appended) plus every fan-out agent's raw result
/// (`None` = failed agent, skipped by the judge) and the audit, if one ran.
#[derive(Debug)]
pub struct WorkflowOutcome {
    pub workflow: &'static str,
    pub synthesis: String,
    pub step_results: Vec<Option<AgentResult>>,
    pub audit: Option<AuditReport>,
}

/// How a build-plan planner routes audited findings into plan sections, keyed to the kind labels
/// [`kind_label`] puts on each finding line; carried ahead of the findings block, never model text.
const BUILD_PLAN_PREAMBLE: &str = "## How to use the findings\n\
Each finding line starts with its kind (decision, open question, risk, next action or fact), then its verdict in brackets when the audit ran.\n\
- A REFUTED finding, whatever its kind, is never Settled and never a task; leave it out of the plan entirely. This rule wins over every rule below.\n\
- A CONFIRMED decision goes to Settled only with a verbatim owner quote found in the discussion below; otherwise it goes to Open questions (Q#).\n\
- An open question that is not REFUTED goes to Open questions (Q#).\n\
- A risk that is not REFUTED goes to Verify first (P# with a read-only check) or to Kill criteria (K#).\n\
- A CONFIRMED next action becomes a task candidate, keeping any paths or commands it named.\n\
- An UNCERTAIN decision, next action or fact goes to Open questions (Q#).\n\
- A fact is background only, never a task.\n\
- A finding with no verdict label is unchecked: treat it as UNCERTAIN.";

/// Divisor of the stage budget the planner's preamble and findings block may take together, so
/// quotable discussion survives. The cap holds only while `budget / divisor` covers the preamble,
/// the two-byte join and [`MIN_PLANNER_BLOCK`]; below that the preamble is carried whole and the
/// block keeps its floor.
const PLANNER_FINDINGS_DIVISOR: usize = 3;

/// Divisor of the stage budget a non-planner chained step's findings block may take.
const DEFAULT_FINDINGS_DIVISOR: usize = 2;

/// The separator between carried blocks.
const CARRY_JOIN: &str = "\n\n";

/// Smallest byte cap the planner's findings block keeps once the preamble has taken its share,
/// even when that overshoots the shared cap.
const MIN_PLANNER_BLOCK: usize = 200;

/// The plain kind a harvest lens labels its findings with, `None` for any other lens. A finding
/// merged across lenses takes the kind of its first matching lens.
fn kind_label(finding: &Finding) -> Option<&'static str> {
    finding.lenses.iter().find_map(|l| match l.as_str() {
        "extract-key-decisions" => Some("decision"),
        "extract-open-questions" => Some("open question"),
        "extract-risks-assumptions" => Some("risk"),
        "extract-next-actions" => Some("next action"),
        "extract-durable-facts" => Some("fact"),
        _ => None,
    })
}

/// The blocks a chained step after a fan-out carries forward. The planner gets the preamble, and
/// preamble, join and findings block together stay within a third of the budget unless that third
/// cannot hold the preamble plus [`MIN_PLANNER_BLOCK`]; every other step gets the findings block
/// alone, within half.
fn findings_carry(
    findings: &[Finding],
    report: Option<&AuditReport>,
    budget: ContextBudget,
    planner: bool,
) -> Vec<String> {
    if !planner {
        let cap = budget.max_bytes / DEFAULT_FINDINGS_DIVISOR;
        return vec![findings_block(findings, report, budget, cap)];
    }
    let cap = (budget.max_bytes / PLANNER_FINDINGS_DIVISOR)
        .saturating_sub(BUILD_PLAN_PREAMBLE.len() + CARRY_JOIN.len())
        .max(MIN_PLANNER_BLOCK);
    vec![
        BUILD_PLAN_PREAMBLE.to_string(),
        findings_block(findings, report, budget, cap),
    ]
}

/// The findings as a carried-forward block for a chained step, capped at `cap` bytes. Each line
/// leads with its [`kind_label`]; audited lines carry the verdict and the auditor's clipped
/// reason; when the audit failed the verdicts are defaults, so the findings are listed unlabelled
/// and the block says so.
fn findings_block(
    findings: &[Finding],
    report: Option<&AuditReport>,
    budget: ContextBudget,
    cap: usize,
) -> String {
    let allowance = audit::finding_allowance(budget, findings.len());
    let audited = report.filter(|r| !r.failed);
    let lines = findings
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let text = audit::clip(&f.text, allowance);
            let kind = kind_label(f).map_or(String::new(), |k| format!("{k} · "));
            match audited.and_then(|r| r.verdicts.get(i)) {
                Some(v) if v.reason.trim().is_empty() => {
                    format!("- {kind}[{}] {text} ({})", v.label.as_str(), f.provenance())
                }
                Some(v) => format!(
                    "- {kind}[{}] {text} ({}) — auditor: {}",
                    v.label.as_str(),
                    f.provenance(),
                    audit::clip(v.reason.trim(), allowance / 2)
                ),
                None => format!("- {kind}{text} ({})", f.provenance()),
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let preface = match report {
        Some(r) if r.failed => Some(
            "The audit was unavailable, so these findings carry no verdicts; treat every one as \
             unchecked.",
        ),
        Some(_) => Some(audit::VERDICT_GUIDANCE),
        None => None,
    };
    let heading = "## Prior stage: findings\n";
    let block = match preface {
        Some(p) => format!("{heading}{p}\n\n{lines}"),
        None => format!("{heading}{lines}"),
    };
    audit::clip(&block, cap)
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

/// The context a fan-out or chained stage sees: the related-ideas block, the carried-forward
/// blocks, then the idea, memory and discussion hydrated under whatever budget the carried blocks
/// leave. `related` is asked per stage, against what that stage's hydrated context leaves of its
/// own `rest` budget, because carried blocks shrink `rest` from stage to stage.
fn stage_context(
    vault_dir: &Path,
    idea_slug: &str,
    budget: ContextBudget,
    carried: &[String],
    related: RelatedProvider<'_>,
) -> Result<String, ConceptError> {
    stage_context_flagged(vault_dir, idea_slug, budget, carried, related, false)
        .map(|(text, _)| text)
}

/// [`stage_context`] plus, when `count_turns` and hydration dropped discussion turns, how many of
/// the discussion's turns it kept and how many there were. The total is read from the vault only
/// on request, since only the planner reports it.
fn stage_context_flagged(
    vault_dir: &Path,
    idea_slug: &str,
    budget: ContextBudget,
    carried: &[String],
    related: RelatedProvider<'_>,
    count_turns: bool,
) -> Result<(String, Option<(usize, usize)>), ConceptError> {
    let carried = carried.join(CARRY_JOIN);
    let rest = ContextBudget::new(budget.max_bytes.saturating_sub(carried.len()));
    let base = hydrate_context(vault_dir, idea_slug, rest)?;
    let block = related(related_allowance(rest, base.text.len()));
    let text = if carried.is_empty() {
        format!("{block}{}", base.text)
    } else {
        format!("{block}{carried}\n\n{}", base.text)
    };
    if !count_turns {
        return Ok((text, None));
    }
    let total = discussion_turns(vault_dir, idea_slug)?;
    let clipped = (base.included_turns < total).then_some((base.included_turns, total));
    Ok((text, clipped))
}

/// How many discussion turns hydration draws from: the turns after any applied compaction.
fn discussion_turns(vault_dir: &Path, idea_slug: &str) -> Result<usize, ConceptError> {
    let turns = store::split_turns(&store::read_conversation(vault_dir, idea_slug)?);
    let compacted = store::read_compacted(vault_dir, idea_slug)?;
    let win = crate::memory::compact::effective_window(&turns, compacted.as_ref());
    Ok(turns.len() - win.applied.unwrap_or(0))
}

/// Why the plan's mode label says the audit was skipped when the Settings toggle is off.
const AUDIT_OFF_REASON: &str = "audit off in Settings";

/// The error text of a build-plan workflow whose every harvester failed.
const NOTHING_HARVESTED: &str = "harvest produced nothing; use the quick build prompt";

/// Gate and persist the planner's answer on the blocking pool, labelled as the multi-step
/// pipeline; no permit is held.
async fn persist_ready_to_build(
    llm: &LlmBackend,
    vault_dir: &Path,
    idea_slug: &str,
    answer: String,
    workflow: &str,
    audit: Option<AuditView>,
    skipped: Option<&str>,
) -> Result<Finished, ConceptError> {
    let vault_dir = vault_dir.to_path_buf();
    let idea_slug = idea_slug.to_string();
    let turn_role = format!("assistant (workflow: {workflow})");
    let lens = workflow.to_string();
    let skipped = skipped.map(str::to_string);
    let model = llm.model();
    let probe = llm.source_probe();
    let joined = tokio::task::spawn_blocking(move || {
        finish_as(
            PlanInputs {
                vault_dir: &vault_dir,
                idea_slug: &idea_slug,
                answer: &answer,
                turn_role: &turn_role,
                lens: &lens,
                model,
                audit: audit.as_ref(),
                probe: &probe,
                now: chrono::Utc::now(),
            },
            PlanMode::ReadyToBuild {
                skipped: skipped.as_deref(),
            },
        )
    })
    .await;
    match joined {
        Ok(result) => result,
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(e) => Err(ConceptError::Vault(crate::vault::VaultError::Io(
            std::io::Error::other(format!("build-plan task did not finish: {e}")),
        ))),
    }
}

/// Run a named workflow against `idea_slug` (D19/D32): execute its stages in order, holding no
/// semaphore permit of its own (every model call takes one), and append the final stage's output
/// — plus the audit appendix, if an audit ran — as one assistant turn. Deterministic control
/// flow: the same workflow takes the same path every run; only stage outputs vary.
///
/// Every fan-out and chained stage is prefixed with a related-ideas block, asked of `related` once
/// per such stage against that stage's own leftover budget; audit and synthesis stages never see
/// it.
///
/// A build-plan workflow whose every harvester failed errors with nothing persisted, pointing at
/// the quick build prompt; the plan's mode label names an audit that was skipped or failed and an
/// audit that confirmed everything.
///
/// Degradation: failed fan-out agents are skipped; a failed middle chained step is skipped with
/// nothing carried forward; a failed final stage, or a fan-out with no usable result before an
/// synthesis, fails the run with nothing persisted; an empty harvest skips the audit without a
/// model call, exactly as when the audit is off.
///
/// A workflow whose last stage chains a [`OutputContract::BuildPlan`] skill persists through the
/// build-plan gates instead: the audited harvest (when the audit ran) is carried into them, the
/// plan lands as an artifact and the turn is its pointer; an unusable plan fails the run with
/// nothing persisted.
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
    related: RelatedProvider<'_>,
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
                let context = stage_context(vault_dir, idea_slug, budget, &carried, related)?;
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
                let contract = step
                    .skill
                    .and_then(|s| registry.get(s))
                    .map_or(OutputContract::Free, |s| s.contract);
                let planner = contract == OutputContract::BuildPlan;
                // A chained step after a fan-out reads its findings (with verdicts, if audited).
                if !step_results.is_empty() {
                    if findings.is_none() {
                        findings = gather(&step_results).ok();
                    }
                    if planner && findings.is_none() {
                        return Err(ConceptError::Ai(AiError::Backend(NOTHING_HARVESTED.into())));
                    }
                    if let Some(f) = &findings {
                        carried.extend(findings_carry(f, report.as_ref(), budget, planner));
                    }
                }
                let (mut context, clipped) = stage_context_flagged(
                    vault_dir, idea_slug, budget, &carried, related, planner,
                )?;
                if let (true, Some((kept, total))) = (planner, clipped) {
                    context = format!(
                        "(discussion clipped: the latest {kept} of {total} turns are shown)\n\n{context}"
                    );
                }
                let task = AgentTask {
                    role: step.role,
                    skill: step.skill.map(str::to_string),
                    context,
                };
                let prompt = build_prompt(registry, &task)?;
                let llm = ollama.for_role(step.role.as_str());
                match ask_on_contract(&llm, ai_semaphore, prompt, contract, label, progress).await {
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
                    match gather(&step_results) {
                        Ok(f) => findings = Some(f),
                        Err(ConceptError::NothingToSynthesize) => {
                            note("nothing harvested — audit skipped");
                            carried.push(
                                "## Prior stage: audit\nnothing harvested — audit skipped".into(),
                            );
                            continue;
                        }
                        Err(e) => return Err(e),
                    }
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
    let planner = match workflow.stages.last() {
        Some(Stage::Chain(step)) => step
            .skill
            .and_then(|s| registry.get(s))
            .filter(|s| s.contract == OutputContract::BuildPlan)
            .map(|_| step.role),
        _ => None,
    };
    if let Some(role) = planner {
        let audit = match (&report, &findings) {
            (Some(r), Some(f)) => Some(AuditView::new(f, r)),
            _ => None,
        };
        let skipped = (!audit_findings).then_some(AUDIT_OFF_REASON);
        let finished = persist_ready_to_build(
            &ollama.for_role(role.as_str()),
            vault_dir,
            idea_slug,
            output,
            workflow.name,
            audit,
            skipped,
        )
        .await?;
        output = finished.pointer;
    } else if output.trim().is_empty() {
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

    fn finding(lens: &str, text: &str) -> Finding {
        Finding {
            lenses: vec![lens.to_string()],
            role: AgentRole::Harvester,
            text: text.to_string(),
        }
    }

    #[test]
    fn ready_to_build_preamble_is_only_for_the_planner() {
        let findings: Vec<Finding> = (0..4)
            .map(|_| finding("extract-key-decisions", &"d ".repeat(600)))
            .collect();
        let budget = ContextBudget::new(4096);
        let other = findings_carry(&findings, None, budget, false);
        assert_eq!(other.len(), 1);
        assert!(!other[0].contains("## How to use the findings"));
        assert!(other[0].starts_with("## Prior stage: findings\n"));
        assert!(other[0].len() > budget.max_bytes / 3, "half-budget cap");
        assert!(other[0].len() <= budget.max_bytes / 2 + '…'.len_utf8());
        let planner = findings_carry(&findings, None, budget, true);
        assert_eq!(planner.len(), 2);
        assert_eq!(planner[0], BUILD_PLAN_PREAMBLE);
    }

    #[test]
    fn kind_label_names_every_harvest_lens() {
        let labels: Vec<Option<&str>> = crate::concepts::knowledge::LENSES
            .iter()
            .map(|l| kind_label(&finding(l, "x")))
            .collect();
        assert_eq!(
            labels,
            [
                Some("decision"),
                Some("fact"),
                Some("open question"),
                Some("risk"),
                Some("next action"),
            ]
        );
        assert_eq!(kind_label(&finding("premortem", "x")), None);
        let merged = Finding {
            lenses: vec![
                "extract-key-decisions".into(),
                "extract-risks-assumptions".into(),
            ],
            role: AgentRole::Harvester,
            text: "x".into(),
        };
        assert_eq!(kind_label(&merged), Some("decision"));
    }

    #[test]
    fn an_unaudited_finding_line_is_the_kind_then_the_text() {
        let f = [finding("extract-key-decisions", "Ship solo")];
        let block = findings_block(&f, None, ContextBudget::new(4096), 4096);
        assert!(
            block
                .lines()
                .any(|l| l.starts_with("- decision · Ship solo (")),
            "{block}"
        );
    }

    #[test]
    fn ready_to_build_preamble_is_carried_whole_when_the_budget_cannot_hold_it() {
        let findings = [finding("extract-key-decisions", &"d ".repeat(600))];
        let planner = findings_carry(&findings, None, ContextBudget::new(1500), true);
        assert_eq!(planner[0], BUILD_PLAN_PREAMBLE);
        assert!(planner[1].len() <= MIN_PLANNER_BLOCK);
        let roomy = ContextBudget::new(9000);
        let planner = findings_carry(&findings, None, roomy, true);
        let total = planner.join(CARRY_JOIN).len();
        assert!(
            total <= roomy.max_bytes / PLANNER_FINDINGS_DIVISOR,
            "{total}"
        );
    }

    #[test]
    fn unknown_workflow_lookup_is_none() {
        assert!(get_workflow("does-not-exist").is_none());
    }
}
