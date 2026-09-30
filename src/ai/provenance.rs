//! Recipe provenance (ADR-0040): the digests and build id an artifact's `recipe:` frontmatter is
//! stamped with, so an old artifact can be told from a new one and a changed skill or workflow
//! shows as "recipe changed since".
//!
//! Only the prompts whose answers code parses are registered as [`PromptTemplate`]s — the audit
//! instruction and its re-ask, the contract retry note, and the extract, consolidate and compact
//! instructions. A wording change there can change what a parser sees, so each is pinned by a
//! golden test and carries an explicit version. Personas and skills are owner-editable prompt
//! data: they are digested, never frozen.

use sha2::{Digest, Sha256};

use crate::domain::Recipe;

/// A parse-coupled prompt: a stable id, a version bumped by hand when the wording changes on
/// purpose, and the text itself (with any `{placeholder}` still in it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptTemplate {
    pub id: &'static str,
    pub version: u16,
    pub text: &'static str,
}

/// The first 12 hex digits of the SHA-256 of `bytes`: short enough for a badge, long enough that
/// two edits of one skill never collide in practice.
pub fn digest12(bytes: &[u8]) -> String {
    let full = Sha256::digest(bytes);
    full.iter()
        .take(6)
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
}

/// `id@vN:digest12` — the template's name, its declared version and the digest of its text, so
/// an edit that forgot to bump the version still shows as a different ref.
pub fn template_ref(t: &PromptTemplate) -> String {
    format!("{}@v{}:{}", t.id, t.version, digest12(t.text.as_bytes()))
}

/// The build that wrote an artifact: the crate version, plus `+<sha>` when the image was built
/// with `IDEA_VAULT_BUILD_SHA` (a Dockerfile ARG). A plain `cargo run` has no sha and stamps the
/// version alone (owner decision, ADR-0040).
pub fn build_id() -> String {
    compose_build_id(
        env!("CARGO_PKG_VERSION"),
        option_env!("IDEA_VAULT_BUILD_SHA"),
    )
}

/// A recipe stamped with this build and the given parse-coupled templates; the writer adds the
/// skill or workflow it ran.
pub fn recipe(templates: &[PromptTemplate]) -> Recipe {
    Recipe {
        templates: templates.iter().map(template_ref).collect(),
        build: build_id(),
        ..Recipe::default()
    }
}

fn compose_build_id(version: &str, sha: Option<&str>) -> String {
    match sha.map(str::trim).filter(|s| !s.is_empty()) {
        Some(sha) => format!("{version}+{sha}"),
        None => version.to_string(),
    }
}

/// Compare a rendered prompt with its committed golden in `tests/fixtures/prompt-goldens/`.
/// Shared by the goldens here and in `concepts` (which `ai` may not import, D4). A golden is an
/// expectation: a change to one is declared under `## Expectation changes` (ADR-0041).
#[cfg(test)]
pub(crate) fn assert_golden(actual: &str, golden: &str, file: &str) {
    // A golden file ends with the newline an editor adds; the prompt itself does not.
    let golden = golden.strip_suffix('\n').unwrap_or(golden);
    assert_eq!(
        actual, golden,
        "prompt drifted from tests/fixtures/prompt-goldens/{file}; if the change is intended, \
         bump the template version, update the golden and declare it"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::contract::{retry_note, Violation};

    #[test]
    fn digest12_is_twelve_lowercase_hex() {
        let d = digest12(b"premortem");
        assert_eq!(d.len(), 12);
        assert!(d
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_ne!(d, digest12(b"premortem "));
    }

    #[test]
    fn template_ref_names_id_version_and_digest() {
        let t = PromptTemplate {
            id: "demo",
            version: 3,
            text: "hello",
        };
        assert_eq!(template_ref(&t), format!("demo@v3:{}", digest12(b"hello")));
    }

    #[test]
    fn build_id_is_version_alone_without_a_sha() {
        assert_eq!(compose_build_id("0.1.0", None), "0.1.0");
        assert_eq!(compose_build_id("0.1.0", Some("  ")), "0.1.0");
        assert_eq!(compose_build_id("0.1.0", Some("abc123")), "0.1.0+abc123");
        assert!(build_id().starts_with(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn golden_retry_note() {
        assert_golden(
            &retry_note(&Violation::NoNumberedList),
            include_str!("../../tests/fixtures/prompt-goldens/retry-note.txt"),
            "retry-note.txt",
        );
    }
}
