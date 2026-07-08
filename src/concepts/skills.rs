//! Skills: named, reusable ideation moves — parameterized prompt templates the AI can apply to
//! an idea on demand (docs/06-concepts/skills.md D18).

use std::path::Path;

use tokio::sync::Semaphore;

use crate::ai::budget::{assemble_context, AssembledContext, ContextBudget, ContextInput};
use crate::ai::ollama::ChatMessage;
use crate::ai::LlmBackend;
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
                Skill {
                    name: "constraints".to_string(),
                    description: "Map the practical constraints, prerequisites, and precedents bearing on the idea.".to_string(),
                    // TODO(skills): see docs/06-concepts/skills.md — flesh out the full
                    // constraints prompt template; {context} is filled by ai::budget (D21).
                    prompt: "Map the practical constraints, prerequisites, and relevant precedents that bear on this idea.\n{context}".to_string(),
                },
                Skill {
                    name: "second-order-effects".to_string(),
                    description: "Assume the idea works; trace the second-order and knock-on effects.".to_string(),
                    // TODO(skills): see docs/06-concepts/skills.md — flesh out the full
                    // second-order-effects prompt template; {context} is filled by ai::budget (D21).
                    prompt: "Assume this idea succeeds as stated. Trace the second-order and knock-on effects, good and bad.\n{context}".to_string(),
                },
                Skill {
                    name: "build-prompt".to_string(),
                    description: "Fold the whole discussion into a ready-to-run build prompt for a coding agent.".to_string(),
                    // The capstone move: turn the interrogation into an actionable spec another
                    // agent (e.g. Claude Code) can execute. Output is one copy-pasteable prompt.
                    prompt: "Synthesize the ENTIRE discussion below into a single, self-contained BUILD PROMPT that a coding agent (such as Claude Code) can execute to actually build this idea.\n\nReturn ONLY the prompt itself, wrapped in one fenced ```markdown code block, ready to copy and paste. The prompt must:\n- Open with the goal and the concrete deliverable in the first sentence.\n- Fold in what the discussion SETTLED — the decisions, constraints, and disproofs — rather than restating the chat; extract, don't transcribe.\n- Lay out an ordered plan: understand → design → implement → verify.\n- Say explicitly where the agent should fan out parallel subagents or a workflow (independent modules, multi-angle review) versus work sequentially, and why.\n- State the acceptance criteria and how to verify them.\nWrite it as direct instructions to the agent, specific and imperative — not prose about the idea.\n{context}".to_string(),
                },
                // The `extract-*` lenses below are the knowledge-extraction angles
                // (docs/adr/0015): orchestrator-only, hidden from the moves chip row via
                // `move_names`. Each harvests exactly one category of durable knowledge from
                // the discussion; outputting nothing when the category is empty is correct.
                Skill {
                    name: "extract-key-decisions".to_string(),
                    description: "Harvest the decisions the discussion actually settled.".to_string(),
                    prompt: "From the discussion below, harvest ONLY the key decisions that were actually settled — choices made, directions committed to, options explicitly rejected. As markdown bullets, one decision per bullet, each with the deciding rationale in one clause. Do not critique, do not add new ideas. If the discussion settled no decisions, output nothing.\n{context}".to_string(),
                },
                Skill {
                    name: "extract-durable-facts".to_string(),
                    description: "Harvest durable facts and evidence established in the discussion.".to_string(),
                    prompt: "From the discussion below, harvest ONLY the durable facts and evidence that were established — numbers, constraints found true, precedents cited, conclusions grounded in reasoning. As markdown bullets, one fact per bullet. Exclude speculation and opinions. If the discussion established no durable facts, output nothing.\n{context}".to_string(),
                },
                Skill {
                    name: "extract-open-questions".to_string(),
                    description: "Harvest the questions the discussion raised but did not resolve.".to_string(),
                    prompt: "From the discussion below, harvest ONLY the open questions — raised but unresolved threads, known unknowns, disagreements left standing. As markdown bullets, one question per bullet, phrased as a question. If nothing was left open, output nothing.\n{context}".to_string(),
                },
                Skill {
                    name: "extract-risks-assumptions".to_string(),
                    description: "Harvest the risks and load-bearing assumptions the discussion surfaced.".to_string(),
                    prompt: "From the discussion below, harvest ONLY the risks and load-bearing assumptions that were surfaced — what the idea silently depends on, what could sink it. As markdown bullets, one item per bullet, marked either `risk:` or `assumption:`. If none were surfaced, output nothing.\n{context}".to_string(),
                },
                Skill {
                    name: "extract-next-actions".to_string(),
                    description: "Harvest the concrete next actions the discussion pointed to.".to_string(),
                    prompt: "From the discussion below, harvest ONLY the concrete next actions the discussion pointed to — experiments to run, people to ask, things to build or measure. As markdown bullets, one action per bullet, imperative form. If the discussion pointed to no actions, output nothing.\n{context}".to_string(),
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

    /// Skills surfaced as interactive move chips in the discussion UI. The `extract-*` lenses
    /// are knowledge-extraction angles driven by `concepts::knowledge` (docs/adr/0015), not
    /// standalone moves — they are registered (so `run_agent` can resolve them) but excluded
    /// here.
    pub fn move_names(&self) -> Vec<String> {
        self.skills
            .iter()
            .filter(|s| !s.name.starts_with("extract-"))
            .map(|s| s.name.clone())
            .collect()
    }
}

/// Gather the D18 skill inputs (`idea_body`, `memory`, `recent_conversation`) via `vault::store`
/// and assemble them under `budget` with `ai::budget` directly — per D4, `concepts` composes
/// `vault` + `ai` itself rather than reaching through `memory` (whose `load_context` is the
/// D13 reopen path; the gathering logic is intentionally parallel, not shared).
/// `pub(crate)`: `swarm` hydrates the same budgeted block once per fan-out (D14/D21).
pub(crate) fn hydrate_context(
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
    let compacted = store::read_compacted(vault_dir, idea_slug)?;
    let win = crate::memory::compact::effective_window(&turns, compacted.as_ref());
    let (summary, tail): (Option<&str>, &[String]) = match win.applied {
        Some(k) => (compacted.as_ref().map(|c| c.summary.as_str()), &turns[k..]),
        None => (None, &turns[..]),
    };
    Ok(assemble_context(
        budget,
        ContextInput {
            idea_body: &idea.body,
            memory: &memory,
            summary,
            turns: tail,
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
    ollama: &LlmBackend,
    ai_semaphore: &Semaphore,
    vault_dir: &Path,
    idea_slug: &str,
    skill: &Skill,
    budget: ContextBudget,
    progress: &(dyn Fn(&str) + Sync),
) -> Result<String, ConceptError> {
    progress(&format!("running {}", skill.name));
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
        // append_turn owns the heading grammar and escapes any embedded "## " lines the model
        // may emit, so its output can never forge a turn boundary.
        store::append_turn(
            vault_dir,
            idea_slug,
            &format!("assistant (skill: {})", skill.name),
            &output,
        )?;
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

    #[test]
    fn move_names_excludes_extraction_lenses_but_they_stay_resolvable() {
        let registry = SkillRegistry::builtin();
        let moves = registry.move_names();
        assert!(moves.iter().any(|n| n == "premortem"));
        assert!(
            !moves.iter().any(|n| n.starts_with("extract-")),
            "extraction lenses must not appear as move chips: {moves:?}"
        );
        // Still registered — the knowledge orchestrator resolves them like any skill.
        for lens in crate::concepts::knowledge::LENSES {
            assert!(registry.get(lens).is_some(), "unregistered lens: {lens}");
        }
    }
}
