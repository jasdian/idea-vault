//! Workflows: deterministic multi-stage orchestrations over an idea — a fixed pipeline of
//! fan-out, chained, audit, and synthesis stages, plus the grounded, ranked and bounded stages of
//! ADR-0034 (Ground, Panel, Loop, Refine) — as opposed to free-form chat
//! (docs/06-concepts/workflows.md D19, D32).
//!
//! Script-driven, not model-driven: the control flow (which stages, in which order, when a loop
//! stops, who wins a panel) is fixed by the definition and decided in code; only stage *content*
//! is generated. A chained step's output is carried forward as a `## Prior stage` block into every
//! later stage, so a workflow can steelman an idea and then attack the steelman, or harvest
//! findings and then fold them into a build prompt. The parallel stages delegate to `swarm`'s
//! bounded fan-out primitive; a failed fan-out agent drops to a null result the judge skips. Only
//! the final stage's output becomes a turn; the ADR-0034 stages' artifacts and the run record are
//! written beside it in the same all-or-nothing tail, never as turns and never as evidence.
//!
//! A workflow is a markdown file (ADR-0035): built-ins in `src/concepts/workflows/*.md`, owner
//! additions and overrides in `vault/.workflows/`, loaded and cross-validated against the skill
//! registry by [`registry`]. The engine is [`run`]; the ADR-0034 stages live in [`ground`],
//! [`panel`] and [`rounds`].

pub mod ground;
pub mod panel;
pub mod registry;
pub mod rounds;
pub mod run;

use crate::concepts::agents::AgentRole;
use crate::domain::workflow::{CriterionSpec, GroundSpec, StageKind};

pub use registry::{Book, LiveWorkflows, WorkflowRegistry};
pub use run::{run_workflow, RunCtx, WorkflowOutcome};

/// The most model calls one workflow may cost in the worst case, repairs included (ADR-0034). A
/// definition over it is rejected at load, never clamped.
pub const WORKFLOW_MAX_CALLS: u32 = 32;

/// Name of the one workflow that may be a capstone (ADR-0035), which the capstone row renders
/// instead of the generic list.
pub const READY_TO_BUILD: &str = "ready-to-build";

/// One agent in a workflow: a role, optionally through a skill lens and/or a free-text angle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowStep {
    pub role: AgentRole,
    pub skill: Option<String>,
    pub angle: Option<String>,
}

impl WorkflowStep {
    /// What progress notes and the angles line call this step: its skill, else its role.
    pub fn label(&self) -> &str {
        self.skill.as_deref().unwrap_or(self.role.as_str())
    }
}

/// A resolved `panel` stage (ADR-0034).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelStage {
    pub proposers: Vec<WorkflowStep>,
    pub criteria: Vec<CriterionSpec>,
    pub judges: usize,
}

/// A resolved `loop` stage (ADR-0034).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopStage {
    pub steps: Vec<WorkflowStep>,
    pub dry_rounds: usize,
    pub max_rounds: usize,
    pub max_calls: usize,
}

/// A resolved `refine` stage (ADR-0034).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefineStage {
    pub step: WorkflowStep,
    pub max_rounds: usize,
}

/// One stage of a workflow (D32, ADR-0034).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stage {
    /// Run these steps in parallel over the same context (the swarm fan-out primitive); their
    /// answers become findings for a later `Audit` / `Synthesize` / `Chain`.
    FanOut(Vec<WorkflowStep>),
    /// Run one step alone. Mid-workflow, its output is carried forward to every later stage; as
    /// the last stage, its output is the workflow's result.
    Chain(WorkflowStep),
    /// The factored audit over the findings gathered so far (skipped when the Settings toggle is
    /// off, docs/adr/0023).
    Audit,
    /// Converge the findings gathered so far into one position.
    Synthesize,
    Ground(GroundSpec),
    Panel(PanelStage),
    Loop(LoopStage),
    Refine(RefineStage),
}

impl Stage {
    pub fn kind(&self) -> StageKind {
        match self {
            Stage::FanOut(_) => StageKind::FanOut,
            Stage::Chain(_) => StageKind::Chain,
            Stage::Audit => StageKind::Audit,
            Stage::Synthesize => StageKind::Synthesize,
            Stage::Ground(_) => StageKind::Ground,
            Stage::Panel(_) => StageKind::Panel,
            Stage::Loop(_) => StageKind::Loop,
            Stage::Refine(_) => StageKind::Refine,
        }
    }

