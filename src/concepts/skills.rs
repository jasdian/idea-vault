//! Skills: named, reusable ideation moves — parameterized prompt templates the AI can apply to
//! an idea on demand (docs/06-concepts/skills.md D18).

use crate::concepts::ConceptError;

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

/// Hydrate a skill's `{context}` slot and run it against the AI, appending the result as an
/// assistant turn (docs/06-concepts/skills.md §D18).
///
/// TODO(skills): see docs/06-concepts/skills.md §D18 — fill `{context}` via `ai::budget` under
/// budget, call `ai::ollama` (streamed, under the shared semaphore), then append the result to
/// `conversation.md` via `vault::store`.
pub fn invoke(_skill: &Skill, _context: &str) -> Result<String, ConceptError> {
    Err(ConceptError::NotImplemented("concepts::skills::invoke"))
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
