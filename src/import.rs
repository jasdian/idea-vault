//! Obsidian / flat-markdown importer (docs/adr/0009 Phase 3): turn a directory of plain `.md`
//! notes into idea-vault ideas, then rebuild the index. Driven by `idea-vault import <dir>`.
//!
//! A bin-level driver over `domain` + `vault` + `index` — it introduces no new primitives. Each
//! note becomes a `Draft` idea under a **path-derived slug** (stable across runs), so re-importing
//! is idempotent: a note whose slug directory already exists is skipped, never duplicated.

use std::collections::HashSet;
use std::path::Path;

use chrono::{DateTime, Utc};
use walkdir::WalkDir;

use crate::domain::{slug as domain_slug, Idea, IdeaFrontmatter, IdeaState};
use crate::index::{reindex, schema};
use crate::vault::store;

/// Counts from an import run.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ImportSummary {
    pub imported: usize,
    pub skipped: usize,
    pub errored: usize,
}

/// Import every `*.md` under `source` into `vault_dir`, then reindex `index_path`.
pub fn import_dir(
    source: &Path,
    vault_dir: &Path,
    index_path: &Path,
) -> anyhow::Result<ImportSummary> {
    store::ensure_vault_dir(vault_dir)?;
    let mut summary = ImportSummary::default();
    // Slugs claimed earlier in this run, so two notes with the same path-slug can't collide.
    let mut claimed: HashSet<String> = HashSet::new();

    for entry in WalkDir::new(source) {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                // A permission-denied / unreadable subtree is counted + logged, not silently lost.
                summary.errored += 1;
                tracing::warn!(error = %e, "skipping unreadable path during import walk");
                continue;
            }
        };
        let path = entry.path();
        if !is_importable_md(source, path) {
            continue;
        }
        match import_one(source, path, vault_dir, &mut claimed) {
            Ok(true) => summary.imported += 1,
            Ok(false) => {
                summary.skipped += 1;
                // Common case: this note was imported on an earlier run (idempotent). Rare case:
                // a *different* source path slugified to the same base slug and claimed it first —
                // that note stays unimported. Logged so a bulk import is auditable, not silent.
                tracing::debug!(path = %path.display(), "skipped (slug already present)");
            }
            Err(e) => {
                summary.errored += 1;
                tracing::warn!(path = %path.display(), error = %e, "skipping unimportable note");
            }
        }
    }

    // Rebuild the derived index from the freshly written truth (ADR-0002).
    let mut conn = schema::open_or_create(index_path)?;
    let counts = reindex::reindex(&mut conn, vault_dir)?;
    tracing::info!(
        ideas = counts.ideas,
        facts = counts.facts,
        links = counts.links,
        "post-import reindex complete"
    );
    Ok(summary)
}

/// A `.md` file that isn't inside an Obsidian/system dir we should skip.
fn is_importable_md(source: &Path, path: &Path) -> bool {
    if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("md") {
        return false;
    }
    let rel = path.strip_prefix(source).unwrap_or(path);
    !rel.components().any(|c| {
        let s = c.as_os_str().to_string_lossy();
        s == ".obsidian" || s == ".trash" || s.starts_with('.')
    })
}

/// Import one note. Returns `Ok(true)` if written, `Ok(false)` if its slug already existed (skip).
fn import_one(
    source: &Path,
    path: &Path,
    vault_dir: &Path,
    claimed: &mut HashSet<String>,
) -> anyhow::Result<bool> {
    let raw = std::fs::read_to_string(path)?;
    let (fm, body) = split_frontmatter(&raw);

    // Slug is derived from the source-relative path (stable across runs → idempotent).
    let rel = path.strip_prefix(source).unwrap_or(path);
    let rel_stem = rel
        .with_extension("")
        .to_string_lossy()
        .replace(['/', '\\'], "-");
    let base_slug = domain_slug::slugify(&rel_stem);
    // Idempotency: the base slug is deterministic per source path, so if its directory already
    // exists this note was imported on an earlier run — skip without disambiguating (otherwise the
    // existing dir would push us to `-2` and duplicate the note).
    if vault_dir.join(&base_slug).is_dir() {
        return Ok(false);
    }
    // Disambiguate only against slugs claimed earlier in *this* run (two distinct source paths that
    // happen to slugify identically).
    let slug = domain_slug::disambiguate(&base_slug, |cand| claimed.contains(cand));
    claimed.insert(slug.clone());

    let title = derive_title(&fm, body, path);
    let tags = derive_tags(&fm, body);
    let modified = file_mtime(path);
    let created = fm_datetime(&fm, "created")
        .or_else(|| fm_datetime(&fm, "date"))
        .unwrap_or(modified);

    let idea = Idea {
        frontmatter: IdeaFrontmatter {
            title,
            slug: slug.clone(),
            state: IdeaState::Draft,
            tags,
            created,
            updated: modified,
        },
        // Rewrite `[[Wiki Links]]` to idea-vault's `[[slug]]` backlink form.
        body: rewrite_wikilinks(body),
    };
    store::create_idea(vault_dir, &idea)?;
    Ok(true)
}

