//! Frontmatter schema (docs/03-data-model.md D8) and the `---\n<yaml>\n---\n<body>` fence
//! parse/emit functions used for both `idea.md` and `memory/<fact-slug>.md`.

use chrono::{DateTime, Utc};

use crate::domain::artifact::ArtifactKind;
use crate::domain::idea::IdeaState;
use crate::domain::DomainError;

/// Cap on `IdeaFrontmatter::tags`, shared by every writer (the owner-edit form and store-time
/// model-suggested merge) so the set stays chips, not prose, no matter how many store/reopen
/// cycles an idea goes through.
pub const MAX_IDEA_TAGS: usize = 10;

/// The structured header of `idea.md`. Field names and the serialized `state` values are a data
/// contract (docs/03-data-model.md D8) — do not rename.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct IdeaFrontmatter {
    pub title: String,
    pub slug: String,
    pub state: IdeaState,
    #[serde(default)]
    pub tags: Vec<String>,
    pub created: DateTime<Utc>,
    pub updated: DateTime<Utc>,
}

/// The structured header of a `compacted.md` sidecar — the derived rolling summary of the
/// conversation head (auto-compact, docs/adr/0012). `covered_bytes` is a staleness fingerprint
/// over `turns[0..compacted_through]`; `compacted.md` is a *deletable cache*, never truth.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CompactedFrontmatter {
    /// `k`: `turns[0..k]` (in `store::split_turns` order) are folded into the summary body.
    pub compacted_through: usize,
    /// Σ `prefix_bytes(turns, k)` at write time — the fingerprint that detects a mutated prefix.
    pub covered_bytes: usize,
    /// `n` (total turn count) at write time — for display / staleness UI only.
    pub turn_count_at_compaction: usize,
    /// The model that produced the summary — provenance.
    pub model: String,
    pub updated: DateTime<Utc>,
}

/// The structured header of an `artifacts/<file-slug>.md` file — one persisted
/// knowledge-extraction output (docs/adr/0015). `lens` is the extraction skill that produced a
/// finding (`None` for the synthesis); `model` is provenance, like `CompactedFrontmatter`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ArtifactFrontmatter {
    /// File stem (canonical slug charset) — mirrors `MemoryFactFrontmatter::slug`.
    pub slug: String,
    pub title: String,
    pub kind: ArtifactKind,
    #[serde(default)]
    pub lens: Option<String>,
    pub created: DateTime<Utc>,
    pub model: String,
}

/// The (lighter) structured header of a `memory/<fact-slug>.md` file.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MemoryFactFrontmatter {
    pub slug: String,
    pub title: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub created: DateTime<Utc>,
    #[serde(default)]
    pub links: Vec<String>,
}

/// Split a `---\n<yaml>\n---\n<body>` fenced document into its raw YAML block and body text.
/// Returns `Err(DomainError::MissingFrontmatter)` if the leading fence is absent or malformed.
fn split_fence(input: &str) -> Result<(&str, &str), DomainError> {
    let rest = input
        .strip_prefix("---\n")
        .or_else(|| input.strip_prefix("---\r\n"))
        .ok_or(DomainError::MissingFrontmatter)?;

    // Find the closing fence: a line that is exactly "---".
    let mut search_from = 0usize;
    loop {
        let rel_idx = rest[search_from..]
            .find("---")
            .ok_or(DomainError::MissingFrontmatter)?;
        let idx = search_from + rel_idx;

        // The closing fence must start at the beginning of a line (preceded by \n, or at 0 which
        // can't happen here since idx > 0 always after a non-empty yaml block) and be followed by
        // end-of-string, \n, or \r\n.
        let preceded_by_newline = idx > 0 && rest.as_bytes()[idx - 1] == b'\n';
        if !preceded_by_newline {
            search_from = idx + 3;
            continue;
        }

        let after = &rest[idx + 3..];
        let (yaml, body_start) = if let Some(stripped) = after
            .strip_prefix("\r\n")
            .or_else(|| after.strip_prefix('\n'))
        {
            // `emit_fence` separates the closing fence from the body with a blank line
            // (`---\n\n<body>`); consume that separator too so parse(emit(fm, body)) == body.
            let stripped = stripped
                .strip_prefix("\r\n")
                .or_else(|| stripped.strip_prefix('\n'))
                .unwrap_or(stripped);
            (&rest[..idx], stripped)
        } else if after.is_empty() {
            (&rest[..idx], "")
        } else {
            // "---" appeared mid-line (e.g. "---foo"); not a real fence, keep searching.
            search_from = idx + 3;
            continue;
        };

        return Ok((yaml, body_start));
    }
}

/// Render a value's YAML plus body into the canonical `---\n<yaml>---\n\n<body>` fence.
fn emit_fence(yaml: &str, body: &str) -> String {
    let mut out = String::with_capacity(yaml.len() + body.len() + 16);
    out.push_str("---\n");
    out.push_str(yaml);
    if !yaml.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("---\n\n");
    out.push_str(body);
    out
}

