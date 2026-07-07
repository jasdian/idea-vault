//! `concepts` — the five LLM-harness-inspired primitives applied to one idea: skills, agents,
//! workflows, and subagent swarming. See docs/06-concepts/*.md.

pub mod agents;
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
    #[error("ai error: {0}")]
    Ai(#[from] crate::ai::AiError),
    #[error("vault error: {0}")]
    Vault(#[from] crate::vault::VaultError),
    /// The process-wide AI semaphore was closed — only happens during shutdown.
    #[error("ai concurrency semaphore closed")]
    SemaphoreClosed,
}
