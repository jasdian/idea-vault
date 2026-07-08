//! In-memory representation of a vault idea's `artifacts/*.md` files — persisted
//! knowledge-extraction outputs (docs/adr/0015). A `.md` artifact is truth (frontmatter + body);
//! the optional `.html` report sibling is a derived export and has no domain type.

use crate::domain::frontmatter::ArtifactFrontmatter;

/// What an artifact file holds: one lens's findings, or the converged synthesis of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Finding,
    Synthesis,
}

impl ArtifactKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ArtifactKind::Finding => "finding",
            ArtifactKind::Synthesis => "synthesis",
        }
    }
}

/// One parsed `artifacts/<file-slug>.md` file: frontmatter plus the findings body.
#[derive(Debug, Clone, PartialEq)]
pub struct Artifact {
    pub frontmatter: ArtifactFrontmatter,
    pub body: String,
}
