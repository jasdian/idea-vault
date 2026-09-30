//! Agents: scoped subagent roles — a role prompt plus an I/O contract, the unit a swarm fans out
//! and a workflow sequences (docs/06-concepts/agents.md).
//!
//! An agent is not a process: it is a configured way of calling `ai` for one bounded task. This
//! module only knows how to *run one role well* — building `AgentTask`s (with budgeted context,
//! D21) and consuming `AgentResult`s is the orchestrator's job (`swarm`/`workflows`/`knowledge`).
//! Whether intermediate agent outputs persist is the orchestrator's decision: `swarm` and
//! `workflows` discard them (only a final synthesis becomes a conversation turn), while
//! `knowledge` persists each lens's findings as an artifact file (docs/adr/0015).

use std::collections::BTreeMap;

use tokio::sync::Semaphore;

use crate::ai::contract;
use crate::ai::ollama::ChatMessage;
use crate::ai::{LlmBackend, RoleProfile};
use crate::concepts::skills::SkillRegistry;
use crate::concepts::ConceptError;
use crate::domain::SkillRole;

/// A scoped subagent persona (docs/06-concepts/agents.md "Standard roles").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentRole {
    Critic,
    Researcher,
    /// Builds the strongest honest case for the idea (the steelman lens).
    Advocate,
    /// Extracts only what the material already says (the `extract-*` lenses).
    Harvester,
    Synthesizer,
    /// Judges other agents' findings against the idea and discussion (the factored audit).
    Auditor,
}

impl AgentRole {
    /// Every role, in canonical order.
    pub const ALL: [AgentRole; 6] = [
        AgentRole::Critic,
        AgentRole::Researcher,
        AgentRole::Advocate,
        AgentRole::Harvester,
        AgentRole::Synthesizer,
        AgentRole::Auditor,
    ];

    /// The role's default call profile (docs/adr/0026): the extractive and judging roles run
    /// cold, the adversarial and advocating roles hot, the synthesizer in between. A blank claude
    /// model or effort inherits the global setting.
    pub fn default_profile(&self) -> RoleProfile {
        let (temperature, claude_effort) = match self {
            AgentRole::Harvester => (0.2, "low"),
            AgentRole::Auditor => (0.2, "high"),
            AgentRole::Synthesizer => (0.5, "high"),
            AgentRole::Researcher => (0.7, "medium"),
            AgentRole::Critic | AgentRole::Advocate => (0.9, ""),
        };
        RoleProfile {
            temperature,
            claude_model: String::new(),
            claude_effort: claude_effort.to_string(),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            AgentRole::Critic => "critic",
            AgentRole::Researcher => "researcher",
            AgentRole::Advocate => "advocate",
            AgentRole::Harvester => "harvester",
            AgentRole::Synthesizer => "synthesizer",
            AgentRole::Auditor => "auditor",
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
            AgentRole::Advocate => {
                "You are the Advocate: make the strongest honest case FOR the idea below — its \
                 best version, the conditions under which it wins, the evidence in its favour. \
                 Do not attack it and do not hedge — other agents do that."
            }
            AgentRole::Harvester => {
                "You are the Harvester: extract only what the material below already contains, \
                 faithfully and without embellishment. Add no new ideas, critique, or outside \
                 knowledge — other agents do that."
            }
            AgentRole::Synthesizer => {
                "You are the Synthesizer: neutral. Merge the prior agent outputs below into one \
                 coherent position, surfacing (not smoothing over) the real tensions between \
                 them. Do not add new critiques or research of your own."
            }
            AgentRole::Auditor => {
                "You are the Auditor: sceptical by default. You did not produce the findings \
                 below and have no stake in them; your only job is to say which ones hold up. \
                 A confirmation you cannot justify from the material is a failure."
            }
        }
    }
}

impl From<SkillRole> for AgentRole {
    /// The persona a skill runs under when an orchestrator fans it out.
    fn from(role: SkillRole) -> Self {
        match role {
            SkillRole::Critic => AgentRole::Critic,
            SkillRole::Researcher => AgentRole::Researcher,
            SkillRole::Advocate => AgentRole::Advocate,
            SkillRole::Harvester => AgentRole::Harvester,
            SkillRole::Synthesizer => AgentRole::Synthesizer,
        }
    }
}