    /// The owner-named agents this stage runs; Ground's readers and Panel's scorers run hidden
    /// internal skills and are not listed.
    pub fn steps(&self) -> &[WorkflowStep] {
        match self {
            Stage::FanOut(steps) => steps,
            Stage::Chain(step) => std::slice::from_ref(step),
            Stage::Panel(p) => &p.proposers,
            Stage::Loop(l) => &l.steps,
            Stage::Refine(r) => std::slice::from_ref(&r.step),
            Stage::Audit | Stage::Synthesize | Stage::Ground(_) => &[],
        }
    }

    /// The most model calls this stage can have in flight at once, before the shared bound K
    /// (ADR-0006) serialises them: `width / K` rounded up is how many waves the stage waits
    /// through, which the chips and the book show next to the ceiling (ADR-0034).
    pub fn width(&self) -> usize {
        match self {
            Stage::FanOut(steps) => steps.len(),
            Stage::Ground(g) => g.readers,
            Stage::Panel(p) => p.proposers.len().max(p.judges * p.proposers.len()),
            Stage::Loop(l) => l.steps.len(),
            Stage::Chain(_) | Stage::Audit | Stage::Synthesize | Stage::Refine(_) => 1,
        }
    }

    /// This stage's exact worst-case model calls, repair retries included (ADR-0034): a chained
    /// step may be asked once more on a contract violation, as may each Ground reader.
    pub fn call_ceiling(&self) -> u32 {
        let n = |x: usize| u32::try_from(x).unwrap_or(u32::MAX);
        match self {
            Stage::FanOut(steps) => n(steps.len()),
            Stage::Chain(_) => 2,
            Stage::Audit | Stage::Synthesize => 1,
            Stage::Ground(g) => n(g.readers).saturating_mul(2),
            Stage::Panel(p) => {
                let proposals = n(p.proposers.len());
                proposals.saturating_add(n(p.judges).saturating_mul(proposals))
            }
            Stage::Loop(l) => n(l.max_calls).min(n(l.max_rounds).saturating_mul(n(l.steps.len()))),
            Stage::Refine(r) => n(r.max_rounds).saturating_mul(2),
        }
    }
}

/// Where a registered workflow's definition came from — shown on the book.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowSource {
    BuiltIn,
    /// An owner file in `vault/.workflows/` that replaced the built-in of the same name.
    VaultOverride,
    /// An owner file in `vault/.workflows/` adding a new workflow.
    Vault,
}

impl WorkflowSource {
    pub fn as_str(self) -> &'static str {
        match self {
            WorkflowSource::BuiltIn => "built-in",
            WorkflowSource::VaultOverride => "vault override",
            WorkflowSource::Vault => "vault",
        }
    }
}

/// A named workflow definition, parsed from markdown and validated against the skill registry it
/// was loaded with.
#[derive(Debug, Clone)]
pub struct Workflow {
    pub name: String,
    pub description: String,
    pub use_when: String,
    pub avoid_when: String,
    pub hidden: bool,
    /// The owner-facing explanation below the frontmatter; never sent to a model.
    pub body: String,
    pub stages: Vec<Stage>,
    pub source: WorkflowSource,
    /// Derived, never declared: the workflow chains a build-plan skill (ADR-0035). Only
    /// [`READY_TO_BUILD`] may be one.
    pub capstone: bool,
    /// The file as read, for the book's source view.
    pub raw: String,
}

impl Workflow {
    /// The worst-case model calls of one run (ADR-0034); at most [`WORKFLOW_MAX_CALLS`] once
    /// loaded.
    pub fn call_ceiling(&self) -> u32 {
        self.stages
            .iter()
            .map(Stage::call_ceiling)
            .fold(0, u32::saturating_add)
    }

    /// The widest stage's [`Stage::width`]: what the ceiling's `⌈width/K⌉` waves are counted
    /// from.
    pub fn width(&self) -> usize {
        self.stages.iter().map(Stage::width).max().unwrap_or(0)
    }

    /// Whether a run does its best work with sources attached (it has a Ground stage).
    pub fn needs_sources(&self) -> bool {
        self.stages.iter().any(|s| matches!(s, Stage::Ground(_)))
    }

    /// Every skill a step names, in stage order.
    pub fn skills(&self) -> Vec<&str> {
        self.stages
            .iter()
            .flat_map(Stage::steps)
            .filter_map(|s| s.skill.as_deref())
            .collect()
    }
}

/// A workflow file that failed to load or validate. The registry keeps running on everything
/// else; the book lists these so the owner can fix the file.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowIssue {
    pub file: String,
    pub message: String,
}
