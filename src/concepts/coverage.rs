//! Spine coverage (docs/06-concepts/skills.md "The spine", docs/adr/0022): which stages of the
//! ideation spine — steelman → attack → consequence → converge → capstone — an idea's discussion
//! has actually been through, the move to suggest next, and the soft "wrong turn" warnings.
//!
//! Derived purely from `conversation.md`'s turn headings plus the skill registry: nothing new is
//! persisted, so the markdown stays the only truth and a deleted turn simply uncovers its stage.
//! Warnings never block anything — they are advice, like a skill book's "common wrong turns".

use crate::concepts::skills::SkillRegistry;
use crate::concepts::{swarm, workflows};
use crate::domain::SkillStage;
use crate::vault::store::{self, TurnSource};

/// A run of the same move this long, back to back, earns a nudge toward another lens.
const REPEAT_WARNING_RUN: usize = 3;

/// What to suggest next: a skill move, or the swarm when only convergence is missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NextMove {
    Skill { name: String, why: String },
    Swarm,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coverage {
    /// Every spine stage, in order, with whether the discussion has covered it.
    pub stages: Vec<(SkillStage, bool)>,
    pub next: Option<NextMove>,
    pub warnings: Vec<String>,
    /// No attack-stage move has run although the foil has answered — storing now would keep an
    /// idea nobody tried to break.
    pub untested: bool,
}

/// The spine stages one assistant turn covers.
fn stages_of(source: &TurnSource, registry: &SkillRegistry) -> Vec<SkillStage> {
    let skill_stage = |name: &str| registry.get(name).map(|s| s.stage);
    match source {
        TurnSource::Skill(name) => skill_stage(name).into_iter().collect(),
        TurnSource::Swarm(angles) => {
            let named: Vec<&str> = if angles.is_empty() {
                swarm::DEFAULT_ANGLES.to_vec()
            } else {
                angles.iter().map(String::as_str).collect()
            };
            named
                .into_iter()
                .filter_map(skill_stage)
                .chain([SkillStage::Converge])
                .collect()
        }
        TurnSource::Workflow(name) => match workflows::get_workflow(name) {
            Some(wf) => wf
                .stages
                .iter()
                .flat_map(|stage| {
                    let converges = matches!(stage, workflows::Stage::Synthesize);
                    stage
                        .steps()
                        .iter()
                        .filter_map(|s| s.skill.and_then(skill_stage))
                        .chain(converges.then_some(SkillStage::Converge))
                        .collect::<Vec<_>>()
                })
                .collect(),
            None => Vec::new(),
        },
        TurnSource::Knowledge => vec![SkillStage::Converge],
        TurnSource::User | TurnSource::Chat | TurnSource::Other(_) => Vec::new(),
    }
}

/// Coverage of `conversation` against the spine, with the next move and any warnings.
pub fn coverage(conversation: &str, registry: &SkillRegistry) -> Coverage {
    let turns = store::split_turns(conversation);
    let sources: Vec<TurnSource> = turns
        .iter()
        .map(|t| store::parse_turn_heading(store::turn_role(t)))
        .collect();
    let per_turn: Vec<Vec<SkillStage>> = sources.iter().map(|s| stages_of(s, registry)).collect();
    let covered = |stage: SkillStage| per_turn.iter().any(|s| s.contains(&stage));
    let first_turn_with =
        |stage: SkillStage| per_turn.iter().position(|stages| stages.contains(&stage));

    let stages: Vec<(SkillStage, bool)> = SkillStage::SPINE
        .iter()
        .map(|&stage| (stage, covered(stage)))
        .collect();

    // Next: the earliest uncovered stage that has a move to offer. Converge has no single-skill
    // move, so the swarm stands in for it; the capstone is only suggested once the rest is done.
    let next = stages
        .iter()
        .find(|(_, done)| !done)
        .and_then(|(stage, _)| {
            if *stage == SkillStage::Converge {
                return Some(NextMove::Swarm);
            }
            registry
                .visible()
                .find(|s| s.stage == *stage)
                .map(|s| NextMove::Skill {
                    name: s.name.clone(),
                    why: s.use_when.clone(),
                })
        });

    let mut warnings = Vec::new();
    if let Some(capstone) = first_turn_with(SkillStage::Capstone) {
        let attacked_before = per_turn[..capstone]
            .iter()
            .any(|s| s.contains(&SkillStage::Attack));
        if !attacked_before {
            warnings.push(
                "A build prompt was generated before any attack move ran — what it builds \
                 hasn't been stress-tested."
                    .to_string(),
            );
        }
    }
    // The same move back to back, ignoring the owner's turns in between.
    let moves: Vec<&str> = sources
        .iter()
        .filter_map(|s| match s {
            TurnSource::Skill(name) => Some(name.as_str()),
            TurnSource::User => None,
            _ => Some(""),
        })
        .collect();
    if let Some(last) = moves.last().filter(|m| !m.is_empty()) {
        let run = moves.iter().rev().take_while(|m| *m == last).count();
        if run >= REPEAT_WARNING_RUN {
            warnings.push(format!(
                "{last} has run {run} times in a row — a different lens will find more."
            ));
        }
    }

    let answered = sources
        .iter()
        .any(|s| !matches!(s, TurnSource::User | TurnSource::Other(_)));
    Coverage {
        untested: answered && !covered(SkillStage::Attack),
        stages,
        next,
        warnings,
    }
}