/// The boot-time role profile map, keyed by [`AgentRole::as_str`] (`ai::LlmSettings::role_profiles`).
pub fn default_role_profiles() -> BTreeMap<String, RoleProfile> {
    AgentRole::ALL
        .iter()
        .map(|r| (r.as_str().to_string(), r.default_profile()))
        .collect()
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
    /// The skill lens the agent ran through, if any — kept so the synthesizer and the audit can
    /// say which angle produced a finding.
    pub lens: Option<String>,
    pub content: String,
}

/// Build the full prompt for a task: role persona, then the optional skill lens hydrated with
/// the (already budgeted, D21) context — or the bare context when no skill is named.
pub(crate) fn build_prompt(
    registry: &SkillRegistry,
    task: &AgentTask,
) -> Result<String, ConceptError> {
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

    let llm = ollama.for_role(task.role.as_str());
    let content = {
        let _permit = ai_semaphore
            .acquire()
            .await
            .map_err(|_| ConceptError::SemaphoreClosed)?;
        llm.chat(vec![ChatMessage {
            role: "user".to_string(),
            content: prompt,
        }])
        .await?
    };

    // Repair only, never retry (docs/adr/0023): a retry per fan-out agent would double the
    // fan-out's model calls. A lens whose answer can't be repaired degrades to the raw text.
    let content = match task.skill.as_deref().and_then(|name| registry.get(name)) {
        Some(skill) => match contract::validate(skill.contract, &content) {
            Ok(repaired) => repaired,
            Err(violation) => {
                tracing::warn!(skill = %skill.name, %violation, "agent answer off-contract; kept raw");
                content.trim().to_string()
            }
        },
        None => content.trim().to_string(),
    };
    if content.is_empty() {
        tracing::warn!(role = task.role.as_str(), "agent returned empty output");
    }
    Ok(AgentResult {
        role: task.role,
        lens: task.skill,
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

    /// The assembly shape (persona, blank line, skill body with `{context}` filled) is pinned; the
    /// skill is a fixed test file, so editing a built-in skill never moves this golden
    /// (skills are digested, not frozen — ADR-0040).
    #[test]
    fn golden_build_prompt_fixed_role_skill_context() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("golden-lens.md"),
            "---\nname: golden-lens\ndescription: \"fixed\"\nstage: attack\n---\n\
             List what breaks first.\n\n{context}\n",
        )
        .expect("write skill");
        let (registry, issues) = SkillRegistry::load(dir.path());
        assert!(issues.is_empty(), "{issues:?}");
        let task = AgentTask {
            role: AgentRole::Critic,
            skill: Some("golden-lens".to_string()),
            context: "## Idea\nA tool library for one street.".to_string(),
        };
        crate::ai::provenance::assert_golden(
            &build_prompt(&registry, &task).expect("prompt"),
            include_str!("../../tests/fixtures/prompt-goldens/build-prompt.txt"),
            "build-prompt.txt",
        );
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

    #[test]
    fn all_lists_every_role_variant() {
        // Exhaustive: a new variant fails to compile here until it is placed in `ALL`.
        let in_all = |r: AgentRole| AgentRole::ALL.contains(&r);
        for role in AgentRole::ALL {
            match role {
                AgentRole::Critic
                | AgentRole::Researcher
                | AgentRole::Advocate
                | AgentRole::Harvester
                | AgentRole::Synthesizer
                | AgentRole::Auditor => assert!(in_all(role)),
            }
        }
        assert_eq!(AgentRole::ALL.len(), 6);
    }

    #[test]
    fn default_profiles_stay_within_the_settings_bands() {
        let profiles = default_role_profiles();
        for role in AgentRole::ALL {
            let p = &profiles[role.as_str()];
            assert!((0.0..=2.0).contains(&p.temperature), "{}", role.as_str());
            assert!(matches!(
                p.claude_effort.as_str(),
                "" | "low" | "medium" | "high"
            ));
        }
    }

    #[test]
    fn extractive_roles_default_colder_than_adversarial_ones() {
        let t = |r: AgentRole| r.default_profile().temperature;
        assert!(t(AgentRole::Harvester) < t(AgentRole::Synthesizer));
        assert!(t(AgentRole::Auditor) < t(AgentRole::Synthesizer));
        assert!(t(AgentRole::Synthesizer) < t(AgentRole::Critic));
        assert!(t(AgentRole::Synthesizer) < t(AgentRole::Advocate));
    }
}
