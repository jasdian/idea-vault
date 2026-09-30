//! The workflow engine (D19/D32, docs/adr/0034): runs one workflow's stages in order against an
//! idea. Control flow is fixed by the definition; only stage content is generated.
//!
//! Every model call goes through a stage's own `run_agent`/`ask_on_contract` and takes one permit
//! (ADR-0006); the engine holds none. A [`CallBudget`] counts the calls against the workflow's
//! worst-case ceiling and keeps the ceiling of every later stage in reserve, so an elastic stage
//! (a Loop) can never starve the final one. Each stage leaves a [`StageLog`] row and at most one
//! staged artifact; nothing reaches the vault until the final stage has succeeded, and then the
//! turn (or the capstone's plan), the stage artifacts and the run record are written together in
//! one await-free tail — a cancel or a failed final stage persists nothing.

use std::path::Path;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;

use chrono::Utc;
use tokio::sync::Semaphore;

use crate::ai::budget::{related_allowance, ContextBudget};
use crate::ai::LlmBackend;
use crate::concepts::agents::{build_prompt, AgentResult, AgentTask};
use crate::concepts::audit::{self, AuditReport, Finding};
use crate::concepts::build_plan::finish::PlanMode;
use crate::concepts::build_plan::gates::AuditView;
use crate::concepts::skills::{
    ask_on_contract, hydrate_context, persist_plan, prior_plan_block, RelatedProvider,
};
use crate::concepts::swarm::{angles_line, fan_out, judge, synthesize_brief, Brief};
use crate::concepts::workflows::ground::{self, GroundMap, GROUND_DIVISOR};
use crate::concepts::workflows::panel::{self, PanelVerdict};
use crate::concepts::workflows::rounds;
use crate::concepts::workflows::{Book, Stage, Workflow};
use crate::concepts::ConceptError;
use crate::domain::workflow::StageKind;
use crate::domain::{slug, Artifact, ArtifactFrontmatter, ArtifactKind, OutputContract};
use crate::vault::store;

/// Everything one workflow run reads, borrowed for the run's lifetime. `book` is the job's one
/// skills + workflows snapshot (ADR-0035), so a reload mid-run never changes what the run resolves.
pub struct RunCtx<'a> {
    pub llm: &'a LlmBackend,
    pub sem: &'a Semaphore,
    pub book: &'a Book,
    pub vault_dir: &'a Path,
    pub idea_slug: &'a str,
    pub budget: ContextBudget,
    /// The Settings audit toggle (docs/adr/0023): off skips every Audit stage.
    pub audit_on: bool,
    pub related: RelatedProvider<'a>,
    pub progress: &'a (dyn Fn(&str) + Sync),
}

/// What a workflow run produced: the final stage's output (for a workflow ending in a build-plan
/// step, the pointer turn it appended) plus every fan-out agent's and panel proposer's raw result
/// (`None` = failed agent, skipped by the judge), the audit, if one ran, and the slugs of the stage
/// artifacts and run record it wrote (ADR-0034) — empty when no stage staged an artifact.
#[derive(Debug)]
pub struct WorkflowOutcome {
    pub workflow: String,
    pub synthesis: String,
    pub step_results: Vec<Option<AgentResult>>,
    pub audit: Option<AuditReport>,
    pub artifacts: Vec<String>,
}

/// The run's model-call account (docs/adr/0034): calls used so far against the workflow's
/// worst-case ceiling, with the ceilings of the stages after the current one held in reserve.
///
/// Charged by billed requests, not by steps (docs/adr/0037): the run's backend view carries
/// [`meter`](Self::meter), so every request it sends — a retry, each Ollama tool round, one
/// claude process — is counted as it goes out, failed ones included. A tool-using call can
/// therefore cost more than the one call its stage's ceiling assumed, and the reserve check then
/// funds fewer elastic rounds.
pub(crate) struct CallBudget {
    ceilings: Vec<u32>,
    stage: AtomicUsize,
    used: Arc<AtomicU32>,
}

impl CallBudget {
    /// An account over these per-stage ceilings, positioned at the first stage.
    pub(crate) fn new(ceilings: &[u32]) -> Self {
        CallBudget {
            ceilings: ceilings.to_vec(),
            stage: AtomicUsize::new(0),
            used: Arc::new(AtomicU32::new(0)),
        }
    }

    /// The counter the run's backend view charges (`LlmBackend::with_call_meter`).
    pub(crate) fn meter(&self) -> Arc<AtomicU32> {
        self.used.clone()
    }

