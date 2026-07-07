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
}