/// Parsed frontmatter mapping + the body after it. Obsidian frontmatter is arbitrary YAML, so we
/// parse it loosely (not through `domain::frontmatter`, which expects idea-vault's exact fields).
type Frontmatter = serde_norway::Mapping;

fn split_frontmatter(raw: &str) -> (Frontmatter, &str) {
    let empty = Frontmatter::new();
    let Some(rest) = raw.strip_prefix("---\n") else {
        return (empty, raw);
    };
    let Some(end) = rest.find("\n---\n").or_else(|| rest.find("\n---")) else {
        return (empty, raw);
    };
    let yaml = &rest[..end];
    let body = rest[end..]
        .trim_start_matches('\n')
        .trim_start_matches("---")
        .trim_start_matches('\n');
    match serde_norway::from_str::<Frontmatter>(yaml) {
        Ok(map) => (map, body),
        Err(_) => (empty, raw),
    }
}

/// Title precedence: frontmatter `title` → first `# H1` → filename stem.
fn derive_title(fm: &Frontmatter, body: &str, path: &Path) -> String {
    if let Some(t) = fm_string(fm, "title") {
        return t;
    }
    for line in body.lines() {
        if let Some(h1) = line.strip_prefix("# ") {
            let h1 = h1.trim();
            if !h1.is_empty() {
                return h1.to_string();
            }
        }
    }
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Untitled".to_string())
}

/// Tags: frontmatter `tags` (string or list) plus inline `#tag` occurrences in the body.
fn derive_tags(fm: &Frontmatter, body: &str) -> Vec<String> {
    let mut tags: Vec<String> = Vec::new();
    if let Some(v) = fm.get("tags") {
        match v {
            serde_norway::Value::String(s) => tags.extend(
                s.split([',', ' '])
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .map(|t| t.trim_start_matches('#').to_string()),
            ),
            serde_norway::Value::Sequence(seq) => tags.extend(
                seq.iter()
                    .filter_map(|x| x.as_str())
                    .map(|t| t.trim_start_matches('#').to_string()),
            ),
            _ => {}
        }
    }
    // Inline #hashtags (skip fenced code lines defensively).
    let mut in_fence = false;
    for line in body.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        for tok in line.split_whitespace() {
            if let Some(tag) = tok.strip_prefix('#') {
                let tag: String = tag
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '-' || *c == '_' || *c == '/')
                    .collect();
                if !tag.is_empty() && tag.chars().any(|c| c.is_alphabetic()) {
                    tags.push(tag);
                }
            }
        }
    }
    tags.sort();
    tags.dedup();
    tags
}

