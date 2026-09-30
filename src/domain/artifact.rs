//! In-memory representation of a vault idea's `artifacts/*.md` files — persisted
//! knowledge-extraction outputs (docs/adr/0015). A `.md` artifact is truth (frontmatter + body);
//! the optional `.html` report sibling is a derived export and has no domain type.

use crate::domain::frontmatter::ArtifactFrontmatter;

/// What an artifact file holds: one lens's findings, the converged synthesis of a run, or the
/// facts a store's evidence gate held back from memory (docs/adr/0023), or a gated build plan
/// (docs/adr/0030), or one of a workflow run's stage artifacts (docs/adr/0034): a Ground stage's
/// verified code map, a Panel stage's scorecard, and the run record listing every stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Finding,
    Synthesis,
    Quarantine,
    BuildPlan,
    GroundMap,
    Scorecard,
    WorkflowRun,
}

impl ArtifactKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ArtifactKind::Finding => "finding",
            ArtifactKind::Synthesis => "synthesis",
            ArtifactKind::Quarantine => "quarantine",
            ArtifactKind::BuildPlan => "build_plan",
            ArtifactKind::GroundMap => "ground_map",
            ArtifactKind::Scorecard => "scorecard",
            ArtifactKind::WorkflowRun => "workflow_run",
        }
    }
}

/// One parsed `artifacts/<file-slug>.md` file: frontmatter plus the findings body.
#[derive(Debug, Clone, PartialEq)]
pub struct Artifact {
    pub frontmatter: ArtifactFrontmatter,
    pub body: String,
}
