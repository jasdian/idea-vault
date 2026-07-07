//! `vault` — the only module that reads/writes the markdown files that are the source of truth
//! for idea-vault (docs/02-module-reference.md, docs/03-data-model.md). Owns the on-disk file
//! contract: `vault/<slug>/{idea.md, conversation.md, memory/*.md, MEMORY.md}`.
//!
//! Dependency rule (docs/02-module-reference.md D4): `vault` may depend only on `domain` — never
//! on `index`, `ai`, `memory`, `concepts`, or `web`.

pub mod store;
pub mod walk;

pub use store::ensure_vault_dir;

/// Errors produced by vault I/O and the vault/domain boundary.
#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    #[error("not yet implemented: {0}")]
    NotImplemented(&'static str),
    /// No `vault/<slug>/` idea on disk (distinct from a transport-level IO failure so `web` can
    /// answer 404 rather than 500).
    #[error("idea not found: {0}")]
    IdeaNotFound(String),
    /// A slug that fails `domain::slug::is_valid` — rejected before any path join so a malformed
    /// or hostile slug (`../`, separators) can never escape the vault directory.
    #[error("invalid slug: {0:?}")]
    InvalidSlug(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("domain error: {0}")]
    Domain(#[from] crate::domain::DomainError),
}
