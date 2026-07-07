//! Agents: scoped subagent roles — a role prompt plus an I/O contract, the unit a swarm fans out
//! and a workflow sequences (docs/06-concepts/agents.md).

use crate::concepts::ConceptError;

/// A scoped subagent persona (docs/06-concepts/agents.md "Standard roles").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentRole {
    Critic,
    Researcher,
    Synthesizer,
}

/// One bounded unit of work for an agent: a role, an optional skill lens, and a budgeted context
/// block (docs/06-concepts/agents.md "I/O contract").
#[derive(Debug, Clone)]
pub struct AgentTask {
    pub role: AgentRole,
    pub skill: Option<String>,
    pub context: String,
}

/// The result an agent hands back to the orchestrator (judge/synthesizer) to rank or merge.
#[derive(Debug, Clone)]
pub struct AgentResult {
    pub role: AgentRole,
    pub content: String,
}

/// Run a single agent role prompt (optionally through a skill lens) via `ai::ollama`, under the
/// shared concurrency semaphore.
///
/// TODO(agents): see docs/06-concepts/agents.md — resolve `task.skill` via `concepts::skills` if
/// present, build the role persona prompt, call `ai::ollama` under `ADR-0006`'s semaphore, and
/// return an `AgentResult` (or a null-equivalent on failure, per docs/06-concepts/swarm.md D14 —
/// callers/judges treat a failed agent as skippable, not fatal).
pub async fn run_agent(_task: AgentTask) -> Result<AgentResult, ConceptError> {
    Err(ConceptError::NotImplemented("concepts::agents::run_agent"))
}
