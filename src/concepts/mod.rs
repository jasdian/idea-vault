//! `concepts` — the five LLM-harness-inspired primitives applied to one idea: skills, agents,
//! workflows, and subagent swarming. See docs/06-concepts/*.md.

pub mod agents;
pub mod audit;
pub mod build_plan;
pub mod coverage;
pub mod knowledge;
pub mod make_skill;
pub mod skills;
pub mod swarm;
pub mod workflows;

/// Errors produced by the skills/agents/workflows/swarm orchestration primitives.
#[derive(Debug, thiserror::Error)]
pub enum ConceptError {
    #[error("not yet implemented: {0}")]
    NotImplemented(&'static str),
    #[error("unknown skill: {0}")]
    UnknownSkill(String),
    #[error("unknown workflow: {0}")]
    UnknownWorkflow(String),
    #[error("ai error: {0}")]
    Ai(#[from] crate::ai::AiError),
    #[error("vault error: {0}")]
    Vault(#[from] crate::vault::VaultError),
    /// The process-wide AI semaphore was closed — only happens during shutdown.
    #[error("ai concurrency semaphore closed")]
    SemaphoreClosed,
    /// Every fan-out agent failed, so there is nothing for the synthesizer to converge
    /// (degrade-don't-abort stops at the point where there is no signal left, D14).
    #[error("swarm produced no usable agent results to synthesize")]
    NothingToSynthesize,
    /// A build-plan workflow's every harvester failed, so the planner has no findings to fold
    /// (docs/adr/0030); nothing was persisted.
    #[error("harvest produced nothing; use the quick build prompt")]
    NothingHarvested,
    /// The planner's answer named neither a goal nor a task, so no build plan was persisted
    /// (docs/adr/0030).
    #[error("the model's answer had neither a goal nor a task — nothing was saved; try again")]
    PlanUnusable,
    /// The distiller's kept answer held no skill file the loader accepts (docs/adr/0042), even
    /// after the one contract retry; no draft was written.
    #[error(
        "the model's answer was not a usable skill draft ({0}) — nothing was saved; try again"
    )]
    DraftUnusable(String),
}
