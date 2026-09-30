//! Skill definition vocabulary (docs/06-concepts/skills.md, docs/adr/0022): the enums a skill
//! file's frontmatter names. A skill is a markdown file — built-ins ship compiled in, the owner
//! adds or overrides them under `vault/.skills/` — so these values are a data contract.

/// Where a skill sits on the ideation spine (docs/06-concepts/skills.md "The spine"): the order
/// an idea is best run through, used to group the skill book and to suggest the next move.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillStage {
    /// Make the strongest honest case for the idea before attacking it.
    Steelman,
    /// Try to break it: failure causes, disproofs, hostile arguments.
    Attack,
    /// Ground it: constraints, precedents, knock-on effects, size.
    Consequence,
    /// Fold the findings into one position.
    Converge,
    /// Turn a settled idea into something actionable (e.g. a build prompt).
    Capstone,
    /// Orchestrator-only knowledge-harvest lenses (docs/adr/0015).
    Extract,
}

impl SkillStage {
    /// The owner-facing spine, in the order an idea should travel it. `Extract` is off-spine:
    /// its lenses are harvest angles for `concepts::knowledge`, never a move the owner picks.
    pub const SPINE: [SkillStage; 5] = [
        SkillStage::Steelman,
        SkillStage::Attack,
        SkillStage::Consequence,
        SkillStage::Converge,
        SkillStage::Capstone,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            SkillStage::Steelman => "steelman",
            SkillStage::Attack => "attack",
            SkillStage::Consequence => "consequence",
            SkillStage::Converge => "converge",
            SkillStage::Capstone => "capstone",
            SkillStage::Extract => "extract",
        }
    }
}

/// The agent persona a skill runs under when an orchestrator (swarm, workflow, extraction)
/// fans it out — maps 1:1 onto `concepts::agents::AgentRole`. A direct interactive skill run
/// sends the bare skill prompt and ignores this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillRole {
    #[default]
    Critic,
    Researcher,
    Advocate,
    Harvester,
    Synthesizer,
}

/// The shape a skill's output must take (docs/adr/0023). `ai::contract` validates it and, for a
/// single interactive call, asks the model once more when the shape is violated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputContract {
    /// Any non-empty text.
    #[default]
    Free,
    /// Markdown bullets, or nothing at all (a harvest lens with nothing to harvest).
    BulletsOrEmpty,
    /// A numbered list, most important first.
    RankedList,
    /// Exactly one fenced ```` ```markdown ```` block — the copy-pasteable deliverable.
    FencedMarkdown,
    /// A sectioned build plan (docs/adr/0030): `## Goal`, `## Settled`, `## Verify first`,
    /// `## Open questions`, `## Plan`, `## Kill criteria`. Checked for shape here; its claims are
    /// gated by `concepts::build_plan`.
    BuildPlan,
    /// A Ground reader's anchored claims (ADR-0034): at most eight ``- `path:N[-M]` | `symbol` |
    /// claim`` lines. Checked for shape here; every anchor is then verified in code.
    GroundClaims,
    /// A Panel proposal (ADR-0034): a `## Proposal` heading over at most eight bullets.
    Proposal,
    /// A Panel scorer's rubric answer (ADR-0034): one `C<i>: <0|1|2> — reason` line per
    /// criterion.
    Scorecard,
}
