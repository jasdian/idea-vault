//! Domain types: the vault's markdown-frontmatter contract (docs/03-data-model.md D8) and the
//! idea lifecycle state machine (docs/04-state-machine.md D9). No I/O here — see `vault` for
//! reading/writing files on disk.

pub mod frontmatter;
pub mod idea;
pub mod links;
pub mod memory;
pub mod slug;

pub use frontmatter::{IdeaFrontmatter, MemoryFactFrontmatter};
pub use idea::{Idea, IdeaState};
pub use memory::{MemoryFact, MemoryIndex};

/// Errors produced while parsing/validating domain data (frontmatter + state).
#[derive(Debug, thiserror::Error)]
pub enum DomainError {
    #[error("missing or malformed frontmatter fence")]
    MissingFrontmatter,
    #[error("invalid idea state: {0}")]
    InvalidState(String),
    #[error("yaml error: {0}")]
    Yaml(#[from] serde_norway::Error),
}
