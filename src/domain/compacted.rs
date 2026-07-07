//! The `compacted.md` sidecar type (auto-compact, docs/adr/0012): a derived, non-canonical
//! rolling summary of the conversation head. Analogous to how `MemoryIndex` mirrors `MEMORY.md`,
//! but for the live in-discussion compaction cache. `conversation.md` stays the source of truth;
//! `compacted.md` is a deletable cache, made correct by the `covered_bytes` prefix fingerprint.

use crate::domain::CompactedFrontmatter;

/// A parsed `compacted.md`: the fingerprinted header plus the four-heading rolling summary body.
#[derive(Debug, Clone, PartialEq)]
pub struct Compacted {
    pub frontmatter: CompactedFrontmatter,
    pub summary: String,
}
