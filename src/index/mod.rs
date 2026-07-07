//! `index` — the derived, rebuildable SQLite index over `vault/**`.
//!
//! Per [ADR-0002](../../docs/adr/0002-markdown-source-of-truth-sqlite-index.md) the entire
//! database is a *derived* artifact: markdown under `vault/` is the source of truth, and
//! `index.db` holds only search/tags/backlinks that can be reconstructed from disk alone.
//! There are therefore **no schema migrations** — recovery from corruption or a schema change
//! is `delete index.db + reindex`, never a migration.
//!
//! Dependency direction (docs/02 D4): `index` depends only on `vault` and `domain`.

pub mod queries;
pub mod reindex;
pub mod schema;

use thiserror::Error;

/// Errors surfaced by the derived-index module.
#[derive(Debug, Error)]
pub enum IndexError {
    /// A scaffolded entry point that is not wired up yet.
    #[error("not implemented: {0}")]
    NotImplemented(&'static str),

    /// An error from the underlying SQLite driver.
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// An error reading the vault while (re)building the index.
    #[error("vault error: {0}")]
    Vault(#[from] crate::vault::VaultError),

    /// A filesystem error (e.g. creating the index's parent directory).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
