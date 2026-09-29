//! The evidence gate: does a quoted claim occur, verbatim after normalization, in the text it
//! claims to come from? Pure string matching with no model call, shared by store-time memory
//! extraction and the build-plan gates (docs/adr/0023, docs/adr/0029).

/// Fold text for quote matching: lowercase, typographic quotes and dashes made plain, markdown
/// emphasis/quote/heading marks and backslash escapes dropped, whitespace collapsed — so a quote
/// the model copied from rendered-looking text, or wrote with `\"` escapes, still matches the raw
/// markdown it came from.
pub fn normalize_for_match(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last_space = true;
    for ch in text.chars() {
        let ch = match ch {
            '“' | '”' | '„' => '"',
            '‘' | '’' => '\'',
            '–' | '—' => '-',
            c => c,
        };
        if matches!(ch, '*' | '_' | '`' | '>' | '#' | '\\') {
            continue;
        }
        if ch.is_whitespace() {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
            continue;
        }
        out.extend(ch.to_lowercase());
        last_space = false;
    }
    out.trim().to_string()
}

/// Minimum words a supporting quote must carry — anything shorter ("yes", "the market") matches
/// almost any discussion and proves nothing.
pub const MIN_QUOTE_WORDS: usize = 3;

/// Most normalized bytes an elision (`…`) in a supporting quote may skip.
pub const MAX_ELISION_GAP: usize = 200;

/// The evidence gate: does `quote` occur (normalized) in `haystack`? An elided quote
/// (`a … b`) passes only if its segments occur in order, each close after the last. Pure — no
/// model call.
pub fn grounded(quote: &str, haystack_normalized: &str) -> bool {
    let segments: Vec<String> = quote
        .split(['…'])
        .flat_map(|s| s.split("..."))
        .map(|s| {
            normalize_for_match(s)
                .trim_matches(|c: char| {
                    c.is_whitespace() || matches!(c, '"' | '\'' | '.' | ',' | ';' | ':' | '!' | '?')
                })
                .to_string()
        })
        .filter(|s| !s.is_empty())
        .collect();
    let words: usize = segments.iter().map(|s| s.split(' ').count()).sum();
    let Some((first, rest)) = segments.split_first() else {
        return false;
    };
    // An elision stands for a few skipped words, not a jump across the discussion: every later
    // segment must follow the previous one within MAX_ELISION_GAP bytes, in order — otherwise
    // two unrelated true fragments could vouch for a spliced false claim.
    let chained_from = |start: usize| {
        rest.iter().try_fold(start, |cursor, segment| {
            haystack_normalized[cursor..]
                .find(segment.as_str())
                .filter(|gap| *gap <= MAX_ELISION_GAP)
                .map(|gap| cursor + gap + segment.len())
        })
    };
    words >= MIN_QUOTE_WORDS
        && haystack_normalized
            .match_indices(first.as_str())
            .any(|(at, _)| chained_from(at + first.len()).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_backslash_escaped_quote_still_grounds() {
        let hay = normalize_for_match(
            "## user\nOf 56 stored facts, 6 contain \"cheapest disproof\" in their text.",
        );
        assert!(
            grounded(r#"6 contain \"cheapest disproof\" in their text"#, &hay),
            "a model-escaped double quote matches the plain quote"
        );
        let escaped_hay = normalize_for_match(r"the owner wrote \*not\* \_this\_ one");
        assert!(
            grounded("the owner wrote not this one", &escaped_hay),
            "markdown escapes in the source fold away too"
        );
    }

    #[test]
    fn grounded_matches_normalized_verbatim_spans_only() {
        let hay = normalize_for_match(
            "## user\nWe **ship v1** solo — no hires until\n   revenue.\n## assistant\nOK.",
        );
        assert!(
            grounded("we ship v1 solo - no hires", &hay),
            "markup + dash + case folded"
        );
        assert!(
            grounded("\u{201c}no hires until revenue\u{201d}", &hay),
            "line break folded"
        );
        assert!(
            grounded("we ship … until revenue", &hay),
            "elided segments each match"
        );
        assert!(!grounded("we hire a team first", &hay), "invented quote");
        assert!(!grounded("ship v1", &hay), "under the minimum word count");
        assert!(
            !grounded("we ship … hire a CTO", &hay),
            "one segment invented"
        );
    }

    #[test]
    fn an_elided_quote_cannot_splice_distant_fragments_together() {
        let filler = "unrelated discussion ".repeat(40);
        let hay = normalize_for_match(&format!(
            "we ship solo. {filler} the market is agencies. {filler} revenue first"
        ));
        assert!(
            grounded("the market … is agencies", &hay),
            "adjacent segments pass"
        );
        assert!(
            !grounded("we ship … revenue first", &hay),
            "segments hundreds of bytes apart are a splice, not a quote"
        );
        assert!(
            !grounded("revenue first … we ship", &hay),
            "segments must occur in order"
        );
    }
}