/// Parse an `idea.md` document into its frontmatter and body.
pub fn parse_idea(input: &str) -> Result<(IdeaFrontmatter, String), DomainError> {
    let (yaml, body) = split_fence(input)?;
    let fm: IdeaFrontmatter = serde_norway::from_str(yaml)?;
    Ok((fm, body.to_string()))
}

/// Render an `idea.md` document from frontmatter and body.
///
/// Serialization of these plain-data fields cannot fail in practice; the error is propagated
/// anyway (defense in depth — no panic paths in library code).
pub fn emit_idea(fm: &IdeaFrontmatter, body: &str) -> Result<String, DomainError> {
    let yaml = serde_norway::to_string(fm)?;
    Ok(emit_fence(&yaml, body))
}

/// Parse a `compacted.md` sidecar into its frontmatter and summary body.
pub fn parse_compacted(input: &str) -> Result<(CompactedFrontmatter, String), DomainError> {
    let (yaml, body) = split_fence(input)?;
    let fm: CompactedFrontmatter = serde_norway::from_str(yaml)?;
    Ok((fm, body.to_string()))
}

/// Render a `compacted.md` sidecar from frontmatter and summary body.
///
/// Serialization of these plain-data fields cannot fail in practice; the error is propagated
/// anyway (defense in depth — no panic paths in library code).
pub fn emit_compacted(fm: &CompactedFrontmatter, body: &str) -> Result<String, DomainError> {
    let yaml = serde_norway::to_string(fm)?;
    Ok(emit_fence(&yaml, body))
}

/// Parse an `artifacts/<file-slug>.md` document into its frontmatter and body.
pub fn parse_artifact(input: &str) -> Result<(ArtifactFrontmatter, String), DomainError> {
    let (yaml, body) = split_fence(input)?;
    let fm: ArtifactFrontmatter = serde_norway::from_str(yaml)?;
    Ok((fm, body.to_string()))
}

/// Render an `artifacts/<file-slug>.md` document from frontmatter and body.
///
/// Serialization of these plain-data fields cannot fail in practice; the error is propagated
/// anyway (defense in depth — no panic paths in library code).
pub fn emit_artifact(fm: &ArtifactFrontmatter, body: &str) -> Result<String, DomainError> {
    let yaml = serde_norway::to_string(fm)?;
    Ok(emit_fence(&yaml, body))
}

/// Parse a `memory/<fact-slug>.md` document into its frontmatter and body.
pub fn parse_memory_fact(input: &str) -> Result<(MemoryFactFrontmatter, String), DomainError> {
    let (yaml, body) = split_fence(input)?;
    let fm: MemoryFactFrontmatter = serde_norway::from_str(yaml)?;
    Ok((fm, body.to_string()))
}

