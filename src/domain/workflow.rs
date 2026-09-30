//! Workflow definition vocabulary (docs/06-concepts/workflows.md, ADR-0035): the frontmatter and
//! stage bodies a workflow file names. A workflow is a markdown file — built-ins ship compiled in,
//! the owner adds or overrides them under `vault/.workflows/` — so these shapes are a data
//! contract. Pure data: resolving skills and checking caps across registries is
//! `concepts::workflows::registry`'s job.
//!
//! A stage is a mapping tagged by `kind:`. It is dispatched by hand ([`parse_stage`]) rather than
//! through a serde internally-tagged enum, whose interaction with `deny_unknown_fields` is
//! unreliable — and an owner's typo must surface on the book, never fall back to a default.

use crate::domain::skill::SkillRole;

/// The kinds of stage a workflow can run (ADR-0034 adds the last four).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageKind {
    FanOut,
    Chain,
    Audit,
    Synthesize,
    Ground,
    Panel,
    Loop,
    Refine,
}

impl StageKind {
    /// Every kind, in the order the docs list them.
    pub const ALL: [StageKind; 8] = [
        StageKind::FanOut,
        StageKind::Chain,
        StageKind::Audit,
        StageKind::Synthesize,
        StageKind::Ground,
        StageKind::Panel,
        StageKind::Loop,
        StageKind::Refine,
    ];

    /// The on-disk `kind:` spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            StageKind::FanOut => "fan_out",
            StageKind::Chain => "chain",
            StageKind::Audit => "audit",
            StageKind::Synthesize => "synthesize",
            StageKind::Ground => "ground",
            StageKind::Panel => "panel",
            StageKind::Loop => "loop",
            StageKind::Refine => "refine",
        }
    }
}

impl std::str::FromStr for StageKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        StageKind::ALL
            .into_iter()
            .find(|k| k.as_str() == s)
            .ok_or_else(|| format!("unknown stage kind {s:?}"))
    }
}

/// One agent in a stage: a role, optionally through a skill lens and/or a free-text angle (the
/// angle is read only by panel proposers).
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepSpec {
    pub role: SkillRole,
    #[serde(default)]
    pub skill: Option<String>,
    #[serde(default)]
    pub angle: Option<String>,
}

/// One panel criterion (ADR-0034): a slug name, a weight and the anchors for scores 0 and 2.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CriterionSpec {
    pub name: String,
    pub weight: u8,
    pub zero: String,
    pub two: String,
}

/// `fan_out`: run these steps in parallel over the same context.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FanOutSpec {
    pub steps: Vec<StepSpec>,
}

/// `chain`: run one step alone.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChainSpec {
    pub role: SkillRole,
    #[serde(default)]
    pub skill: Option<String>,
}

fn default_readers() -> usize {
    2
}

fn default_tool_rounds() -> usize {
    2
}

/// `ground` (ADR-0034): map the attached sources and verify the readers' anchors in code.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroundSpec {
    #[serde(default = "default_readers")]
    pub readers: usize,
    #[serde(default = "default_tool_rounds")]
    pub tool_rounds: usize,
    /// One angle per reader; empty means the built-in default angles.
    #[serde(default)]
    pub angles: Vec<String>,
}

fn default_judges() -> usize {
    1
}

/// `panel` (ADR-0034): competing proposals scored on a weighted rubric.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PanelSpec {
    pub proposers: Vec<StepSpec>,
    pub criteria: Vec<CriterionSpec>,
    #[serde(default = "default_judges")]
    pub judges: usize,
}

fn default_dry_rounds() -> usize {
    1
}

fn default_loop_rounds() -> usize {
    3
}

fn default_max_calls() -> usize {
    16
}

/// `loop` (ADR-0034): rerun the steps until a round finds nothing new or a cap is hit.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoopSpec {
    pub steps: Vec<StepSpec>,
    #[serde(default = "default_dry_rounds")]
    pub dry_rounds: usize,
    #[serde(default = "default_loop_rounds")]
    pub max_rounds: usize,
    #[serde(default = "default_max_calls")]
    pub max_calls: usize,
}

fn default_refine_rounds() -> usize {
    1
}

/// `refine` (ADR-0034): rewrite the audit's REFUTED/UNCERTAIN findings and re-audit.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefineSpec {
    pub role: SkillRole,
    pub skill: String,
    #[serde(default = "default_refine_rounds")]
    pub max_rounds: usize,
}

/// The body of a stage without its `kind:` tag: `audit` and `synthesize` take no keys.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct NoFields {}

/// One parsed stage, before any skill is resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StageSpec {
    FanOut(FanOutSpec),
    Chain(ChainSpec),
    Audit,
    Synthesize,
    Ground(GroundSpec),
    Panel(PanelSpec),
    Loop(LoopSpec),
    Refine(RefineSpec),
}

