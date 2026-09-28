//! Tag-name hygiene: detecting names that look like accidental drift of one another.

/// Whether two distinct tag names look like the same tag spelled differently.
///
/// True when, lowercased, they are equal once `-`/`_`/space separators are removed, or they have
/// the same words and each word pair agrees on some singular reading: a word of at least
/// [`MIN_PLURAL_WORD`] characters may drop `-ies` → `-y`, `-es`, or a single `s` unless it ends in
/// `ss`. Also true for a single typo between two long names: one insertion,
/// deletion or adjacent transposition when both are at least [`MIN_TYPO_LEN`] characters, or one
/// substitution when both are at least [`MIN_SUBSTITUTION_LEN`], as long as the edit neither
/// touches a digit, a word's first character, nor a word of at most three characters, since
/// `series-a`/`series-b`, `hiring`/`firing` or `python2`/`python3` name different things.
/// Identical names are never near-duplicates of themselves.
pub fn near_duplicate(a: &str, b: &str) -> bool {
    if a == b {
        return false;
    }
    if same_up_to_separators_and_plurals(a, b) {
        return true;
    }
    let (ca, cb): (Vec<char>, Vec<char>) = (
        a.to_lowercase().chars().collect(),
        b.to_lowercase().chars().collect(),
    );
    if ca == cb || ca.len().min(cb.len()) < MIN_TYPO_LEN {
        return false;
    }
    match single_edit(&ca, &cb) {
        Some(Edit::Substitution(at)) => {
            ca.len() >= MIN_SUBSTITUTION_LEN
                && plausible_typo_at(&ca, at)
                && plausible_typo_at(&cb, at)
        }
        Some(Edit::Other { short_at, long_at }) => {
            let (short, long) = if ca.len() <= cb.len() {
                (&ca, &cb)
            } else {
                (&cb, &ca)
            };
            plausible_typo_at(short, short_at.min(short.len().saturating_sub(1)))
                && plausible_typo_at(long, long_at)
        }
        None => false,
    }
}

const MIN_PLURAL_WORD: usize = 5;
const MIN_TYPO_LEN: usize = 6;
const MIN_SUBSTITUTION_LEN: usize = 8;

fn is_separator(c: char) -> bool {
    c == '-' || c == '_' || c.is_whitespace()
}

