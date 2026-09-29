//! The evidence gate: does a quoted claim occur, verbatim after normalization, in the text it
//! claims to come from? Pure string matching with no model call, shared by store-time memory
//! extraction and the build-plan gates (docs/adr/0023, docs/adr/0030).

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

/// The skill/workflow names whose transcript turns are build plans or their pointers
/// (docs/adr/0030). Those turns are model-authored plan text, so they are never evidence: not
/// for a later plan's quotes, and not for store-time memory quotes. A skill or workflow turn
/// under any other name is excluded by its pointer shape ([`POINTER_PREFIX`]).
pub const CAPSTONE_TURNS: &[&str] = &["build-prompt", "ready-to-build"];

/// The line prefix the code writes at the start of every build-plan pointer turn, whatever
/// skill or workflow produced it. Writer and detector share it so they cannot drift.
pub const POINTER_PREFIX: &str = "**Build plan** → [";

/// True when `body` (a turn's text after its heading line) is a build-plan pointer: it starts
/// with [`POINTER_PREFIX`]. Pure.
pub fn is_pointer_body(body: &str) -> bool {
    body.trim_start().starts_with(POINTER_PREFIX)
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
    locate(quote, haystack_normalized).is_some()
}

/// Where a grounded quote starts in `haystack_normalized` (a byte offset into the normalized
/// text), under the same rules as [`grounded`]; `None` when it does not ground.
pub fn locate(quote: &str, haystack_normalized: &str) -> Option<usize> {
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
    let (first, rest) = segments.split_first()?;
    if words < MIN_QUOTE_WORDS {
        return None;
    }
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
    haystack_normalized
        .match_indices(first.as_str())
        .find(|(at, _)| chained_from(at + first.len()).is_some())
        .map(|(at, _)| at)
}

/// Words too common to signal that two texts are about the same thing.
const STOPWORDS: &[&str] = &[
    "the", "and", "for", "are", "but", "not", "you", "all", "any", "can", "had", "her", "was",
    "one", "our", "out", "has", "his", "how", "its", "may", "new", "now", "who", "did", "get",
    "let", "say", "she", "too", "use", "that", "this", "with", "from", "have", "they", "will",
    "what", "when", "which", "their", "there", "been", "into", "than", "then", "them", "these",
    "those", "would", "could", "should", "about", "each", "just", "also", "only", "some", "more",
    "most", "such", "very", "does", "were", "your", "over", "after", "before", "because", "while",
];

/// The distinct content words of `text`: lowercased alphanumeric runs of at least three
/// characters that are not stopwords.
pub fn content_words(text: &str) -> std::collections::BTreeSet<String> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|w| w.chars().count() >= 3)
        .map(str::to_lowercase)
        .filter(|w| !STOPWORDS.contains(&w.as_str()))
        .collect()
}

/// How much two texts share: the count of common content words and that count as a fraction of
/// the smaller text's content words (0.0 when either has none).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Overlap {
    pub shared: usize,
    pub ratio: f64,
}

impl Overlap {
    /// At least `ratio` of the smaller text, and at least `min_shared` words.
    pub fn at_least(self, ratio: f64, min_shared: usize) -> bool {
        self.shared >= min_shared && self.ratio >= ratio
    }
}

/// Content-word overlap between `a` and `b` (see [`Overlap`]).
pub fn content_overlap(a: &str, b: &str) -> Overlap {
    let (a, b) = (content_words(a), content_words(b));
    let shared = a.intersection(&b).count();
    let smaller = a.len().min(b.len());
    Overlap {
        shared,
        ratio: if smaller == 0 {
            0.0
        } else {
            shared as f64 / smaller as f64
        },
    }
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
    fn locate_reports_where_a_grounded_quote_starts() {
        let hay = normalize_for_match("We ship solo. Revenue comes first, then hires.");
        assert_eq!(locate("revenue comes first", &hay), Some(14));
        assert_eq!(locate("revenue … then hires", &hay), Some(14));
        assert_eq!(locate("hires come first", &hay), None);
        assert_eq!(
            locate("revenue comes", &hay),
            None,
            "under the minimum word count"
        );
    }

    #[test]
    fn content_overlap_counts_shared_content_words() {
        let o = content_overlap(
            "Freeze the zone snapshot at entry",
            "Should we freeze the zone snapshot, or use dwell hysteresis?",
        );
        assert_eq!(o.shared, 3, "freeze, zone, snapshot");
        assert!(
            (o.ratio - 0.75).abs() < 1e-9,
            "3 of the smaller text's 4 content words"
        );
        assert!(o.at_least(0.6, 3));
        assert!(!content_overlap("the and for", "the and for").at_least(0.1, 1));
        assert_eq!(content_overlap("", "anything").ratio, 0.0);
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

    #[test]
    fn pointer_body_is_recognised_by_its_prefix() {
        assert!(is_pointer_body(&format!("{POINTER_PREFIX}x](/a) · quick")));
        assert!(is_pointer_body(&format!("\n{POINTER_PREFIX}x](/a)")));
    }

    #[test]
    fn pointer_body_rejects_text_that_merely_mentions_it() {
        assert!(!is_pointer_body("see **Build plan** → [x](/a)"));
        assert!(!is_pointer_body("Build plan → [x]"));
        assert!(!is_pointer_body(""));
    }
}
