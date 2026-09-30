//! Frontmatter schema (docs/03-data-model.md D8) and the `---\n<yaml>\n---\n<body>` fence
//! parse/emit functions used for both `idea.md` and `memory/<fact-slug>.md`.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};

use crate::domain::artifact::ArtifactKind;
use crate::domain::idea::IdeaState;
use crate::domain::skill::{OutputContract, SkillRole, SkillStage};
use crate::domain::workflow::{parse_stage, StageSpec, WorkflowFrontmatter};
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
    /// The idea's attached reference-source names — keys into the named-source registry.
    /// Frontmatter is the canonical home for this list (SQLite must stay rebuildable from disk).
    /// Empty is skipped on emit so an idea with no sources serializes byte-identically to before.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<String>,
    pub created: DateTime<Utc>,
    pub updated: DateTime<Utc>,
    /// Every frontmatter key the app does not know (an owner's own `aliases:`, a key a newer
    /// idea-vault writes), kept so a rewrite of `idea.md` never drops it. Emitted after the known
    /// keys, sorted by key; the original order of unknown keys is not kept.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_norway::Value>,
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
    /// The build plan this one is a new version of (docs/adr/0032). Absent on every other kind
    /// and on a plan written before plan lineage existed, which is then a version-1 root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revises: Option<String>,
    /// The plan's version in its lineage; `None` reads as 1 (docs/adr/0032).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    /// The `Q#`/`T#` ids the owner answered to make this version (docs/adr/0032).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub answered: Vec<String>,
    /// What made this artifact (ADR-0040). Absent on an artifact written before provenance
    /// existed, which then reads as "provenance unknown", never as stale.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipe: Option<Recipe>,
}

/// An artifact's recipe (ADR-0040): the skill or workflow definition it ran (by digest, so an
/// edit since shows), the parse-coupled prompt templates it used, the build, and every lens whose
/// answer was off its output contract. Truth in the markdown, round-tripped unchanged by reindex.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Recipe {
    /// The skill's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill: Option<String>,
    /// 12 hex digits of the skill file's raw bytes, before `{context}` is filled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_digest: Option<String>,
    /// Where the skill came from: `built-in`, `vault override` or `vault`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_source: Option<String>,
    /// The workflow's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    /// 12 hex digits of the resolved workflow file's raw bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_digest: Option<String>,
    /// `id@vN:digest12` for each parse-coupled template the run used.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub templates: Vec<String>,
    /// `CARGO_PKG_VERSION`, plus `+<sha>` in an image built with `IDEA_VAULT_BUILD_SHA`.
    pub build: String,
    /// `<lens>: off-contract: <violation>`, one per lens whose answer broke its contract.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contract: Vec<String>,
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

/// The structured header of a skill file — a built-in `src/concepts/skills/<name>.md` or an
/// owner-authored `vault/.skills/<name>.md` (docs/adr/0022). The body is the prompt template.
/// Unknown keys are rejected so a typo in an owner's file surfaces on the skill book instead of
/// silently falling back to a default.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillFrontmatter {
    pub name: String,
    pub description: String,
    pub stage: SkillStage,
    #[serde(default)]
    pub role: SkillRole,
    #[serde(default)]
    pub contract: OutputContract,
    /// When to reach for this move — shown on the chip and in the skill book.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub use_when: String,
    /// When not to — the skill book's "wrong turn" column.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub avoid_when: String,
    /// Registered and resolvable, but never offered as a move chip (the `extract-*` lenses).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
    /// The idea slug this skill was distilled from (docs/adr/0042). Absent on hand-written
    /// skills; set by code on a make-skill draft, never taken from the distiller's model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
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

/// Parse a skill file into its frontmatter and prompt template. Trailing whitespace is trimmed
/// from the template so a file's final newline never leaks into the prompt.
pub fn parse_skill(input: &str) -> Result<(SkillFrontmatter, String), DomainError> {
    let (yaml, body) = split_fence(input)?;
    let fm: SkillFrontmatter = serde_norway::from_str(yaml)?;
    Ok((fm, body.trim_end().to_string()))
}

