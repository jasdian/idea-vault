//! In-memory representations of a vault idea's `memory/*.md` facts and the `MEMORY.md` index
//! that points at them (docs/03-data-model.md D7, docs/06-concepts/memory.md).

use crate::domain::frontmatter::MemoryFactFrontmatter;

/// One parsed `memory/<fact-slug>.md` file: frontmatter plus the durable-conclusion body.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryFact {
    pub frontmatter: MemoryFactFrontmatter,
    pub body: String,
}

/// One line of the per-idea `MEMORY.md` index: a pointer to a memory fact plus a short summary.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryIndexEntry {
    pub slug: String,
    pub summary: String,
}

/// The parsed `MEMORY.md` index for an idea: one entry per `memory/*.md` fact.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryIndex {
    pub entries: Vec<MemoryIndexEntry>,
}
