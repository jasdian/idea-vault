//! Subagent swarming: fan out N agents in parallel against one idea, each from an independent
//! angle, then converge/synthesize (docs/06-concepts/swarm.md D14, D21; ADR-0006).

use crate::concepts::ConceptError;

/// Fan out one agent per angle (bounded by the shared semaphore), then judge and synthesize the
/// results into a single converged position.
///
/// TODO(swarm): see docs/06-concepts/swarm.md §D14/§D21 + ADR-0006 — build N `AgentTask`s (one
/// per angle) with budgeted context (`ai::budget`), dispatch them through the shared `AppState`
/// semaphore so at most K run concurrently against the single local Ollama server (excess N-K
/// tasks queue rather than firing unbounded parallel calls); a failed/timed-out agent yields a
/// null result the judge skips (degrade, don't abort); a Synthesizer merges the shortlisted
/// findings into one result appended as an assistant turn.
pub async fn swarm(_angles: Vec<String>) -> Result<String, ConceptError> {
    Err(ConceptError::NotImplemented("concepts::swarm::swarm"))
}
