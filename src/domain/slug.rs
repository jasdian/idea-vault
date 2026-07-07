//! Title → slug normalization and collision disambiguation (docs/03-data-model.md D22).
//! The slug is generated once at idea creation and never changes afterward — it is both the
//! vault folder name and the `[[slug]]` link target.

/// Normalize a raw title into a filesystem/URL-safe slug: lowercase, collapse whitespace runs
/// to `-`, strip anything outside `[a-z0-9-]`, collapse repeated `-`, trim leading/trailing `-`.
/// Titles that normalize to nothing (empty or symbol-only) fall back to `"idea"`.
pub fn slugify(title: &str) -> String {
    let lowered = title.to_lowercase();

    let mut normalized = String::with_capacity(lowered.len());
    for ch in lowered.chars() {
        if ch.is_whitespace() {
            normalized.push('-');
        } else if ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-' {
            normalized.push(ch);
        }
        // anything else (unicode letters, punctuation, symbols) is stripped
    }

    let slug = normalized
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");

    if slug.is_empty() {
        "idea".to_string()
    } else {
        slug
    }
}

/// True if `slug` has the canonical shape `slugify` produces: non-empty, only `[a-z0-9-]`.
/// This is the filesystem-boundary check — a slug is used verbatim as a vault path component,
/// so anything outside this charset (separators, `..`, unicode) must be rejected before any
/// path join (docs/03-data-model.md D22: slug == folder name).
pub fn is_valid(slug: &str) -> bool {
    !slug.is_empty()
        && slug
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
}

/// Given a candidate base slug and a predicate telling whether a slug is already taken, return
/// `base` if free, else `base-2`, `base-3`, … until a free slug is found (docs/03-data-model.md D22).
pub fn disambiguate(base: &str, exists: impl Fn(&str) -> bool) -> String {
    if !exists(base) {
        return base.to_string();
    }
    let mut n = 2u32;
    loop {
        let candidate = format!("{base}-{n}");
        if !exists(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_basic_lowercase_and_spaces() {
        assert_eq!(
            slugify("Distributed Idea Market"),
            "distributed-idea-market"
        );
    }

    #[test]
    fn slugify_collapses_whitespace_runs() {
        assert_eq!(slugify("hello    world"), "hello-world");
        assert_eq!(slugify("hello\t\nworld"), "hello-world");
    }

    #[test]
    fn slugify_strips_unicode_and_symbols() {
        assert_eq!(slugify("Café Idée! #1"), "caf-ide-1");
        assert_eq!(slugify("$$$ money-maker €€€"), "money-maker");
    }

    #[test]
    fn slugify_collapses_repeated_dashes() {
        assert_eq!(slugify("a---b"), "a-b");
        assert_eq!(slugify("a - - b"), "a-b");
    }

    #[test]
    fn slugify_trims_leading_and_trailing_dashes() {
        assert_eq!(slugify("-leading and trailing-"), "leading-and-trailing");
        assert_eq!(slugify("  spaced out  "), "spaced-out");
    }

    #[test]
    fn slugify_empty_input_falls_back_to_idea() {
        assert_eq!(slugify(""), "idea");
    }

    #[test]
    fn slugify_symbol_only_input_falls_back_to_idea() {
        assert_eq!(slugify("!!!@@@###"), "idea");
        assert_eq!(slugify("   "), "idea");
    }

    #[test]
    fn is_valid_accepts_canonical_slugs() {
        assert!(is_valid("distributed-idea-market"));
        assert!(is_valid("idea-2"));
    }

    #[test]
    fn is_valid_rejects_traversal_separators_and_empties() {
        assert!(!is_valid(""));
        assert!(!is_valid("../etc"));
        assert!(!is_valid("a/b"));
        assert!(!is_valid("a\\b"));
        assert!(!is_valid("Idea"));
        assert!(!is_valid("café"));
    }

    #[test]
    fn slugify_output_is_always_valid() {
        for title in ["Distributed Idea Market", "Café Idée! #1", "", "!!!"] {
            assert!(is_valid(&slugify(title)), "title: {title:?}");
        }
    }

    #[test]
    fn disambiguate_returns_base_when_free() {
        let got = disambiguate("foo", |_| false);
        assert_eq!(got, "foo");
    }

    #[test]
    fn disambiguate_appends_dash_2_when_base_taken() {
        let taken = ["foo"];
        let got = disambiguate("foo", |s| taken.contains(&s));
        assert_eq!(got, "foo-2");
    }

    #[test]
    fn disambiguate_advances_past_multiple_collisions() {
        let taken = ["foo", "foo-2", "foo-3"];
        let got = disambiguate("foo", |s| taken.contains(&s));
        assert_eq!(got, "foo-4");
    }
}
