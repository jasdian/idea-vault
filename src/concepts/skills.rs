//! Skills: named, reusable ideation moves — parameterized prompt templates the AI can apply to
//! an idea on demand (docs/06-concepts/skills.md D18).

use std::path::Path;

use tokio::sync::Semaphore;

use crate::ai::budget::{assemble_context, AssembledContext, ContextBudget, ContextInput};
use crate::ai::ollama::{ChatMessage, OllamaClient};
use crate::concepts::ConceptError;
use crate::vault::store;

/// A skill is data, not code: a name, a description, and a prompt template with a `{context}`
/// slot filled by `ai::budget` at invocation time.
#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub prompt: String,
}

/// The set of skills available at runtime, populated at boot with the built-ins.
pub struct SkillRegistry {
    skills: Vec<Skill>,
}

impl SkillRegistry {
    /// The built-in skills that ship with the binary (docs/06-concepts/skills.md).
    pub fn builtin() -> Self {
        Self {
            skills: vec![
                Skill {
                    name: "premortem".to_string(),
                    description: "Assume the idea failed; enumerate the most likely causes."
                        .to_string(),
                    // TODO(skills): see docs/06-concepts/skills.md — flesh out the full
                    // premortem prompt template; {context} is filled by ai::budget (D21).
                    prompt: "The idea below failed badly 12 months from now. Working backwards, list the most likely causes, ranked by probability × impact.\n{context}".to_string(),
                },
                Skill {
                    name: "cheapest-disproof".to_string(),
                    description: "Find the fastest, cheapest experiment that could disprove the idea.".to_string(),
                    // TODO(skills): see docs/06-concepts/skills.md — flesh out the full
                    // cheapest-disproof prompt template; {context} is filled by ai::budget (D21).
                    prompt: "What is the cheapest, fastest test that could disprove this idea?\n{context}".to_string(),
                },
                Skill {
                    name: "devils-advocate".to_string(),
                    description: "Argue against the idea as persuasively as possible.".to_string(),
                    // TODO(skills): see docs/06-concepts/skills.md — flesh out the full
                    // devils-advocate prompt template; {context} is filled by ai::budget (D21).
                    prompt: "Argue against this idea as persuasively as you can.\n{context}".to_string(),
                },
            ],
        }
    }

    /// Look up a skill by exact name.
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.name == name)
    }

    /// All registered skills, in registration order.
    pub fn list(&self) -> &[Skill] {
        &self.skills
    }
}

/// Gather the D18 skill inputs (`idea_body`, `memory`, `recent_conversation`) via `vault::store`
/// and assemble them under `budget` with `ai::budget` directly — per D4, `concepts` composes
/// `vault` + `ai` itself rather than reaching through `memory` (whose `load_context` is the
/// D13 reopen path; the gathering logic is intentionally parallel, not shared).
fn hydrate_context(
    vault_dir: &Path,
    idea_slug: &str,
    budget: ContextBudget,
) -> Result<AssembledContext, ConceptError> {
    let idea = store::read_idea(vault_dir, idea_slug)?;
    let conversation = store::read_conversation(vault_dir, idea_slug)?;

    let index = store::read_memory_index(vault_dir, idea_slug)?;
    let mut memory: Vec<String> = index
        .entries
        .iter()
        .map(|e| format!("[[{}]] — {}", e.slug, e.summary))
        .collect();
    let mut facts = store::read_memory_facts(vault_dir, idea_slug)?;
    facts.sort_by(|a, b| b.frontmatter.created.cmp(&a.frontmatter.created));
    memory.extend(
        facts
            .iter()
            .map(|f| format!("{}: {}", f.frontmatter.title, f.body.trim())),
    );

    let turns = store::split_turns(&conversation);
    Ok(assemble_context(
        budget,
        ContextInput {
            idea_body: &idea.body,
            memory: &memory,
            turns: &turns,
        },
    ))
}

/// Hydrate a skill's `{context}` slot and run it against the AI, appending the result as an
/// assistant turn (docs/06-concepts/skills.md §D18).
///
/// The `{context}` slot is filled by `ai::budget` (idea body + memory + recent conversation,
/// under `budget` — never the raw full history). The Ollama call is gated by the process-wide
/// `ai_semaphore` (ADR-0006: chat, skills, and swarm share one bound). Callers must NOT already
/// hold a permit from that semaphore when calling this — `invoke` acquires its own, and a held
/// permit plus a small configured bound would deadlock. Stateless: the output is appended as an
/// assistant turn only after the call completes (nothing partial ever reaches
/// `conversation.md`); idea state is not changed.
pub async fn invoke(
    ollama: &OllamaClient,
    ai_semaphore: &Semaphore,
    vault_dir: &Path,
    idea_slug: &str,
    skill: &Skill,
    budget: ContextBudget,
) -> Result<String, ConceptError> {
    let context = hydrate_context(vault_dir, idea_slug, budget)?;
    let prompt = skill.prompt.replace("{context}", &context.text);

    let output = {
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
        // permit released here — before the vault write, which needs no AI slot
    };

    let output = output.trim().to_string();
    if output.is_empty() {
        // A "successful" call with nothing to say is usually a model misfire — surface it
        // rather than silently appending nothing (D24: surface, not swallow).
        tracing::warn!(skill = %skill.name, idea_slug, "skill invocation returned empty output");
    } else {
        let turn = format!("## assistant (skill: {})\n{}\n", skill.name, output);
        store::append_conversation(vault_dir, idea_slug, &turn)?;
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_registry_contains_premortem_and_get_finds_it() {
        let registry = SkillRegistry::builtin();
        assert!(registry.list().iter().any(|s| s.name == "premortem"));
        let found = registry
            .get("premortem")
            .expect("premortem should be registered");
        assert_eq!(found.name, "premortem");
    }
}
