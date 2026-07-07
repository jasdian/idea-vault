//! Pure `[[slug]]` link extraction (docs/06-concepts/memory.md D23).
//!
//! Parsing lives in `domain` (not `memory`) so that `index::reindex` — which may depend only on
//! `vault` + `domain` (docs/02-module-reference.md D4) — can scan bodies and facts for links
//! without a `memory` dependency cycle. Resolution against existing ideas (setting
//! `backlinks.target_idea_id` or leaving it NULL for forward references) happens in
//! `index::reindex`, never here.

use crate::domain::slug;

/// Extract every `[[slug]]` link target from a markdown text, in first-occurrence order,
/// deduplicated. Only tokens whose inner text is a canonical slug (`domain::slug::is_valid`)
/// count — `[[Not a slug]]` is prose, and ordinary markdown links `[text](url)` are never
/// matched. Targets need not exist yet: forward/dangling references are allowed (D23).
pub fn extract_links(markdown: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut rest = markdown;

    while let Some(start) = rest.find("[[") {
        let after_open = &rest[start + 2..];
        let Some(end) = after_open.find("]]") else {
            break;
        };
        let inner = &after_open[..end];
        if slug::is_valid(inner) && !found.iter().any(|s| s == inner) {
            found.push(inner.to_string());
        }
        rest = &after_open[end + 2..];
    }

    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_multiple_links_in_order() {
        let md = "See [[first-idea]] and later [[second-idea]].";
        assert_eq!(extract_links(md), ["first-idea", "second-idea"]);
    }

    #[test]
    fn dedupes_repeated_links_keeping_first_occurrence_order() {
        let md = "[[b-idea]] then [[a-idea]] then [[b-idea]] again";
        assert_eq!(extract_links(md), ["b-idea", "a-idea"]);
    }

    #[test]
    fn forward_and_dangling_references_are_extracted() {
        // The target need not exist anywhere — resolution is reindex's job (D23).
        assert_eq!(extract_links("[[not-created-yet]]"), ["not-created-yet"]);
    }

    #[test]
    fn ordinary_markdown_links_are_not_matched() {
        let md = "A [normal link](https://example.com) and [another](memory/f.md).";
        assert!(extract_links(md).is_empty());
    }

    #[test]
    fn non_slug_inner_text_is_prose_not_a_link() {
        assert!(extract_links("[[Not A Slug]]").is_empty());
        assert!(extract_links("[[has space]]").is_empty());
        assert!(extract_links("[[UPPER]]").is_empty());
        assert!(extract_links("[[]]").is_empty());
        assert!(extract_links("[[a/b]]").is_empty());
    }

    #[test]
    fn unclosed_or_stray_brackets_do_not_panic_or_match() {
        assert!(extract_links("[[unclosed").is_empty());
        assert!(extract_links("]] backwards [[").is_empty());
        assert!(extract_links("[single] brackets [again]").is_empty());
    }

    #[test]
    fn triple_brackets_still_find_the_inner_link() {
        // "[[[x]]]" scans as "[[" + "[x" (invalid slug) — strictness over cleverness; but a
        // clean link adjacent to brackets still parses.
        assert_eq!(extract_links("x [[valid-slug]] y"), ["valid-slug"]);
        assert!(extract_links("[[[nested-ish]]]").is_empty());
    }

    #[test]
    fn multibyte_utf8_around_brackets_is_safe() {
        // Regression guard for the byte-index arithmetic: multi-byte chars adjacent to the
        // 2-byte ASCII tokens must neither panic nor affect matching.
        assert_eq!(extract_links("héllo [[foo]] wörld"), ["foo"]);
        assert_eq!(extract_links("🎯[[goal-idea]]🎯"), ["goal-idea"]);
        assert!(extract_links("[[café]]").is_empty());
        assert!(extract_links("é[[").is_empty());
    }

    #[test]
    fn links_separated_by_text_and_newlines() {
        let md = "line one [[alpha]]\n\nline two [[beta]] end";
        assert_eq!(extract_links(md), ["alpha", "beta"]);
    }
}
