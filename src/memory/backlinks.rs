//! `[[slug]]` backlink parsing (docs/06-concepts/memory.md D23).

use crate::memory::MemoryError;

/// Parse `[[slug]]`-style links out of a markdown body or fact.
///
/// TODO(backlinks): see docs/06-concepts/memory.md §D23 — this function only *parses* the
/// `[[slug]]` tokens out of markdown text; resolution against `ideas.slug` (setting
/// `target_idea_id` or leaving it NULL for forward references) happens in `index::reindex`, not
/// here, so that not-yet-created ideas can be linked before they exist.
pub fn find_links(_markdown: &str) -> Result<Vec<String>, MemoryError> {
    Err(MemoryError::NotImplemented("memory::backlinks::find_links"))
}
