//! Reopen-time memory context reload (docs/06-concepts/memory.md D13).

use std::path::Path;

use crate::memory::MemoryError;

/// Assemble the context block loaded when an idea transitions `Stored→Reopened`.
///
/// TODO(reopen): see docs/06-concepts/memory.md §D13 — always load `MEMORY.md` (cheap index);
/// pull full `memory/*.md` fact bodies in only up to the context budget, selected by
/// relevance/recency (docs/06-concepts/swarm.md D21); reopen is truth-idempotent — it loads
/// context and flips state, it must never rewrite `idea.md` body or `memory/` (those change only
/// on Store, docs/04-state-machine.md D9).
pub fn load_context(_vault_dir: &Path, _slug: &str) -> Result<String, MemoryError> {
    Err(MemoryError::NotImplemented("memory::load::load_context"))
}