impl StageSpec {
    pub fn kind(&self) -> StageKind {
        match self {
            StageSpec::FanOut(_) => StageKind::FanOut,
            StageSpec::Chain(_) => StageKind::Chain,
            StageSpec::Audit => StageKind::Audit,
            StageSpec::Synthesize => StageKind::Synthesize,
            StageSpec::Ground(_) => StageKind::Ground,
            StageSpec::Panel(_) => StageKind::Panel,
            StageSpec::Loop(_) => StageKind::Loop,
            StageSpec::Refine(_) => StageKind::Refine,
        }
    }
}

/// The structured header of a workflow file (ADR-0035). `stages` stays raw here so each stage can
/// be dispatched on its `kind:` by [`parse_stage`].
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowFrontmatter {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub use_when: String,
    #[serde(default)]
    pub avoid_when: String,
    /// Registered and runnable, but never offered as a chip.
    #[serde(default)]
    pub hidden: bool,
    pub stages: Vec<serde_norway::Value>,
}

/// Parse `stages[index]`: take its `kind:` off, then deserialize the rest into that kind's body,
/// which rejects unknown keys. The error names `stages[i] (<kind>)` so the book can point at it.
pub fn parse_stage(index: usize, value: &serde_norway::Value) -> Result<StageSpec, String> {
    let serde_norway::Value::Mapping(map) = value else {
        return Err(format!("stages[{index}]: a stage must be a mapping"));
    };
    let mut rest = map.clone();
    let kind = match rest.remove("kind") {
        Some(serde_norway::Value::String(k)) => k,
        Some(_) => return Err(format!("stages[{index}]: `kind` must be a string")),
        None => return Err(format!("stages[{index}]: missing `kind`")),
    };
    let at = |e: String| format!("stages[{index}] ({kind}): {e}");
    let kind_enum: StageKind = kind.parse().map_err(at)?;
    let rest = serde_norway::Value::Mapping(rest);
    fn body<T: serde::de::DeserializeOwned>(v: serde_norway::Value) -> Result<T, String> {
        serde_norway::from_value(v).map_err(|e| e.to_string())
    }
    let spec = match kind_enum {
        StageKind::FanOut => body(rest).map(StageSpec::FanOut),
        StageKind::Chain => body(rest).map(StageSpec::Chain),
        StageKind::Audit => body::<NoFields>(rest).map(|_| StageSpec::Audit),
        StageKind::Synthesize => body::<NoFields>(rest).map(|_| StageSpec::Synthesize),
        StageKind::Ground => body(rest).map(StageSpec::Ground),
        StageKind::Panel => body(rest).map(StageSpec::Panel),
        StageKind::Loop => body(rest).map(StageSpec::Loop),
        StageKind::Refine => body(rest).map(StageSpec::Refine),
    };
    spec.map_err(at)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stage(yaml: &str) -> Result<StageSpec, String> {
        parse_stage(0, &serde_norway::from_str(yaml).unwrap())
    }

    #[test]
    fn kinds_round_trip_their_spelling() {
        for k in StageKind::ALL {
            assert_eq!(k.as_str().parse::<StageKind>(), Ok(k));
        }
        assert!("fanout".parse::<StageKind>().is_err());
    }

    #[test]
    fn stages_dispatch_on_kind_and_apply_defaults() {
        assert_eq!(stage("kind: audit"), Ok(StageSpec::Audit));
        assert_eq!(
            stage("kind: ground"),
            Ok(StageSpec::Ground(GroundSpec {
                readers: 2,
                tool_rounds: 2,
                angles: vec![]
            }))
        );
        let Ok(StageSpec::FanOut(f)) =
            stage("kind: fan_out\nsteps:\n  - {role: critic, skill: premortem}")
        else {
            panic!("fan_out parses");
        };
        assert_eq!(f.steps[0].skill.as_deref(), Some("premortem"));
    }

    #[test]
    fn unknown_keys_kinds_and_shapes_name_the_stage() {
        let e = stage("kind: audit\nextra: 1").unwrap_err();
        assert!(e.starts_with("stages[0] (audit): "), "{e}");
        let e = stage("kind: chain\nrole: critic\nskil: x").unwrap_err();
        assert!(e.starts_with("stages[0] (chain): "), "{e}");
        let e = stage("kind: judge").unwrap_err();
        assert!(
            e.starts_with("stages[0] (judge): unknown stage kind"),
            "{e}"
        );
        assert!(stage("role: critic")
            .unwrap_err()
            .contains("missing `kind`"));
        assert!(stage("- a").unwrap_err().contains("must be a mapping"));
    }
}
