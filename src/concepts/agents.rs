//! Agents: scoped subagent roles — a role prompt plus an I/O contract, the unit a swarm fans out
//! and a workflow sequences (docs/06-concepts/agents.md).
//!
//! An agent is not a process: it is a configured way of calling `ai` for one bounded task. This
//! module only knows how to *run one role well* — building `AgentTask`s (with budgeted context,
//! D21) and consuming `AgentResult`s is the orchestrator's job (`swarm`/`workflows`), and
//! intermediate agent outputs are never persisted to the vault (only a final synthesis becomes a
//! conversation turn, docs/06-concepts/workflows.md).

use tokio::sync::Semaphore;

use crate::ai::ollama::ChatMessage;
use crate::ai::LlmBackend;
use crate::concepts::skills::SkillRegistry;
use crate::concepts::ConceptError;

/// A scoped subagent persona (docs/06-concepts/agents.md "Standard roles").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentRole {
    Critic,
    Researcher,
    Synthesizer,
}

impl AgentRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentRole::Critic => "critic",
            AgentRole::Researcher => "researcher",
            AgentRole::Synthesizer => "synthesizer",
        }
    }

    /// The scoped persona prompt for this role (docs/06-concepts/agents.md "Standard roles").
    /// Roles are prompt configurations — adding one is additive, like a skill.
    pub fn persona(&self) -> &'static str {
        match self {
            AgentRole::Critic => {
                "You are the Critic: adversarial by design. Find the strongest objections, \
                 failure modes, and hidden assumptions in the idea below, ranked by severity. \
                 Ignore politeness; do not balance the view — other agents do that."
            }
            AgentRole::Researcher => {
                "You are the Researcher: gather the relevant considerations, precedents, and \
                 constraints bearing on the idea below, from your own knowledge (you are fully \
                 offline — no browsing). Stick to what is load-bearing; ignore critique and \
                 synthesis — other agents do that."
            }
            AgentRole::Synthesizer => {
                "You are the Synthesizer: neutral. Merge the prior agent outputs below into one \
                 coherent position, surfacing (not smoothing over) the real tensions between \
                 them. Do not add new critiques or research of your own."
            }
        }
    }
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

/// Build the full prompt for a task: role persona, then the optional skill lens hydrated with
/// the (already budgeted, D21) context — or the bare context when no skill is named.
fn build_prompt(registry: &SkillRegistry, task: &AgentTask) -> Result<String, ConceptError> {
    let body = match &task.skill {
        Some(name) => {
            let skill = registry
                .get(name)
                .ok_or_else(|| ConceptError::UnknownSkill(name.clone()))?;
            skill.prompt.replace("{context}", &task.context)
        }
        None => task.context.clone(),
    };
    Ok(format!("{}\n\n{}", task.role.persona(), body))
}

/// Run a single agent role prompt (optionally through a skill lens) via `ai::ollama`, under the
/// shared concurrency semaphore (ADR-0006 — chat, skills, agents, and swarm share one bound; as
/// with `skills::invoke`, callers must NOT already hold a permit or a small bound deadlocks).
///
/// Returns `Err` on any model failure — per docs/06-concepts/swarm.md D14 the *orchestrator*
/// maps that to a null result the judge skips; this function itself does not swallow errors.
/// Nothing is written to the vault here.
pub async fn run_agent(
    ollama: &LlmBackend,
    ai_semaphore: &Semaphore,
    registry: &SkillRegistry,
    task: AgentTask,
) -> Result<AgentResult, ConceptError> {
    let prompt = build_prompt(registry, &task)?;

    let content = {
        let _permit = ai_semaphore
            .acquire()
            .await
            .map_err(|_| ConceptError::SemaphoreClosed)?;
        ollama
            .chat(vec![ChatMessage {
                role: "user".to_string(),
                content: prompt,
            }])
            .await?
    };

    let content = content.trim().to_string();
    if content.is_empty() {
        tracing::warn!(role = task.role.as_str(), "agent returned empty output");
    }
    Ok(AgentResult {
        role: task.role,
        content,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_prompt_uses_persona_and_hydrates_skill_lens() {
        let registry = SkillRegistry::builtin();
        let task = AgentTask {
            role: AgentRole::Critic,
            skill: Some("premortem".to_string()),
            context: "THE-CONTEXT".to_string(),
        };
        let prompt = build_prompt(&registry, &task).unwrap();
        assert!(prompt.starts_with("You are the Critic"));
        assert!(prompt.contains("failed badly 12 months"));
        assert!(prompt.contains("THE-CONTEXT"));
        assert!(!prompt.contains("{context}"));
    }

    #[test]
    fn build_prompt_without_skill_is_persona_plus_context() {
        let registry = SkillRegistry::builtin();
        let task = AgentTask {
            role: AgentRole::Synthesizer,
            skill: None,
            context: "PRIOR-OUTPUTS".to_string(),
        };
        let prompt = build_prompt(&registry, &task).unwrap();
        assert!(prompt.starts_with("You are the Synthesizer"));
        assert!(prompt.ends_with("PRIOR-OUTPUTS"));
    }

    #[test]
    fn unknown_skill_is_an_error_not_a_silent_fallback() {
        let registry = SkillRegistry::builtin();
        let task = AgentTask {
            role: AgentRole::Researcher,
            skill: Some("not-a-skill".to_string()),
            context: "ctx".to_string(),
        };
        assert!(matches!(
            build_prompt(&registry, &task).unwrap_err(),
            ConceptError::UnknownSkill(name) if name == "not-a-skill"
        ));
    }
}