/// The skill book in one short block for the chat foil's prompt: every visible move with when to
/// use it, so the foil can recommend a move by name. Capped at `max_bytes`, whole lines only.
pub fn skill_book(registry: &SkillRegistry, max_bytes: usize) -> String {
    let mut out = String::from(
        "## Moves the owner can run\nWhen the discussion would benefit, suggest one by name:\n",
    );
    for skill in registry.visible() {
        let line = if skill.use_when.is_empty() {
            format!("- {} — {}\n", skill.name, skill.description)
        } else {
            format!("- {} — {}\n", skill.name, skill.use_when)
        };
        if out.len() + line.len() > max_bytes {
            break;
        }
        out.push_str(&line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn convo(headings: &[&str]) -> String {
        headings.iter().map(|h| format!("## {h}\nbody\n")).collect()
    }

    fn covered(c: &Coverage) -> Vec<&'static str> {
        c.stages
            .iter()
            .filter(|(_, done)| *done)
            .map(|(s, _)| s.as_str())
            .collect()
    }

    #[test]
    fn a_fresh_discussion_suggests_the_steelman_and_warns_nothing() {
        let registry = SkillRegistry::builtin();
        let c = coverage(&convo(&["user", "assistant"]), &registry);
        assert!(covered(&c).is_empty());
        assert!(matches!(&c.next, Some(NextMove::Skill { name, .. }) if name == "steelman"));
        assert!(c.warnings.is_empty());
        assert!(
            c.untested,
            "the foil answered but nothing attacked the idea"
        );
    }

    #[test]
    fn skills_swarms_and_workflows_cover_their_stages() {
        let registry = SkillRegistry::builtin();
        let c = coverage(
            &convo(&[
                "user",
                "assistant (skill: steelman)",
                "assistant (swarm: premortem, constraints)",
            ]),
            &registry,
        );
        assert_eq!(
            covered(&c),
            ["steelman", "attack", "consequence", "converge"]
        );
        assert!(matches!(&c.next, Some(NextMove::Skill { name, .. }) if name == "build-prompt"));
        assert!(!c.untested);

        let legacy = coverage(&convo(&["assistant (swarm)"]), &registry);
        assert_eq!(covered(&legacy), ["attack", "consequence", "converge"]);

        let wf = coverage(&convo(&["assistant (workflow: ready-to-build)"]), &registry);
        assert_eq!(covered(&wf), ["capstone"], "extract lenses are off-spine");
    }

    #[test]
    fn converge_is_suggested_through_the_swarm() {
        let registry = SkillRegistry::builtin();
        let c = coverage(
            &convo(&[
                "assistant (skill: steelman)",
                "assistant (skill: premortem)",
                "assistant (skill: constraints)",
            ]),
            &registry,
        );
        assert_eq!(c.next, Some(NextMove::Swarm));
    }

    #[test]
    fn a_build_prompt_before_any_attack_is_a_wrong_turn() {
        let registry = SkillRegistry::builtin();
        let early = coverage(
            &convo(&[
                "assistant (skill: build-prompt)",
                "assistant (skill: premortem)",
            ]),
            &registry,
        );
        assert_eq!(early.warnings.len(), 1);
        assert!(early.warnings[0].contains("before any attack"));
        let fine = coverage(
            &convo(&[
                "assistant (skill: premortem)",
                "assistant (skill: build-prompt)",
            ]),
            &registry,
        );
        assert!(fine.warnings.is_empty());
    }

    #[test]
    fn the_same_move_three_times_in_a_row_earns_a_nudge() {
        let registry = SkillRegistry::builtin();
        let c = coverage(
            &convo(&[
                "assistant (skill: premortem)",
                "user",
                "assistant (skill: premortem)",
                "assistant (skill: premortem)",
            ]),
            &registry,
        );
        assert!(c
            .warnings
            .iter()
            .any(|w| w.starts_with("premortem has run 3 times")));
        let broken = coverage(
            &convo(&[
                "assistant (skill: premortem)",
                "assistant",
                "assistant (skill: premortem)",
                "assistant (skill: premortem)",
            ]),
            &registry,
        );
        assert!(broken.warnings.is_empty(), "a chat reply breaks the run");
    }

    #[test]
    fn skill_book_lists_visible_moves_within_the_cap() {
        let registry = SkillRegistry::builtin();
        let book = skill_book(&registry, 4096);
        assert!(book.contains("- premortem — "));
        assert!(!book.contains("extract-"));
        let tiny = skill_book(&registry, 120);
        assert!(tiny.len() <= 120);
    }
}