/// Rewrite Obsidian `[[Wiki Link]]` / `[[Link|alias]]` targets to idea-vault `[[slug]]` form.
fn rewrite_wikilinks(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(start) = rest.find("[[") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("]]") else {
            out.push_str(&rest[start..]);
            return out;
        };
        let inner = &after[..end];
        // Obsidian link may be `Target|Alias` or `Target#Heading`; take the target part.
        let target = inner.split(['|', '#']).next().unwrap_or(inner).trim();
        out.push_str("[[");
        out.push_str(&domain_slug::slugify(target));
        out.push_str("]]");
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    // Ensure a trailing newline so the body matches the emit shape.
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn fm_string(fm: &Frontmatter, key: &str) -> Option<String> {
    fm.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn fm_datetime(fm: &Frontmatter, key: &str) -> Option<DateTime<Utc>> {
    let s = fm.get(key).and_then(|v| v.as_str())?;
    DateTime::parse_from_rfc3339(s.trim())
        .map(|d| d.with_timezone(&Utc))
        .ok()
}

fn file_mtime(path: &Path) -> DateTime<Utc> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(DateTime::<Utc>::from)
        .unwrap_or_else(|_| Utc::now())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_frontmatter_extracts_yaml_and_body() {
        let raw = "---\ntitle: My Note\ntags: [a, b]\n---\n\nBody here.\n";
        let (fm, body) = split_frontmatter(raw);
        assert_eq!(fm_string(&fm, "title").as_deref(), Some("My Note"));
        assert_eq!(body.trim(), "Body here.");
    }

    #[test]
    fn split_frontmatter_absent_is_all_body() {
        let (fm, body) = split_frontmatter("no frontmatter\njust text\n");
        assert!(fm.is_empty());
        assert_eq!(body, "no frontmatter\njust text\n");
    }

    #[test]
    fn derive_title_prefers_frontmatter_then_h1_then_stem() {
        let (fm, _) = split_frontmatter("---\ntitle: FM Title\n---\n# H1\n");
        assert_eq!(
            derive_title(&fm, "# H1 body", Path::new("x.md")),
            "FM Title"
        );
        let empty = Frontmatter::new();
        assert_eq!(
            derive_title(&empty, "# The Heading\nx", Path::new("f.md")),
            "The Heading"
        );
        assert_eq!(
            derive_title(&empty, "no heading", Path::new("/a/my-note.md")),
            "my-note"
        );
    }

    #[test]
    fn derive_tags_merges_frontmatter_and_inline() {
        let (fm, body) =
            split_frontmatter("---\ntags: [alpha, beta]\n---\nsome #gamma and #alpha text\n");
        let tags = derive_tags(&fm, body);
        assert_eq!(tags, vec!["alpha", "beta", "gamma"]); // sorted + deduped
    }

    #[test]
    fn rewrite_wikilinks_slugifies_targets_and_strips_alias() {
        let out = rewrite_wikilinks("See [[Distributed Idea Market]] and [[Other Note|alias]].");
        assert!(out.contains("[[distributed-idea-market]]"));
        assert!(out.contains("[[other-note]]"));
        assert!(!out.contains("alias"));
    }

    #[test]
    fn end_to_end_import_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("notes");
        std::fs::create_dir_all(source.join(".obsidian")).unwrap();
        std::fs::write(source.join(".obsidian/app.json"), "{}").unwrap();
        std::fs::write(
            source.join("First Idea.md"),
            "---\ntitle: First Idea\ntags: [markets]\n---\n\nBody with [[Second Idea]].\n",
        )
        .unwrap();
        std::fs::create_dir_all(source.join("sub")).unwrap();
        std::fs::write(
            source.join("sub/Second Idea.md"),
            "# Second Idea\n\nMore text #incentives\n",
        )
        .unwrap();

        let vault = tmp.path().join("vault");
        let index = tmp.path().join("index.db");

        let first = import_dir(&source, &vault, &index).unwrap();
        assert_eq!(first.imported, 2, "two notes imported, .obsidian skipped");

        // First Idea landed as a Draft with its tag and a rewritten wikilink.
        let idea = store::read_idea(&vault, "first-idea").unwrap();
        assert_eq!(idea.frontmatter.state, IdeaState::Draft);
        assert_eq!(idea.frontmatter.tags, vec!["markets".to_string()]);
        assert!(idea.body.contains("[[second-idea]]"));
        // The nested note's slug is path-derived.
        assert!(vault.join("sub-second-idea").is_dir());

        // Re-import: everything skipped, nothing duplicated.
        let second = import_dir(&source, &vault, &index).unwrap();
        assert_eq!(second.imported, 0);
        assert_eq!(second.skipped, 2);
        let dirs = std::fs::read_dir(&vault).unwrap().count();
        assert_eq!(dirs, 2, "no duplicate idea directories on re-import");
    }

    /// `links::extract_links` sees the rewritten targets (importer ↔ backlink integration).
    #[test]
    fn rewritten_links_are_valid_backlinks() {
        let body = rewrite_wikilinks("[[Some Idea]]");
        assert_eq!(
            crate::domain::links::extract_links(&body),
            vec!["some-idea".to_string()]
        );
    }
}