fn words(name: &str) -> Vec<String> {
    name.to_lowercase()
        .split(is_separator)
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

fn same_up_to_separators_and_plurals(a: &str, b: &str) -> bool {
    let (wa, wb) = (words(a), words(b));
    if wa.is_empty() || wb.is_empty() {
        return false;
    }
    if wa.concat() == wb.concat() {
        return true;
    }
    wa.len() == wb.len()
        && wa.iter().zip(&wb).all(|(x, y)| {
            let (cx, cy) = (singulars(x), singulars(y));
            cx.iter().any(|c| cy.contains(c))
        })
}

// Every reading of `word` as a possible English plural, plus the word itself. English is
// ambiguous (`caches` → `cache`, `boxes` → `box`), so all readings are kept and two words match
// when any of their readings agree.
fn singulars(word: &str) -> Vec<String> {
    let mut out = vec![word.to_string()];
    if word.chars().count() < MIN_PLURAL_WORD {
        return out;
    }
    if let Some(stem) = word.strip_suffix("ies") {
        out.push(format!("{stem}y"));
    }
    if let Some(stem) = word.strip_suffix("es") {
        out.push(stem.to_string());
    }
    if let Some(stem) = word.strip_suffix('s') {
        if !word.ends_with("ss") {
            out.push(stem.to_string());
        }
    }
    out
}

enum Edit {
    Substitution(usize),
    Other { short_at: usize, long_at: usize },
}

fn single_edit(a: &[char], b: &[char]) -> Option<Edit> {
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let at = short.iter().zip(long).take_while(|(x, y)| x == y).count();
    match long.len() - short.len() {
        0 if short[at + 1..] == long[at + 1..] => Some(Edit::Substitution(at)),
        0 if at + 1 < short.len()
            && short[at] == long[at + 1]
            && short[at + 1] == long[at]
            && short[at + 2..] == long[at + 2..] =>
        {
            Some(Edit::Other {
                short_at: at,
                long_at: at,
            })
        }
        1 if short[at..] == long[at + 1..] => Some(Edit::Other {
            short_at: at,
            long_at: at,
        }),
        _ => None,
    }
}

// A typo at `at` is plausible only mid-word, off digits, and inside a word longer than three
// characters.
fn plausible_typo_at(name: &[char], at: usize) -> bool {
    let Some(&c) = name.get(at) else {
        return false;
    };
    if c.is_ascii_digit() || is_separator(c) {
        return false;
    }
    let start = name[..at]
        .iter()
        .rposition(|&c| is_separator(c))
        .map_or(0, |i| i + 1);
    let end = name[at..]
        .iter()
        .position(|&c| is_separator(c))
        .map_or(name.len(), |i| at + i);
    at != start && end - start > 3
}

#[cfg(test)]
mod tests {
    use super::near_duplicate;

    #[test]
    fn near_duplicate_matches_plural_drift() {
        assert!(near_duplicate("system-design", "systems-design"));
        assert!(near_duplicate("systems-design", "system-design"));
        assert!(near_duplicate("prototype", "prototypes"));
        assert!(near_duplicate("customer-interview", "customer-interviews"));
        assert!(near_duplicate("cache", "caches"));
        assert!(near_duplicate("user-story", "user-storys"));
    }

    #[test]
    fn near_duplicate_ignores_separator_and_case_drift() {
        assert!(near_duplicate("design_review", "design-review"));
        assert!(near_duplicate("design review", "design-review"));
        assert!(near_duplicate("designreview", "design-review"));
        assert!(near_duplicate("Design-Review", "design_review"));
    }

    #[test]
    fn near_duplicate_matches_one_typo_when_both_are_long_enough() {
        assert!(near_duplicate("onboarding", "onboardng"));
        assert!(near_duplicate("microservice", "microservise"));
        assert!(near_duplicate("microservices-arch", "microservises-arch"));
        assert!(near_duplicate("onboarding", "onbaording"));
    }

    #[test]
    fn near_duplicate_rejects_short_names_one_edit_apart() {
        assert!(!near_duplicate("rust", "trust"));
        assert!(!near_duplicate("ddns", "dns"));
        assert!(!near_duplicate("abcde", "abcdf"));
        assert!(!near_duplicate("pricin", "prici"));
    }

    #[test]
    fn near_duplicate_rejects_identical_and_unrelated_names() {
        assert!(!near_duplicate("system-design", "system-design"));
        assert!(!near_duplicate("", ""));
        assert!(!near_duplicate("market", "markets-x"));
        assert!(!near_duplicate("frontend", "backend"));
        assert!(!near_duplicate("go-to-market", "go-to-marketing-plan"));
        assert!(!near_duplicate("-", "_"));
    }

    #[test]
    fn near_duplicate_strips_only_one_trailing_s_per_word() {
        assert!(!near_duplicate("class", "clas"));
        assert!(!near_duplicate("business", "busine"));
        assert!(!near_duplicate("go-tos", "go-to"));
        assert!(!near_duplicate("moss", "mos"));
        assert!(near_duplicate("boxes", "box"));
        assert!(near_duplicate("mailboxes", "mailbox"));
    }

    #[test]
    fn near_duplicate_is_symmetric() {
        let names = [
            "system-design",
            "systems-design",
            "design_review",
            "design-review",
            "rust",
            "trust",
            "onboarding",
            "onboardng",
            "ddns",
            "dns",
            "microservice",
            "microservise",
            "onbaording",
            "strategies",
            "strategy",
        ];
        for a in names {
            for b in names {
                assert_eq!(near_duplicate(a, b), near_duplicate(b, a), "{a} vs {b}");
            }
        }
    }

    #[test]
    fn near_duplicate_matches_ies_and_es_plurals() {
        assert!(near_duplicate("strategies", "strategy"));
        assert!(near_duplicate("companies", "company"));
        assert!(near_duplicate("businesses", "business"));
        assert!(near_duplicate("classes", "class"));
    }

    #[test]
    fn near_duplicate_rejects_meaningful_one_character_differences() {
        for (a, b) in [
            ("series-a", "series-b"),
            ("b2b-saas", "b2c-saas"),
            ("layer-1", "layer-2"),
            ("python2", "python3"),
            ("hiring", "firing"),
            ("design", "resign"),
            ("market", "marker"),
            ("ios", "io"),
            ("news", "new"),
            ("ops", "op"),
            ("ai", "api"),
            ("pricing", "pricinh"),
        ] {
            assert!(!near_duplicate(a, b), "{a} vs {b}");
            assert!(!near_duplicate(b, a), "{b} vs {a}");
        }
    }
}
