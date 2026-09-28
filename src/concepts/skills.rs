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
                    // Klein's premortem, carried through to a patch: causes alone are a list of
                    // worries, so each one gets a warning sign + mitigation, and the move ends by
                    // restating the idea defended against the worst of them.
                    prompt: "The idea below failed badly 12 months from now. Work backwards from that failure.\n\n1. **The failure** — one sentence on what failure concretely looked like for THIS idea (not a generic \"it didn't take off\").\n2. **Causes** — the 5 most likely causes, ranked by probability × impact, most dangerous first. For each give:\n   - the cause, specific to this idea (a reason that would sink any idea is not an answer);\n   - the early warning sign that would show it happening while there is still time to act;\n   - the cheapest mitigation.\n3. **Patched idea** — restate the idea in one short paragraph, changed to defend against the top two causes. If a cause is fatal and cannot be patched, say so plainly instead.\n\nGround the causes in what the discussion below actually says; skip causes it has already convincingly resolved.\n{context}".to_string(),
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
                    description: "Say where the idea is genuinely wrong — committed dissent with confidence, not scripted objections.".to_string(),
                    // Authentic dissent, not role-play: an assigned devil's advocate mostly makes
                    // the owner rehearse rebuttals and leave MORE confident (Nemeth 2001). So the
                    // model must commit to objections it holds, state confidence, and name what
                    // would change its mind. The name stays for URL/transcript stability.
                    prompt: "Give your honest, committed dissent on the idea below: where do YOU actually think it is wrong? This is not a debate exercise. Do not manufacture objections you do not believe — scripted objections are easy to rebut and only make the owner more confident in a weak idea. Real disagreement is what changes minds.\n\n- List at most 5 objections you genuinely hold, strongest first. For each give: the objection, argued as persuasively and specifically as you can; your confidence that it is right (low / medium / high); and what evidence would change your mind.\n- If you believe the idea is fundamentally sound, say so plainly and give only the single weakest point you would still attack.\n- End with a one-line verdict: would you pursue this idea as it stands? (yes / no / only if …)\n\nAttack the strongest version of the idea as the discussion below has developed it, not a strawman of the first draft.\n{context}".to_string(),
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
                // The structured-dissent protocols below come from the same research as the
                // devils-advocate rewrite: role-played objections bolster the original view, so
                // each of these forces a concrete artifact (a press release, a rival plan, a
                // named contradiction) the idea has to survive, rather than a list of worries.
                Skill {
                    name: "pr-faq".to_string(),
                    description: "Work backwards from launch: write the press release and the hardest FAQ, exposing what can't be stated concretely.".to_string(),
                    // Amazon's working-backwards PR/FAQ. The payoff is section 3: the claims that
                    // resisted being written concretely are the idea's soft spots.
                    prompt: "Work backwards from launch day, Amazon PR/FAQ style, for the idea below.\n\n1. **Press release** (under 200 words), dated launch day: a headline; who the customer is; their problem in their own words; the solution and why it beats what they do today; one customer quote. Be concrete enough that a reader could say \"that's not me\" — vague is failure.\n2. **FAQ** — the 6 hardest questions a skeptical customer, investor, or engineer would ask, each with an honest answer. Where the honest answer is \"we don't know yet\", write that and name what would find out.\n3. **What the press release exposed** — the claims you could not write concretely. These are the idea's weakest points.\n{context}".to_string(),
                },
                Skill {
                    name: "dialectical-inquiry".to_string(),
                    description: "Build the strongest rival plan on the opposite assumptions, then weigh the two head to head.".to_string(),
                    // Mason's dialectical inquiry: dissent as a competing plan, not objections —
                    // the owner has to beat a real alternative instead of rebutting critiques.
                    prompt: "Apply dialectical inquiry to the idea below. Do not list objections — build a rival.\n\n1. **Assumptions** — the 3 to 5 load-bearing assumptions the idea rests on.\n2. **Counter-plan** — negate the most important of those assumptions and build the strongest alternative plan that pursues the same underlying goal on the opposite assumptions. Make it a plan someone could genuinely believe in, not a strawman.\n3. **Head to head** — for each assumption, which plan does the evidence currently favour, and what observation would settle it?\n4. **Synthesis** — what the idea should keep, drop, or steal from the counter-plan.\n{context}".to_string(),
                },
                Skill {
                    name: "triz".to_string(),
                    description: "Name the idea's core contradiction and resolve it without compromise, using TRIZ principles.".to_string(),
                    // TRIZ contradiction resolution, reduced to the separation principles a local
                    // model can apply without the full 40-principle matrix.
                    prompt: "Apply TRIZ contradiction analysis to the idea below.\n\n1. **Core contradiction** — the central conflict the idea must resolve: improving X makes Y worse, or the idea needs something to be both A and not-A. Name it in one sentence. If there are several, pick the one that most limits the idea.\n2. **Ideal final result** — describe the outcome where the benefit arrives with none of the cost.\n3. **Resolutions** — at least 3 ways to resolve the contradiction WITHOUT a compromise or trade-off, each using a different principle: separate in time, separate in space, separate by condition or scale, use a resource already present, invert the approach, or segment it. Name the principle for each.\n4. **Best bet** — which resolution to try first, and why.\n{context}".to_string(),
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
    pub fn moves(&self) -> impl Iterator<Item = &Skill> {
        self.skills
            .iter()
            .filter(|s| !s.name.starts_with("extract-"))
    }

    /// Names of [`Self::moves`], in registration order.
    pub fn move_names(&self) -> Vec<String> {
        self.moves().map(|s| s.name.clone()).collect()
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
    facts.sort_by_key(|b| std::cmp::Reverse(b.frontmatter.created));
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

    #[test]
    fn structured_dissent_skills_are_moves_but_not_default_swarm_angles() {
        let registry = SkillRegistry::builtin();
        let moves = registry.move_names();
        for name in ["pr-faq", "dialectical-inquiry", "triz"] {
            assert!(moves.iter().any(|n| n == name), "missing move: {name}");
            // Opt-in via the swarm angle picker; the canonical four stay the default.
            assert!(!crate::concepts::swarm::DEFAULT_ANGLES.contains(&name));
        }
    }

    #[test]
    fn every_skill_has_one_context_slot_and_a_description() {
        for skill in SkillRegistry::builtin().list() {
            assert_eq!(
                skill.prompt.matches("{context}").count(),
                1,
                "{} must have exactly one {{context}} slot",
                skill.name
            );
            assert!(
                !skill.description.is_empty(),
                "{} has no description",
                skill.name
            );
            // Names are URL path segments (`/idea/:slug/skill/:name`) and transcript labels.
            assert!(
                skill
                    .name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "{} is not a lower-kebab name",
                skill.name
            );
        }
    }
}
