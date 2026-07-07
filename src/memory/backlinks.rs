//! `[[slug]]` backlink parsing (docs/06-concepts/memory.md D23).
//!
//! The actual parser is `domain::links::extract_links` — it lives in `domain` so
//! `index::reindex` can use it without depending on this module (D4). This is the
//! `memory`-facing entry point for callers working with facts and idea bodies. Resolution
//! against `ideas.slug` (setting `target_idea_id` or leaving it NULL for forward references)
//! happens in `index::reindex`, not here, so not-yet-created ideas can be linked before they
//! exist.

use crate::domain::links;
use crate::memory::MemoryError;

/// Parse `[[slug]]`-style links out of a markdown body or fact: first-occurrence order,
/// deduplicated, targets validated as canonical slugs. Dangling/forward targets are allowed.
pub fn find_links(markdown: &str) -> Result<Vec<String>, MemoryError> {
    Ok(links::extract_links(markdown))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delegates_to_domain_links() {
        let got = find_links("see [[other-idea]] and [not a wiki link](x)").unwrap();
        assert_eq!(got, ["other-idea"]);
    }
}