/// Render a skill file from frontmatter and prompt template — how a make-skill draft gets its
/// code-set `origin` (docs/adr/0042). Default-valued optional keys are left out, so the file
/// reads like a hand-written one.
pub fn emit_skill(fm: &SkillFrontmatter, body: &str) -> Result<String, DomainError> {
    let yaml = serde_norway::to_string(fm)?;
    Ok(emit_fence(&yaml, body))
}

/// Parse a workflow file (ADR-0035) into its frontmatter, its stages dispatched on `kind:`, and
/// its body (the owner-facing explanation shown on the book, never a prompt). Unknown keys at the
/// top level or inside any stage are an error, as for a skill file.
pub fn parse_workflow(
    input: &str,
) -> Result<(WorkflowFrontmatter, Vec<StageSpec>, String), DomainError> {
    let (yaml, body) = split_fence(input)?;
    let fm: WorkflowFrontmatter = serde_norway::from_str(yaml)?;
    let stages = fm
        .stages
        .iter()
        .enumerate()
        .map(|(i, v)| parse_stage(i, v).map_err(DomainError::InvalidStage))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((fm, stages, body.trim().to_string()))
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

    /// An `idea.md` exactly as `emit_idea` writes it, carrying keys the app does not know.
    const UNKNOWN_KEYS: &str = "---\n\
title: Distributed idea market\n\
slug: distributed-idea-market\n\
state: in_discussion\n\
tags:\n\
- markets\n\
created: 2026-07-07T10:15:00Z\n\
updated: 2026-07-07T11:40:00Z\n\
aliases:\n\
- Idea bazaar\n\
owner_meta:\n\
\x20\x20priority: 3\n\
\x20\x20reviewed: true\n\
---\n\
\n\
Body text here.\n";

    #[test]
    fn frontmatter_roundtrip_keeps_unknown_keys_with_no_diff() {
        let (fm, body) = parse_idea(UNKNOWN_KEYS).unwrap();
        assert_eq!(
            fm.extra.keys().collect::<Vec<_>>(),
            vec!["aliases", "owner_meta"]
        );
        assert_eq!(emit_idea(&fm, &body).unwrap(), UNKNOWN_KEYS);
    }

    #[test]
    fn frontmatter_roundtrip_writes_known_keys_first_then_unknown_sorted() {
        let shuffled = "---\nzeta: 1\ntitle: T\nalpha: a\nslug: t\nstate: draft\n\
created: 2026-07-07T10:15:00Z\nupdated: 2026-07-07T10:15:00Z\n---\n\nB.\n";
        let (fm, body) = parse_idea(shuffled).unwrap();
        assert_eq!(
            emit_idea(&fm, &body).unwrap(),
            "---\ntitle: T\nslug: t\nstate: draft\ntags: []\n\
created: 2026-07-07T10:15:00Z\nupdated: 2026-07-07T10:15:00Z\nalpha: a\nzeta: 1\n---\n\nB.\n"
        );
    }

    #[test]
    fn frontmatter_roundtrip_without_unknown_keys_is_unchanged() {
        let (fm, body) = parse_idea(DOC_EXAMPLE).unwrap();
        assert!(fm.extra.is_empty());
        let emitted = emit_idea(&fm, &body).unwrap();
        assert!(
            !emitted.contains("extra"),
            "no stray key from the flattened map: {emitted}"
        );
    }

    #[test]
    fn idea_body_separation_preserved_including_blank_lines() {
        let body = "Line one.\n\nLine two.\n";
        let fm = IdeaFrontmatter {
            title: "T".into(),
            slug: "t".into(),
            state: IdeaState::Draft,
            tags: vec![],
            sources: vec![],
            created: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            updated: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            extra: Default::default(),
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
            revises: None,
            version: None,
            answered: Vec::new(),
            recipe: None,
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
            revises: None,
            version: None,
            answered: Vec::new(),
            recipe: None,
        };
        let emitted = emit_artifact(&fm, "Converged summary.\n").unwrap();
        let (fm2, body2) = parse_artifact(&emitted).unwrap();
        assert_eq!(fm, fm2);
        assert_eq!(fm2.lens, None);
        assert_eq!(body2, "Converged summary.\n");
    }

    #[test]
    fn a_build_plan_artifact_round_trips() {
        let input = "---\n\
slug: 20260708-193045-build-prompt\n\
title: Build plan\n\
kind: build_plan\n\
lens: build-prompt\n\
created: 2026-07-08T19:30:45Z\n\
model: claude-code\n\
---\n\
## Goal\n";
        let (fm, body) = parse_artifact(input).unwrap();
        assert_eq!(fm.kind.as_str(), "build_plan");
        assert_eq!(fm.lens.as_deref(), Some("build-prompt"));
        let (fm2, body2) = parse_artifact(&emit_artifact(&fm, &body).unwrap()).unwrap();
        assert_eq!(fm, fm2);
        assert_eq!(body, body2);
    }

    #[test]
    fn artifact_without_lineage_fields_parses_and_skips_on_write() {
        let input = "---\n\
slug: 20260708-193045-build-plan\n\
title: Build plan\n\
kind: build_plan\n\
lens: build-prompt\n\
created: 2026-07-08T19:30:45Z\n\
model: claude-code\n\
---\n\
## Goal\n";
        let (fm, body) = parse_artifact(input).unwrap();
        assert_eq!((fm.revises.as_deref(), fm.version), (None, None));
        assert!(fm.answered.is_empty());
        let emitted = emit_artifact(&fm, &body).unwrap();
        for key in ["revises:", "version:", "answered:"] {
            assert!(!emitted.contains(key), "{key} written: {emitted}");
        }
        let versioned = ArtifactFrontmatter {
            revises: Some("20260708-193045-build-plan".into()),
            version: Some(2),
            answered: vec!["Q6".into(), "T4".into()],
            ..fm
        };
        let (fm2, _) = parse_artifact(&emit_artifact(&versioned, &body).unwrap()).unwrap();
        assert_eq!(fm2, versioned);
    }

    #[test]
    fn artifact_without_recipe_still_parses() {
        let input = "---\n\
slug: 20260708-193045-synthesis\n\
title: Knowledge synthesis\n\
kind: synthesis\n\
created: 2026-07-08T19:30:45Z\n\
model: claude-code\n\
---\n\
Converged.\n";
        let (fm, body) = parse_artifact(input).unwrap();
        assert_eq!(fm.recipe, None);
        let emitted = emit_artifact(&fm, &body).unwrap();
        assert!(!emitted.contains("recipe:"), "{emitted}");
    }

    #[test]
    fn recipe_roundtrips() {
        let fm = ArtifactFrontmatter {
            slug: "20260708-193045-premortem".into(),
            title: "Premortem".into(),
            kind: ArtifactKind::Finding,
            lens: Some("premortem".into()),
            created: dt("2026-07-08T19:30:45Z"),
            model: "qwen3-8b-local".into(),
            revises: None,
            version: None,
            answered: Vec::new(),
            recipe: Some(Recipe {
                skill: Some("premortem".into()),
                skill_digest: Some("3f2a1c9b8d7e".into()),
                skill_source: Some("vault override".into()),
                workflow: Some("interrogate".into()),
                workflow_digest: Some("0123456789ab".into()),
                templates: vec!["audit@v1:abc123def456".into()],
                build: "0.1.0+abc123".into(),
                contract: vec!["premortem: off-contract: the answer was empty".into()],
            }),
        };
        let emitted = emit_artifact(&fm, "- a\n").unwrap();
        let (fm2, body2) = parse_artifact(&emitted).unwrap();
        assert_eq!(fm, fm2);
        assert_eq!(body2, "- a\n");
        let bare = Recipe {
            skill: None,
            skill_digest: None,
            skill_source: None,
            workflow: None,
            workflow_digest: None,
            templates: Vec::new(),
            build: "0.1.0".into(),
            contract: Vec::new(),
        };
        let emitted = emit_artifact(
            &ArtifactFrontmatter {
                recipe: Some(bare),
                ..fm
            },
            "",
        )
        .unwrap();
        assert!(emitted.contains("recipe:\n  build: 0.1.0\n"), "{emitted}");
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

    #[test]
    fn idea_sources_default_to_empty_and_emit_omits_the_key() {
        // Pre-sources ideas (like the doc example) must parse with an empty list and
        // re-serialize byte-identically — no `sources:` key materializing on rewrite.
        let (fm, body) = parse_idea(DOC_EXAMPLE).unwrap();
        assert_eq!(fm.sources, Vec::<String>::new());
        let emitted = emit_idea(&fm, &body).unwrap();
        assert!(
            !emitted.contains("sources"),
            "empty sources must not serialize a key:\n{emitted}"
        );
    }

    #[test]
    fn idea_sources_roundtrip_preserves_names_and_order() {
        let input = "---\n\
title: X\n\
slug: x\n\
state: in_discussion\n\
tags: [markets]\n\
sources: [rf-docs, td-notes]\n\
created: 2026-01-01T00:00:00Z\n\
updated: 2026-01-01T00:00:00Z\n\
---\n\
body\n";
        let (fm, body) = parse_idea(input).unwrap();
        assert_eq!(
            fm.sources,
            vec!["rf-docs".to_string(), "td-notes".to_string()]
        );
        let emitted = emit_idea(&fm, &body).unwrap();
        let (fm2, body2) = parse_idea(&emitted).unwrap();
        assert_eq!(fm, fm2);
        assert_eq!(body, body2);
        assert_eq!(fm2.sources, vec!["rf-docs", "td-notes"]);
    }

    #[test]
    fn parse_skill_reads_every_field_and_trims_the_template() {
        let input = "---\n\
name: steelman\n\
description: Make the strongest case.\n\
stage: steelman\n\
role: advocate\n\
contract: ranked_list\n\
use_when: Before any attack.\n\
avoid_when: Never.\n\
hidden: true\n\
---\n\
\n\
Argue for it.\n\
{context}\n\n";
        let (fm, body) = parse_skill(input).unwrap();
        assert_eq!(fm.name, "steelman");
        assert_eq!(fm.stage, SkillStage::Steelman);
        assert_eq!(fm.role, SkillRole::Advocate);
        assert_eq!(fm.contract, OutputContract::RankedList);
        assert_eq!(fm.use_when, "Before any attack.");
        assert_eq!(fm.avoid_when, "Never.");
        assert!(fm.hidden);
        assert_eq!(body, "Argue for it.\n{context}");
    }

    #[test]
    fn parse_skill_defaults_optional_fields() {
        let input = "---\nname: x\ndescription: d\nstage: attack\n---\n\n{context}\n";
        let (fm, _) = parse_skill(input).unwrap();
        assert_eq!(fm.role, SkillRole::Critic);
        assert_eq!(fm.contract, OutputContract::Free);
        assert!(fm.use_when.is_empty() && fm.avoid_when.is_empty() && !fm.hidden);
    }

    #[test]
    fn parse_skill_rejects_unknown_keys_and_unknown_stages() {
        let typo = "---\nname: x\ndescription: d\nstage: attack\nuse-when: oops\n---\n{context}";
        assert!(matches!(parse_skill(typo), Err(DomainError::Yaml(_))));
        let stage = "---\nname: x\ndescription: d\nstage: dance\n---\n{context}";
        assert!(matches!(parse_skill(stage), Err(DomainError::Yaml(_))));
    }

    #[test]
    fn parse_skill_accepts_origin_and_still_rejects_unknown() {
        let input = "---\nname: x\ndescription: d\nstage: attack\norigin: my-idea\n---\n{context}";
        let (fm, _) = parse_skill(input).unwrap();
        assert_eq!(fm.origin.as_deref(), Some("my-idea"));
        let plain = "---\nname: x\ndescription: d\nstage: attack\n---\n{context}";
        assert_eq!(parse_skill(plain).unwrap().0.origin, None);
        let typo = "---\nname: x\ndescription: d\nstage: attack\norigins: my-idea\n---\n{context}";
        assert!(matches!(parse_skill(typo), Err(DomainError::Yaml(_))));
    }

    #[test]
    fn emit_skill_round_trips_and_omits_defaults() {
        let input = "---\nname: x\ndescription: d\nstage: attack\ncontract: ranked_list\norigin: my-idea\n---\n\nDo it.\n{context}";
        let (fm, body) = parse_skill(input).unwrap();
        let emitted = emit_skill(&fm, &body).unwrap();
        assert_eq!(parse_skill(&emitted).unwrap(), (fm, body));
        assert!(!emitted.contains("hidden") && !emitted.contains("use_when"));
        assert!(emitted.contains("origin: my-idea"), "{emitted}");
    }
}
