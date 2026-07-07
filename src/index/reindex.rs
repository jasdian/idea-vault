//! Reindex — rebuild the derived SQLite index from `vault/**` (docs/03-data-model.md §D15).
//!
//! This is the operation that enforces the *reindex invariant* (ADR-0002): the whole index is
//! reconstructable from markdown alone. It runs inside a single transaction and returns counts so
//! callers (and the property test in docs/10-testing-strategy.md) can verify the rebuild.

use std::path::Path;

use rusqlite::Connection;

use super::IndexError;

/// Row counts produced by a reindex, used for verification (D15).
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct ReindexCounts {
    pub ideas: usize,
    pub facts: usize,
    pub links: usize,
}

/// Cheap staleness check: does the vault differ from what the index reflects?
///
/// Used for startup-if-drift (D25). The scaffold conservatively reports "no drift".
pub fn check_drift(_conn: &Connection, _vault_dir: &Path) -> Result<bool, IndexError> {
    // TODO(reindex): see docs/03-data-model.md §D15 and §D25 — compare vault mtimes/slug set (or a
    // content hash) against the indexed ideas and return true when a rebuild is warranted.
    Ok(false)
}

/// Rebuild the entire derived index from the vault, transactionally.
///
/// D15 sequence (must stay transactional + idempotent — `reindex(V) == reindex(reindex(V))`):
/// 1. BEGIN transaction.
/// 2. Clear derived tables (ideas, tags, idea_tags, memory_facts, backlinks, search_fts).
/// 3. Enumerate `vault/<slug>/` via `vault::walk`.
/// 4. For each idea dir, read idea.md / conversation.md / memory/*.md.
/// 5. Parse frontmatter + bodies via `domain`.
/// 6. Upsert `ideas`, `tags`, `idea_tags`.
/// 7. Upsert `memory_facts`.
/// 8. Insert `search_fts` rows (idea body + conversation).
/// 9. Insert `backlinks` for every `[[slug]]` found (target_idea_id left NULL for now).
/// 10. Resolve `backlinks.target_idea_id` by slug, then COMMIT and return counts.
pub fn reindex(_conn: &mut Connection, _vault_dir: &Path) -> Result<ReindexCounts, IndexError> {
    // TODO(reindex): see docs/03-data-model.md §D15 — implement the 10-step sequence above.
    Err(IndexError::NotImplemented("index::reindex::reindex"))
}