/// Render a `memory/<fact-slug>.md` document from frontmatter and body.
///
/// Serialization of these plain-data fields cannot fail in practice; the error is propagated
/// anyway (defense in depth — no panic paths in library code).
pub fn emit_memory_fact(fm: &MemoryFactFrontmatter, body: &str) -> Result<String, DomainError> {
    let yaml = serde_norway::to_string(fm)?;
    Ok(emit_fence(&yaml, body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn dt(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    /// The exact example from docs/03-data-model.md D8.
    const DOC_EXAMPLE: &str = "---\n\
title: Distributed idea market\n\
slug: distributed-idea-market\n\
state: in_discussion\n\
tags: [markets, incentives]\n\
created: 2026-07-07T10:15:00Z\n\
updated: 2026-07-07T11:40:00Z\n\
---\n\
\n\
Body text here.\n";

    #[test]
    fn parse_idea_doc_example_matches_every_field() {
        let (fm, body) = parse_idea(DOC_EXAMPLE).unwrap();
        assert_eq!(fm.title, "Distributed idea market");
        assert_eq!(fm.slug, "distributed-idea-market");
        assert_eq!(fm.state, IdeaState::InDiscussion);
        assert_eq!(
            fm.tags,
            vec!["markets".to_string(), "incentives".to_string()]
        );
        assert_eq!(fm.created, dt("2026-07-07T10:15:00Z"));
        assert_eq!(fm.updated, dt("2026-07-07T11:40:00Z"));
        assert_eq!(body, "Body text here.\n");
    }

    #[test]
    fn idea_roundtrip_parse_emit_parse_struct_equality() {
        let (fm, body) = parse_idea(DOC_EXAMPLE).unwrap();
        let emitted = emit_idea(&fm, &body).unwrap();
        let (fm2, body2) = parse_idea(&emitted).unwrap();
        assert_eq!(fm, fm2);
        assert_eq!(body, body2);
    }

    #[test]
    fn idea_body_separation_preserved_including_blank_lines() {
        let body = "Line one.\n\nLine two.\n";
        let fm = IdeaFrontmatter {
            title: "T".into(),
            slug: "t".into(),
            state: IdeaState::Draft,
            tags: vec![],
            created: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            updated: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
        };
        let emitted = emit_idea(&fm, body).unwrap();
        let (_, parsed_body) = parse_idea(&emitted).unwrap();
        assert_eq!(parsed_body, body);
    }

    #[test]
    fn parse_idea_missing_fence_errors() {
        let err = parse_idea("no fence here\njust body text").unwrap_err();
        assert!(matches!(err, DomainError::MissingFrontmatter));
    }

    #[test]
    fn parse_idea_unclosed_fence_errors() {
        let input = "---\ntitle: X\nslug: x\n";
        let err = parse_idea(input).unwrap_err();
        assert!(matches!(err, DomainError::MissingFrontmatter));
    }

    #[test]
    fn parse_idea_bad_state_errors() {
        let input = "---\n\
title: X\n\
slug: x\n\
state: not_a_real_state\n\
created: 2026-01-01T00:00:00Z\n\
updated: 2026-01-01T00:00:00Z\n\
---\n\
body\n";
        let err = parse_idea(input).unwrap_err();
        assert!(matches!(err, DomainError::Yaml(_)));
    }

    #[test]
    fn memory_fact_roundtrip_including_links() {
        let fm = MemoryFactFrontmatter {
            slug: "fact-one".into(),
            title: "Fact one".into(),
            tags: vec!["risk".into()],
            created: dt("2026-07-07T10:15:00Z"),
            links: vec!["distributed-idea-market".into(), "other-idea".into()],
        };
        let body = "This is the durable conclusion.\n";
        let emitted = emit_memory_fact(&fm, body).unwrap();
        let (fm2, body2) = parse_memory_fact(&emitted).unwrap();
        assert_eq!(fm, fm2);
        assert_eq!(body, body2);
        assert_eq!(fm2.links, vec!["distributed-idea-market", "other-idea"]);
    }

    #[test]
    fn compacted_roundtrip_preserves_every_field_and_body() {
        let fm = CompactedFrontmatter {
            compacted_through: 7,
            covered_bytes: 15234,
            turn_count_at_compaction: 12,
            model: "qwen3-8b-local".into(),
            updated: dt("2026-07-07T10:15:00Z"),
        };
        let body = "## Decisions\n- kept the sidecar\n## Open threads\n- none\n";
        let emitted = emit_compacted(&fm, body).unwrap();
        let (fm2, body2) = parse_compacted(&emitted).unwrap();
        assert_eq!(fm, fm2);
        assert_eq!(body, body2);
    }

    #[test]
    fn artifact_roundtrip_finding_with_lens() {
        let fm = ArtifactFrontmatter {
            slug: "20260708-193045-key-decisions".into(),
            title: "Key decisions".into(),
            kind: ArtifactKind::Finding,
            lens: Some("extract-key-decisions".into()),
            created: dt("2026-07-08T19:30:45Z"),
            model: "qwen3-8b-local".into(),
        };
        let body = "- decided the sidecar stays\n";
        let emitted = emit_artifact(&fm, body).unwrap();
        let (fm2, body2) = parse_artifact(&emitted).unwrap();
        assert_eq!(fm, fm2);
        assert_eq!(body, body2);
    }

    #[test]
    fn artifact_roundtrip_synthesis_without_lens() {
        let fm = ArtifactFrontmatter {
            slug: "20260708-193045-synthesis".into(),
            title: "Knowledge synthesis".into(),
            kind: ArtifactKind::Synthesis,
            lens: None,
            created: dt("2026-07-08T19:30:45Z"),
            model: "claude-code".into(),
        };
        let emitted = emit_artifact(&fm, "Converged summary.\n").unwrap();
        let (fm2, body2) = parse_artifact(&emitted).unwrap();
        assert_eq!(fm, fm2);
        assert_eq!(fm2.lens, None);
        assert_eq!(body2, "Converged summary.\n");
    }

    #[test]
    fn parse_artifact_missing_fence_errors() {
        let err = parse_artifact("no fence").unwrap_err();
        assert!(matches!(err, DomainError::MissingFrontmatter));
    }

    #[test]
    fn parse_artifact_bad_kind_errors() {
        let input = "---\n\
slug: x\n\
title: X\n\
kind: not_a_kind\n\
created: 2026-01-01T00:00:00Z\n\
model: m\n\
---\n\
body\n";
        let err = parse_artifact(input).unwrap_err();
        assert!(matches!(err, DomainError::Yaml(_)));
    }

    #[test]
    fn memory_fact_missing_fence_errors() {
        let err = parse_memory_fact("plain text, no frontmatter").unwrap_err();
        assert!(matches!(err, DomainError::MissingFrontmatter));
    }

    #[test]
    fn idea_tags_default_to_empty_when_absent() {
        let input = "---\n\
title: X\n\
slug: x\n\
state: draft\n\
created: 2026-01-01T00:00:00Z\n\
updated: 2026-01-01T00:00:00Z\n\
---\n\
body\n";
        let (fm, _) = parse_idea(input).unwrap();
        assert_eq!(fm.tags, Vec::<String>::new());
    }
}
