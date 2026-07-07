//! Subagent swarming: fan out N agents in parallel against one idea, each from an independent
//! angle, then converge/synthesize (docs/06-concepts/swarm.md D14, D21; ADR-0006).
//!
//! Bounding: this orchestrator holds NO semaphore permit of its own — every `run_agent` call
//! acquires one, so at most K Ollama calls are ever in flight process-wide and the N−K excess
//! queues (backpressure). Concurrency comes from polling all agent futures together
//! (`join_all`), not from spawning unbounded tasks. Each agent sees the same budgeted context
//! block, hydrated once (agents are blind to each other — diverse lenses over identical input).

use std::path::Path;

use futures::future::join_all;
use tokio::sync::Semaphore;

use crate::ai::budget::ContextBudget;
use crate::ai::ollama::OllamaClient;
use crate::concepts::agents::{run_agent, AgentResult, AgentRole, AgentTask};
use crate::concepts::skills::{hydrate_context, SkillRegistry};
use crate::concepts::ConceptError;
use crate::vault::store;

/// What a swarm run produced: the converged synthesis plus the per-angle raw results
/// (`None` = that agent failed and was skipped by the judge — degrade, don't abort).
#[derive(Debug)]
pub struct SwarmOutcome {
    pub synthesis: String,
    pub agent_results: Vec<Option<AgentResult>>,
}

/// Judge (D14 "rank / dedupe"): deterministic code, not a model call. Drops failed agents and
/// empty outputs, dedupes byte-identical findings, keeps angle order.
fn judge(results: &[Option<AgentResult>]) -> Vec<&AgentResult> {
    let mut shortlist: Vec<&AgentResult> = Vec::new();
    for result in results.iter().flatten() {
        if result.content.is_empty() {
            continue;
        }
        if shortlist.iter().any(|kept| kept.content == result.content) {
            continue;
        }
        shortlist.push(result);
    }
    shortlist
}

/// Run the D14 pipeline for `idea_slug`: one Critic agent per angle (an angle is a skill name —
/// e.g. `premortem`, `cheapest-disproof`), bounded fan-out, judge, then a Synthesizer converges
/// the shortlist. The synthesis is appended to `conversation.md` as a single assistant turn only
/// after everything completes; intermediate agent outputs are never persisted.
///
/// Unknown angles fail fast before any model call. If every agent fails the swarm errors with
/// [`ConceptError::NothingToSynthesize`] and nothing is appended.
pub async fn swarm(
    ollama: &OllamaClient,
    ai_semaphore: &Semaphore,
    registry: &SkillRegistry,
    vault_dir: &Path,
    idea_slug: &str,
    angles: Vec<String>,
    budget: ContextBudget,
) -> Result<SwarmOutcome, ConceptError> {
    // Fail fast on a misconfigured request — before any AI call.
    for angle in &angles {
        if registry.get(angle).is_none() {
            return Err(ConceptError::UnknownSkill(angle.clone()));
        }
    }

    // One budgeted context block for every agent (D21; hydrated once, lenses differ per angle).
    let context = hydrate_context(vault_dir, idea_slug, budget)?;

    // Bounded fan-out: N futures polled concurrently; each run_agent acquires its own permit,
    // so in-flight Ollama calls never exceed K and the rest queue (D14/ADR-0006).
    let fanout = angles.iter().map(|angle| {
        let task = AgentTask {
            role: AgentRole::Critic,
            skill: Some(angle.clone()),
            context: context.text.clone(),
        };
        async move {
            match run_agent(ollama, ai_semaphore, registry, task).await {
                Ok(result) => Some(result),
                Err(e) => {
                    // Degrade, don't abort: a failed agent is a null result the judge skips.
                    tracing::warn!(angle = %angle, error = %e, "swarm agent failed; skipping");
                    None
                }
            }
        }
    });
    let agent_results: Vec<Option<AgentResult>> = join_all(fanout).await;

    let shortlist = judge(&agent_results);
    if shortlist.is_empty() {
        return Err(ConceptError::NothingToSynthesize);
    }

    // Synthesizer converges the shortlisted findings (one more bounded AI call).
    let findings = shortlist
        .iter()
        .enumerate()
        .map(|(i, r)| format!("Finding {} ({}):\n{}", i + 1, r.role.as_str(), r.content))
        .collect::<Vec<_>>()
        .join("\n\n");
    let synthesis = run_agent(
        ollama,
        ai_semaphore,
        registry,
        AgentTask {
            role: AgentRole::Synthesizer,
            skill: None,
            context: findings,
        },
    )
    .await?
    .content;

    // Persist boundary: the single converged result becomes one assistant turn, only now.
    if synthesis.is_empty() {
        // Consistent with skills::invoke: an Ok-but-empty model response is surfaced, and no
        // empty turn is appended (D24: surface, not swallow).
        tracing::warn!(
            idea_slug,
            "swarm synthesizer returned empty output; nothing persisted"
        );
    } else {
        let turn = format!("## assistant (swarm)\n{synthesis}\n");
        store::append_conversation(vault_dir, idea_slug, &turn)?;
    }

    Ok(SwarmOutcome {
        synthesis,
        agent_results,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(role: AgentRole, content: &str) -> Option<AgentResult> {
        Some(AgentResult {
            role,
            content: content.to_string(),
        })
    }

    #[test]
    fn judge_skips_nulls_and_empties_and_dedupes_keeping_order() {
        let results = vec![
            result(AgentRole::Critic, "finding A"),
            None,
            result(AgentRole::Critic, ""),
            result(AgentRole::Critic, "finding B"),
            result(AgentRole::Critic, "finding A"),
        ];
        let shortlist = judge(&results);
        let contents: Vec<_> = shortlist.iter().map(|r| r.content.as_str()).collect();
        assert_eq!(contents, ["finding A", "finding B"]);
    }

    #[test]
    fn judge_of_all_nulls_is_empty() {
        assert!(judge(&[None, None]).is_empty());
        assert!(judge(&[]).is_empty());
    }
}
