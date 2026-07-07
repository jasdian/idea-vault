//! Store-time memory extraction (docs/06-concepts/memory.md D12).

use crate::domain::MemoryFact;
use crate::memory::MemoryError;

/// Consolidate the idea body then distil a bounded set of durable facts from the discussion, on
/// the `InDiscussion→Stored` (or `Reopened→Stored`) transition (docs/04-state-machine.md D9).
///
/// TODO(store): see docs/06-concepts/memory.md §D12 — consolidate the idea body to the current
/// best statement before extracting; target a small bounded fact set, not a transcript dump; on
/// re-store (idea was `Reopened`), merge and dedupe against existing `memory/*.md` rather than
/// dropping prior facts; write markdown before any index upsert (ADR-0002).
pub fn extract_facts(_conversation_markdown: &str) -> Result<Vec<MemoryFact>, MemoryError> {
    Err(MemoryError::NotImplemented(
        "memory::extract::extract_facts",
    ))
}