    /// The workflow's worst case: every stage's ceiling.
    pub(crate) fn ceiling(&self) -> u32 {
        self.ceilings.iter().fold(0, |a, c| a.saturating_add(*c))
    }

    pub(crate) fn used(&self) -> u32 {
        self.used.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn charge(&self, calls: u32) {
        self.used.fetch_add(calls, Ordering::SeqCst);
    }

    fn enter(&self, stage: usize) {
        self.stage.store(stage, Ordering::SeqCst);
    }

    /// Whether `calls` more fit now while every later stage keeps its full ceiling.
    pub(crate) fn can_fund(&self, calls: u32) -> bool {
        let next = self.stage.load(Ordering::SeqCst) + 1;
        let reserve = self
            .ceilings
            .get(next..)
            .unwrap_or_default()
            .iter()
            .fold(0u32, |a, c| a.saturating_add(*c));
        self.used().saturating_add(calls).saturating_add(reserve) <= self.ceiling()
    }
}

/// How one stage went, for the run record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StageStatus {
    Ran,
    /// Made no call and changed nothing, for this reason.
    Skipped(String),
    /// Ran, but with less than it needed (no contest, no verifiable claims, …).
    Degraded(String),
}

impl StageStatus {
    fn label(&self) -> String {
        match self {
            StageStatus::Ran => "ran".to_string(),
            StageStatus::Skipped(why) => format!("skipped — {why}"),
            StageStatus::Degraded(why) => format!("degraded — {why}"),
        }
    }
}

/// One row of the run record: which stage, how it went, what it cost and what it found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageLog {
    pub kind: StageKind,
    pub status: StageStatus,
    pub calls: u32,
    pub detail: String,
    /// Where the stage's calls sit in the run journal (docs/adr/0037), so the record points at
    /// the verbatim answers instead of re-summarizing them. `None` when the run is unjournaled
    /// or the stage made no call.
    pub journal: Option<JournalSpan>,
}

/// A contiguous range of one run journal's call seqs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalSpan {
    pub run_id: String,
    pub first_seq: u32,
    pub last_seq: u32,
}

/// The journal's run id and next call seq, when the backend view is journaled.
fn journal_mark(llm: &LlmBackend) -> Option<(String, u32)> {
    let w = llm.journal()?.lock().ok()?;
    Some((w.run_id().to_string(), w.next_seq()))
}

/// The seqs recorded between two [`journal_mark`]s; `None` when nothing was.
fn journal_span(
    before: Option<(String, u32)>,
    after: Option<(String, u32)>,
) -> Option<JournalSpan> {
    let ((run_id, first), (_, next)) = (before?, after?);
    (next > first).then(|| JournalSpan {
        run_id,
        first_seq: first,
        last_seq: next - 1,
    })
}

/// A stage artifact held back until the run's final persist succeeds (docs/adr/0034).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingArtifact {
    pub kind: ArtifactKind,
    pub title: String,
    pub lens: Option<String>,
    pub body: String,
}

/// What a stage reports back to the engine.
pub(crate) struct StageOutcome {
    pub status: StageStatus,
    pub detail: String,
    pub artifact: Option<PendingArtifact>,
}

impl StageOutcome {
    pub(crate) fn skipped(why: &str) -> Self {
        StageOutcome {
            status: StageStatus::Skipped(why.to_string()),
            detail: why.to_string(),
            artifact: None,
        }
    }

