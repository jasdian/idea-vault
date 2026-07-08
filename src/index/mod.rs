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

/// Private-Use-Area sentinel codepoints [`queries::search`] asks `snippet()` to wrap match spans
/// in, instead of HTML markup. Rationale: `search_fts.content` is owner-authored plain text that
/// the web layer must HTML-escape before display (never trust it as markup); escaping first and
/// *then* turning these two sentinels into `<mark>...</mark>` lets a snippet be both safe and
/// highlighted. U+E000/U+E001 sit in the Private Use Area, which no real document text will ever
/// contain (no Unicode block assigns them a meaning) — but "never" is a claim about well-behaved
/// input, not adversarial or binary-garbage input, so [`reindex::reindex`] defensively strips any
/// occurrence of these two codepoints from every string it indexes. That keeps the contract exact
/// rather than merely probabilistic: any sentinel byte found in a rendered snippet came from
/// `snippet()` marking a match, never from the vault.
pub(crate) const SNIPPET_MATCH_OPEN: char = '\u{E000}';
pub(crate) const SNIPPET_MATCH_CLOSE: char = '\u{E001}';

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
