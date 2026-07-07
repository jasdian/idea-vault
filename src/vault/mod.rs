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
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("domain error: {0}")]
    Domain(#[from] crate::domain::DomainError),
}