    fn ran(detail: String) -> Self {
        StageOutcome {
            status: StageStatus::Ran,
            detail,
            artifact: None,
        }
    }
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
- An UNCERTAIN decision or next action goes to Open questions (Q#).\n\
- A fact is background only, whatever its verdict: never a task, never Settled.\n\
- A finding with no verdict label was not audited, so do not treat it as UNCERTAIN. Route it by kind: a decision is Settled only with a verbatim owner quote, otherwise it goes to Open questions (Q#); a next action becomes a task candidate marked unchecked.";

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
/// preamble, join, any grounded map already in its share (`shared` bytes, docs/adr/0034) and
/// findings block together stay within a third of the budget unless that third cannot hold them
/// plus [`MIN_PLANNER_BLOCK`]; every other step gets the findings block alone, within half.
fn findings_carry(
    findings: &[Finding],
    report: Option<&AuditReport>,
    budget: ContextBudget,
    planner: bool,
    shared: usize,
) -> Vec<String> {
    if !planner {
        let cap = budget.max_bytes / DEFAULT_FINDINGS_DIVISOR;
        return vec![findings_block(findings, report, budget, cap)];
    }
    let joins = if shared > 0 { 2 } else { 1 } * CARRY_JOIN.len();
    let cap = (budget.max_bytes / PLANNER_FINDINGS_DIVISOR)
        .saturating_sub(BUILD_PLAN_PREAMBLE.len() + shared + joins)
        .max(MIN_PLANNER_BLOCK);
    vec![
        BUILD_PLAN_PREAMBLE.to_string(),
        findings_block(findings, report, budget, cap),
    ]
}

/// The planner's slice of the grounded map (docs/adr/0034): half of its third of the budget,
/// never below [`MIN_PLANNER_BLOCK`].
fn planner_ground_cap(budget: ContextBudget) -> usize {
    (budget.max_bytes / PLANNER_FINDINGS_DIVISOR / 2).max(MIN_PLANNER_BLOCK)
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
/// [`ConceptError::NothingToSynthesize`] when no agent produced anything usable — with how many
/// findings the cap left out.
fn gather(results: &[Option<AgentResult>]) -> Result<(Vec<Finding>, usize), ConceptError> {
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
pub(crate) fn stage_context(
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

/// Everything a run accumulates between stages.
#[derive(Default)]
struct RunState {
    carried: Vec<String>,
    /// The Ground stage's map and where its block sits in `carried`, so a planner can re-carry it
    /// within its own share.
    ground: Option<(usize, GroundMap)>,
    step_results: Vec<Option<AgentResult>>,
    findings: Option<Vec<Finding>>,
    dropped: usize,
    fanned: (Vec<String>, Vec<Option<AgentResult>>),
    report: Option<AuditReport>,
    /// Set by a scored Panel; the next Synthesize runs in graft mode and consumes it.
    panel: Option<PanelVerdict>,
    output: String,
    logs: Vec<StageLog>,
    pending: Vec<(usize, PendingArtifact)>,
}

impl RunState {
    /// The gathered findings, computed from every result so far when a stage has not already.
    fn ensure_findings(&mut self) -> Result<&[Finding], ConceptError> {
        if self.findings.is_none() {
            let (f, d) = gather(&self.step_results)?;
            (self.findings, self.dropped) = (Some(f), d);
        }
        Ok(self.findings.as_deref().unwrap_or_default())
    }

    /// Record the results of a stage that produces findings, so the next consumer re-gathers.
    fn produced(&mut self, labels: Vec<String>, results: Vec<Option<AgentResult>>) {
        self.fanned.0.extend(labels);
        self.fanned.1.extend(results.iter().cloned());
        self.extend_results(results);
    }

    /// Add results to the findings pool and invalidate everything computed from the old pool.
    /// An earlier audit report is dropped with the findings: its verdicts pair with findings by
    /// index, so keeping it against a re-gathered, longer list would strike or carry the wrong
    /// items (docs/adr/0034 — unaudited is reported as unaudited, never as another item's verdict).
    fn extend_results(&mut self, results: Vec<Option<AgentResult>>) {
        self.step_results.extend(results);
        self.findings = None;
        self.dropped = 0;
        self.report = None;
    }
}

/// The workflow's final chained step when it is a build-plan planner: its role.
fn planner_role(workflow: &Workflow, book: &Book) -> Option<crate::concepts::agents::AgentRole> {
    match workflow.stages.last() {
        Some(Stage::Chain(step)) => step
            .skill
            .as_deref()
            .and_then(|s| book.skills.get(s))
            .filter(|s| s.contract == OutputContract::BuildPlan)
            .map(|_| step.role),
        _ => None,
    }
}

/// Run a named workflow against `idea_slug` (D19/D32, docs/adr/0034): execute its stages in
/// order, holding no semaphore permit of its own (every model call takes one), then persist the
/// final stage's output — plus the audit appendix, if an audit ran — as one assistant turn, and
/// any stage artifacts with the run record beside it. Deterministic control flow: the same
/// workflow takes the same path every run; only stage outputs vary.
///
/// Every fan-out, chained, panel-proposal and loop stage is prefixed with a related-ideas block,
/// asked of `related` once per such stage against that stage's own leftover budget; audit,
/// scoring and synthesis stages never see it.
///
/// A build-plan workflow whose every harvester failed errors with
/// [`ConceptError::NothingHarvested`] and nothing persisted; the plan's mode label names an audit
/// that was skipped or failed and an audit that confirmed everything.
///
/// Degradation: failed fan-out agents are skipped; a failed middle chained step is skipped with
/// nothing carried forward; a Ground with no sources is skipped with no call; a Panel with fewer
/// than two proposals is no contest; a failed final stage, or no usable result before a
/// synthesis, fails the run with nothing persisted; an empty harvest skips the audit without a
/// model call, exactly as when the audit is off, unless the final stage is the build-plan planner,
/// which errors with [`ConceptError::NothingHarvested`] instead.
///
/// A workflow whose last stage chains a [`OutputContract::BuildPlan`] skill persists through the
/// build-plan gates instead: the audited harvest (when the audit ran) is carried into them, the
/// plan lands as an artifact and the turn is its pointer, untouched; the run record names the
/// plan. An unusable plan fails the run with nothing persisted.
pub async fn run_workflow(ctx: &RunCtx<'_>, name: &str) -> Result<WorkflowOutcome, ConceptError> {
    let book = ctx.book;
    let workflow = book
        .workflows
        .get(name)
        .ok_or_else(|| ConceptError::UnknownWorkflow(name.into()))?;

    // Run-time backstop to the registry's load-time check (ADR-0035): fail fast if a step names a
    // skill the snapshot doesn't have — before any AI call.
    if let Some(skill) = workflow
        .skills()
        .into_iter()
        .find(|s| book.skills.get(s).is_none())
    {
        return Err(ConceptError::UnknownSkill(skill.to_string()));
    }

    let ceilings: Vec<u32> = workflow.stages.iter().map(Stage::call_ceiling).collect();
    let calls = CallBudget::new(&ceilings);
    let metered = ctx.llm.with_call_meter(calls.meter());
    let ctx = &RunCtx {
        llm: &metered,
        ..*ctx
    };
    let total = workflow.stages.len();
    let mut state = RunState::default();

    for (i, stage) in workflow.stages.iter().enumerate() {
        calls.enter(i);
        let kind = stage.kind();
        let note = |what: &str| {
            (ctx.progress)(&format!(
                "workflow · {name} · {}/{total} {}: {what} · calls {}/{}",
                i + 1,
                kind.as_str(),
                calls.used(),
                calls.ceiling()
            ))
        };
        let before = calls.used();
        let mark = journal_mark(ctx.llm);
        let last = i + 1 == total;
        let outcome = run_stage(ctx, workflow, stage, last, &mut state, &calls, &note).await?;
        if let Some(artifact) = outcome.artifact {
            state.pending.push((i, artifact));
        }
        state.logs.push(StageLog {
            kind,
            status: outcome.status,
            calls: calls.used() - before,
            detail: outcome.detail,
            journal: journal_span(mark, journal_mark(ctx.llm)),
        });
    }

    persist(ctx, workflow, state, &calls).await
}

/// Run one stage against the run so far.
async fn run_stage(
    ctx: &RunCtx<'_>,
    workflow: &Workflow,
    stage: &Stage,
    last: bool,
    state: &mut RunState,
    calls: &CallBudget,
    note: &(dyn Fn(&str) + Sync),
) -> Result<StageOutcome, ConceptError> {
    let registry = ctx.book.skills.as_ref();
    match stage {
        Stage::FanOut(steps) => {
            note(&format!("fanning out {} angles", steps.len()));
            let context = stage_context(
                ctx.vault_dir,
                ctx.idea_slug,
                ctx.budget,
                &state.carried,
                ctx.related,
            )?;
            let tasks = steps
                .iter()
                .map(|s| AgentTask {
                    role: s.role,
                    skill: s.skill.clone(),
                    context: context.clone(),
                })
                .collect();
            let on_done = |done: usize, of: usize, angle: &str| {
                note(&format!("fanned out {done}/{of} {angle}"));
            };
            let results = fan_out(ctx.llm, ctx.sem, registry, tasks, &on_done).await;
            let answered = results.iter().flatten().count();
            state.produced(
                steps.iter().map(|s| s.label().to_string()).collect(),
                results,
            );
            Ok(StageOutcome::ran(format!(
                "{answered} of {} answered",
                steps.len()
            )))
        }
        Stage::Chain(step) => {
            let label = step.label();
            note(label);
            let contract = step
                .skill
                .as_deref()
                .and_then(|s| registry.get(s))
                .map_or(OutputContract::Free, |s| s.contract);
            let planner = contract == OutputContract::BuildPlan;
            // A chained step after a producing stage reads its findings (with verdicts, if
            // audited).
            if !state.step_results.is_empty() || state.findings.is_some() {
                if state.findings.is_none() {
                    (state.findings, state.dropped) = match gather(&state.step_results) {
                        Ok((f, d)) => (Some(f), d),
                        Err(_) => (None, 0),
                    };
                }
                if planner && state.findings.is_none() {
                    return Err(ConceptError::NothingHarvested);
                }
                // The planner re-carries the grounded map inside its own third (docs/adr/0034).
                let mut shared = 0;
                if planner {
                    if let Some((at, map)) = &state.ground {
                        let block = ground::carried_block(map, planner_ground_cap(ctx.budget));
                        shared = block.len();
                        state.carried[*at] = block;
                    }
                }
                if let Some(f) = &state.findings {
                    state.carried.extend(findings_carry(
                        f,
                        state.report.as_ref(),
                        ctx.budget,
                        planner,
                        shared,
                    ));
                }
            }
            if planner {
                let prior = prior_plan_block(ctx.vault_dir, ctx.idea_slug);
                if !prior.is_empty() {
                    state.carried.push(prior.trim_end().to_string());
                }
            }
            let (mut context, clipped) = stage_context_flagged(
                ctx.vault_dir,
                ctx.idea_slug,
                ctx.budget,
                &state.carried,
                ctx.related,
                planner,
            )?;
            if let (true, Some((kept, total))) = (planner, clipped) {
                context = format!(
                    "(discussion clipped: the latest {kept} of {total} turns are shown)\n\n{context}"
                );
            }
            let task = AgentTask {
                role: step.role,
                skill: step.skill.clone(),
                context,
            };
            let prompt = build_prompt(registry, &task)?;
            let llm = ctx.llm.for_role(step.role.as_str());
            match ask_on_contract(&llm, ctx.sem, prompt, contract, label, ctx.progress).await {
                Ok((answer, _)) => {
                    if last {
                        state.output = answer;
                    } else {
                        state
                            .carried
                            .push(format!("## Prior stage: {label}\n{answer}"));
                    }
                    Ok(StageOutcome::ran(label.to_string()))
                }
                Err(e) if last => Err(e),
                Err(ConceptError::SemaphoreClosed) => Err(ConceptError::SemaphoreClosed),
                Err(e) => {
                    tracing::warn!(
                        workflow = %workflow.name,
                        step = label,
                        error = %e,
                        "chained step failed; continuing without it"
                    );
                    Ok(StageOutcome {
                        status: StageStatus::Degraded("the step failed".into()),
                        detail: label.to_string(),
                        artifact: None,
                    })
                }
            }
        }
        Stage::Audit => {
            if !ctx.audit_on {
                return Ok(StageOutcome::skipped(AUDIT_OFF_REASON));
            }
            let findings = match state.ensure_findings() {
                Ok(f) => f.to_vec(),
                Err(ConceptError::NothingToSynthesize) => {
                    note("nothing harvested — audit skipped");
                    state
                        .carried
                        .push("## Prior stage: audit\nnothing harvested — audit skipped".into());
                    return Ok(StageOutcome::skipped("nothing harvested"));
                }
                Err(e) => return Err(e),
            };
            note(&format!("auditing {} findings", findings.len()));
            let report = audit::audit(
                ctx.llm,
                ctx.sem,
                registry,
                ctx.vault_dir,
                ctx.idea_slug,
                &findings,
                ctx.budget,
            )
            .await?;
            let detail = format!(
                "{} findings · {}",
                findings.len(),
                if report.failed {
                    "audit unavailable"
                } else {
                    "audited"
                }
            );
            state.report = Some(report);
            Ok(StageOutcome::ran(detail))
        }
        Stage::Synthesize => {
            let findings = state.ensure_findings()?.to_vec();
            note(&format!("converging {} findings", findings.len()));
            let statement = store::read_idea(ctx.vault_dir, ctx.idea_slug)?.body;
            let verdict = state.panel.take();
            let brief = Brief {
                idea_statement: &statement,
                findings: &findings,
                report: state.report.as_ref(),
                directive: verdict.as_ref().map_or("", |v| v.directive.as_str()),
            };
            let synthesis =
                synthesize_brief(ctx.llm, ctx.sem, registry, &brief, ctx.budget).await?;
            let (synthesis, detail) = match &verdict {
                Some(v) => {
                    let (kept, dropped) =
                        panel::strip_invalid_grafts(&synthesis, &v.labels, v.winner);
                    let detail = if dropped.is_empty() {
                        format!("graft mode · P{} as the spine", v.winner)
                    } else {
                        format!(
                            "graft mode · P{} as the spine · {} invalid graft line(s) stripped",
                            v.winner,
                            dropped.len()
                        )
                    };
                    (kept, detail)
                }
                None => (synthesis, format!("{} findings", findings.len())),
            };
            if last {
                state.output = synthesis;
            } else {
                state
                    .carried
                    .push(format!("## Prior stage: synthesis\n{synthesis}"));
            }
            Ok(StageOutcome::ran(detail))
        }
        Stage::Ground(spec) => {
            let (outcome, map) = ground::run_ground(ctx, spec, note).await?;
            if let Some(map) = map {
                state.carried.push(ground::carried_block(
                    &map,
                    ctx.budget.max_bytes / GROUND_DIVISOR,
                ));
                state.ground = Some((state.carried.len() - 1, map));
            }
            Ok(outcome)
        }
        Stage::Panel(p) => {
            let run = panel::run_panel(ctx, p, &state.carried, note).await?;
            let labels = (1..=p.proposers.len())
                .map(|k| format!("panel-p{k}"))
                .collect();
            state.produced(labels, run.results);
            if let Some(carry) = run.carry {
                state.carried.push(carry);
            }
            state.panel = run.verdict;
            Ok(run.outcome)
        }
        Stage::Loop(spec) => {
            let run = rounds::run_loop(ctx, spec, &state.carried, calls, note).await?;
            // Loop results are merged items, not angles: they join the findings without adding
            // to the angles line.
            state.extend_results(run.results);
            Ok(run.outcome)
        }
        Stage::Refine(spec) => match state.findings.as_mut() {
            Some(findings) => {
                rounds::run_refine(ctx, spec, findings, &mut state.report, note).await
            }
            None => {
                note("nothing to refine — refine skipped");
                Ok(StageOutcome::skipped("nothing to refine"))
            }
        },
    }
}

/// The run record (docs/adr/0034): one row per stage, the calls spent against the ceiling, where
/// the final output went, and the stage artifacts.
fn run_record(
    workflow: &Workflow,
    logs: &[StageLog],
    calls: &CallBudget,
    output: &str,
    stage_slugs: &[String],
) -> String {
    let mut out = format!(
        "# Workflow run — {}\n\n{} of {} model calls (worst case) · final output: {output}\n\n\
         | # | Stage | Status | Calls | Detail |\n|---|---|---|---|---|\n",
        workflow.name,
        calls.used(),
        calls.ceiling()
    );
    for (i, log) in logs.iter().enumerate() {
        let detail = match &log.journal {
            Some(span) => format!(
                "{} · journal {} #{}–#{}",
                log.detail, span.run_id, span.first_seq, span.last_seq
            ),
            None => log.detail.clone(),
        };
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            i + 1,
            log.kind.as_str(),
            ground::cell(&log.status.label()),
            log.calls,
            ground::cell(&detail)
        ));
    }
    if !stage_slugs.is_empty() {
        let links: Vec<String> = stage_slugs.iter().map(|s| format!("[[{s}]]")).collect();
        out.push_str(&format!("\n{STAGE_ARTIFACTS_LABEL}{}\n", links.join(" · ")));
    }
    out
}

/// The stage artifacts and the run record, slugged and ready to write, and the trailing turn
/// line that names them. Empty when no stage staged an artifact: a workflow of the classic kinds,
/// or one whose Ground found no sources, leaves exactly the vault it always did. Pure over the
/// vault's existing slugs.
fn staged_artifacts(
    ctx: &RunCtx<'_>,
    workflow: &Workflow,
    state: &RunState,
    calls: &CallBudget,
    final_output: &str,
) -> Vec<Artifact> {
    if state.pending.is_empty() {
        return Vec::new();
    }
    let now = Utc::now();
    let stamp = now.format("%Y%m%d-%H%M%S").to_string();
    let model = ctx.llm.model();
    let taken = |candidate: &str| {
        store::artifact_exists(ctx.vault_dir, ctx.idea_slug, candidate).unwrap_or(false)
    };
    let artifact = |slug: String, kind, title: String, lens, body| Artifact {
        frontmatter: ArtifactFrontmatter {
            slug,
            title,
            kind,
            lens,
            created: now,
            model: model.clone(),
            revises: None,
            version: None,
            answered: vec![],
        },
        body,
    };
    let mut out: Vec<Artifact> = state
        .pending
        .iter()
        .map(|(i, p)| {
            let kind = workflow.stages[*i].kind().as_str().replace('_', "-");
            let slug = slug::disambiguate(
                &format!("{stamp}-{}-{}-{kind}", workflow.name, i + 1),
                taken,
            );
            artifact(
                slug,
                p.kind,
                format!("{} — {}", p.title, workflow.name),
                p.lens.clone(),
                p.body.clone(),
            )
        })
        .collect();
    let stage_slugs: Vec<String> = out.iter().map(|a| a.frontmatter.slug.clone()).collect();
    let run_slug = slug::disambiguate(&format!("{stamp}-{}-run", workflow.name), taken);
    out.push(artifact(
        run_slug,
        ArtifactKind::WorkflowRun,
        format!("Workflow run — {}", workflow.name),
        None,
        run_record(workflow, &state.logs, calls, final_output, &stage_slugs),
    ));
    out
}

/// The label of the line naming a run's stage artifacts as `[[slug]]` links joined by ` · ` —
/// the last line of a workflow turn that staged any (ADR-0034), which MCP `run_workflow` reads
/// back to return the slugs (ADR-0036).
pub const STAGE_ARTIFACTS_LABEL: &str = "Stage artifacts: ";

fn artifact_line(staged: &[Artifact]) -> String {
    if staged.is_empty() {
        return String::new();
    }
    let links: Vec<String> = staged
        .iter()
        .map(|a| format!("[[{}]]", a.frontmatter.slug))
        .collect();
    format!("\n\n{STAGE_ARTIFACTS_LABEL}{}", links.join(" · "))
}

/// The persist boundary (docs/adr/0034): only the final output becomes a turn (or, for a
/// capstone, a gated plan and its untouched pointer turn), and the stage artifacts and run record
/// are written with it in the same await-free tail, never before.
async fn persist(
    ctx: &RunCtx<'_>,
    workflow: &Workflow,
    mut state: RunState,
    calls: &CallBudget,
) -> Result<WorkflowOutcome, ConceptError> {
    let mut written: Vec<String> = Vec::new();
    if let Some(role) = planner_role(workflow, ctx.book) {
        let audit = match (&state.report, &state.findings) {
            (Some(r), Some(f)) => Some(AuditView::new(f, r)),
            _ => None,
        };
        let skipped = (!ctx.audit_on).then_some(AUDIT_OFF_REASON);
        let finished = persist_plan(
            &ctx.llm.for_role(role.as_str()),
            ctx.vault_dir,
            ctx.idea_slug,
            std::mem::take(&mut state.output),
            &workflow.name,
            audit,
            PlanMode::ReadyToBuild { skipped },
        )
        .await?;
        // The pointer turn stays exactly as the gates wrote it (its prefix marks it a capstone
        // turn); the run record names the plan instead.
        let staged = staged_artifacts(
            ctx,
            workflow,
            &state,
            calls,
            &format!("build plan [[{}]]", finished.artifact_slug),
        );
        for a in &staged {
            store::write_artifact(ctx.vault_dir, ctx.idea_slug, a)?;
            written.push(a.frontmatter.slug.clone());
        }
        state.output = finished.pointer;
    } else if state.output.trim().is_empty() {
        tracing::warn!(
            workflow = %workflow.name,
            idea_slug = ctx.idea_slug,
            "workflow final stage returned empty output; nothing persisted"
        );
    } else {
        let appendix = match (&state.report, &state.findings) {
            (Some(r), Some(f)) => audit::appendix(f, r, state.dropped),
            (None, Some(_)) => audit::unaudited_cap_note(state.dropped),
            _ => String::new(),
        };
        let staged = staged_artifacts(ctx, workflow, &state, calls, "the workflow turn");
        // append_turn owns the heading grammar and escapes embedded "## " lines (no forged
        // turn boundaries from model output).
        store::append_turn(
            ctx.vault_dir,
            ctx.idea_slug,
            &format!("assistant (workflow: {})", workflow.name),
            &format!(
                "{}{}{appendix}{}",
                state.output,
                angles_line(&state.fanned.0, &state.fanned.1),
                artifact_line(&staged)
            ),
        )?;
        for a in &staged {
            store::write_artifact(ctx.vault_dir, ctx.idea_slug, a)?;
            written.push(a.frontmatter.slug.clone());
        }
    }

    Ok(WorkflowOutcome {
        workflow: workflow.name.clone(),
        synthesis: state.output,
        step_results: state.step_results,
        audit: state.report,
        artifacts: written,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::concepts::agents::AgentRole;

    fn finding(lens: &str, text: &str) -> Finding {
        Finding {
            lenses: vec![lens.to_string()],
            role: AgentRole::Harvester,
            text: text.to_string(),
        }
    }

    #[test]
    fn a_producer_after_an_audit_drops_the_stale_report() {
        let mut state = RunState {
            findings: Some(vec![finding("premortem", "old")]),
            dropped: 3,
            report: Some(audit::AuditReport {
                verdicts: vec![],
                answered: 0,
                failed: true,
            }),
            ..RunState::default()
        };
        state.produced(vec!["fan".into()], vec![None]);
        assert!(state.findings.is_none() && state.report.is_none() && state.dropped == 0);

        state.report = Some(audit::AuditReport {
            verdicts: vec![],
            answered: 0,
            failed: true,
        });
        state.extend_results(vec![None]);
        assert!(
            state.report.is_none(),
            "a loop's merged items invalidate the report too"
        );
    }

    #[test]
    fn ready_to_build_preamble_is_only_for_the_planner() {
        let findings: Vec<Finding> = (0..4)
            .map(|_| finding("extract-key-decisions", &"d ".repeat(600)))
            .collect();
        let budget = ContextBudget::new(4096);
        let other = findings_carry(&findings, None, budget, false, 0);
        assert_eq!(other.len(), 1);
        assert!(!other[0].contains("## How to use the findings"));
        assert!(other[0].starts_with("## Prior stage: findings\n"));
        assert!(other[0].len() > budget.max_bytes / 3, "half-budget cap");
        assert!(other[0].len() <= budget.max_bytes / 2 + '…'.len_utf8());
        let planner = findings_carry(&findings, None, budget, true, 0);
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
        let planner = findings_carry(&findings, None, ContextBudget::new(1500), true, 0);
        assert_eq!(planner[0], BUILD_PLAN_PREAMBLE);
        assert!(planner[1].len() <= MIN_PLANNER_BLOCK);
        let roomy = ContextBudget::new(9000);
        let planner = findings_carry(&findings, None, roomy, true, 0);
        let total = planner.join(CARRY_JOIN).len();
        assert!(
            total <= roomy.max_bytes / PLANNER_FINDINGS_DIVISOR,
            "{total}"
        );
    }

    #[test]
    fn a_grounded_map_shares_the_planner_third() {
        let findings: Vec<Finding> = (0..6)
            .map(|_| finding("extract-key-decisions", &"d ".repeat(600)))
            .collect();
        let budget = ContextBudget::new(12_000);
        let ground = "g".repeat(planner_ground_cap(budget));
        let planner = findings_carry(&findings, None, budget, true, ground.len());
        let mut blocks = vec![ground];
        blocks.extend(planner);
        let total = blocks.join(CARRY_JOIN).len();
        assert!(
            total <= budget.max_bytes / PLANNER_FINDINGS_DIVISOR,
            "{total}"
        );
        assert_eq!(
            planner_ground_cap(ContextBudget::new(600)),
            MIN_PLANNER_BLOCK
        );
    }

    #[test]
    fn call_budget_reserves_every_later_stage() {
        let calls = CallBudget::new(&[4, 3, 1]);
        assert_eq!(calls.ceiling(), 8);
        calls.enter(1);
        calls.charge(3);
        assert!(calls.can_fund(3), "3 used + 3 + 1 reserved = 7 <= 8");
        assert!(!calls.can_fund(5));
        calls.enter(2);
        calls.charge(4);
        assert!(calls.can_fund(1) && !calls.can_fund(2));
    }

    #[test]
    fn journal_span_covers_only_the_seqs_a_stage_recorded() {
        let mark = |seq| Some(("20260930T000000000Z-workflow".to_string(), seq));
        assert_eq!(
            journal_span(mark(3), mark(6)),
            Some(JournalSpan {
                run_id: "20260930T000000000Z-workflow".into(),
                first_seq: 3,
                last_seq: 5,
            })
        );
        assert_eq!(journal_span(mark(4), mark(4)), None, "no call, no span");
        assert_eq!(journal_span(None, None), None, "unjournaled");
    }
}
