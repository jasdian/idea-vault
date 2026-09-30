//! `validate` — a read-only consistency check over the vault's truth files, run by
//! `idea-vault validate` and by the shipping gate on a copy of the golden vault.
//!
//! It checks exactly three things per idea, nothing more (owner decision: consecutive user turns
//! are legitimate, so turn ordering is deliberately not checked):
//!
//! 1. **Frontmatter** — `idea.md` and every `memory/*.md` parse, each declared slug matches its
//!    folder or file name (D22), and no idea folder has lost its `idea.md`.
//! 2. **MEMORY.md coverage** — every memory fact is listed in `MEMORY.md`, and every line of
//!    `MEMORY.md` points at a fact that exists.
//! 3. **Duplicate memories** — no two facts of one idea share a title or a body, compared with
//!    case and whitespace folded.
//!
//! A file that cannot be read or parsed is a finding, not an error: the report names every problem
//! instead of stopping at the first. Only directory-level I/O failures are returned as errors.
//! Nothing is ever written.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::Path;

use crate::domain::frontmatter;
use crate::domain::memory::{MemoryFact, MemoryIndex};
use crate::vault::{store, walk, VaultError};

/// Which of the three checks a finding comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FindingKind {
    Frontmatter,
    MemoryIndex,
    DuplicateMemory,
}

impl FindingKind {
    fn label(self) -> &'static str {
        match self {
            Self::Frontmatter => "frontmatter",
            Self::MemoryIndex => "memory-index",
            Self::DuplicateMemory => "duplicate-memory",
        }
    }
}

/// One problem in one idea. `file` is relative to the idea folder.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Finding {
    pub slug: String,
    pub kind: FindingKind,
    pub file: String,
    pub detail: String,
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {}/{}: {}",
            self.kind.label(),
            self.slug,
            self.file,
            self.detail
        )
    }
}

/// The outcome of one validate pass: how many ideas were checked and what was wrong, sorted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub ideas: usize,
    pub findings: Vec<Finding>,
}

/// Validate every idea under `vault_dir`. A missing directory is an error rather than an empty
/// pass: a wrong or unmounted path must never read as a clean vault (see `walk::walk_ideas`).
pub fn validate_vault(vault_dir: &Path) -> Result<Report, VaultError> {
    if !vault_dir.is_dir() {
        return Err(VaultError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("vault directory {} does not exist", vault_dir.display()),
        )));
    }
    let ideas = walk::walk_ideas(vault_dir)?;
    let mut findings = ideas
        .iter()
        .map(|entry| validate_idea(vault_dir, &entry.slug, &entry.path))
        .collect::<Result<Vec<_>, _>>()?
        .concat();
    findings.extend(headless_idea_dirs(vault_dir)?);
    findings.sort();
    Ok(Report {
        ideas: ideas.len(),
        findings,
    })
}

/// Idea folders that lost their `idea.md`: `walk_ideas` skips them, so without this pass the very
/// corruption `validate` exists to catch would be invisible. A folder counts as an idea folder
/// when it holds any other idea truth file (`conversation.md`, `MEMORY.md` or `memory/`);
/// dot-folders (`.skills/`, `.workflows/`) are app config, and any other stray folder is ignored.
fn headless_idea_dirs(vault_dir: &Path) -> Result<Vec<Finding>, VaultError> {
    let mut found = Vec::new();
    for entry in fs::read_dir(vault_dir)? {
        let path = entry?.path();
        let Some(slug) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let holds_truth = ["conversation.md", "MEMORY.md", "memory"]
            .iter()
            .any(|f| path.join(f).exists());
        if path.is_dir() && !slug.starts_with('.') && !path.join("idea.md").exists() && holds_truth
        {
            found.push(Finding {
                slug: slug.to_string(),
                kind: FindingKind::Frontmatter,
                file: "idea.md".to_string(),
                detail: "missing: the folder holds idea files but no idea.md".to_string(),
            });
        }
    }
    Ok(found)
}

/// Write one line per finding and a closing summary line.
pub fn write_report(report: &Report, out: &mut impl std::io::Write) -> std::io::Result<()> {
    for finding in &report.findings {
        writeln!(out, "{finding}")?;
    }
    writeln!(
        out,
        "validate: {} idea(s), {} finding(s)",
        report.ideas,
        report.findings.len()
    )
}

fn validate_idea(vault_dir: &Path, slug: &str, dir: &Path) -> Result<Vec<Finding>, VaultError> {
    let finding = |kind, file: &str, detail: String| Finding {
        slug: slug.to_string(),
        kind,
        file: file.to_string(),
        detail,
    };

    // The app never opens a folder whose name is not a D22 slug (every store path join refuses
    // it), so name it once and skip the rest: its other checks would only restate this.
    if !crate::domain::slug::is_valid(slug) {
        return Ok(vec![finding(
            FindingKind::Frontmatter,
            "idea.md",
            "folder name is not a valid slug, so idea-vault cannot open this idea".to_string(),
        )]);
    }

    let idea_findings = match read_file(&dir.join("idea.md"))
        .and_then(|raw| frontmatter::parse_idea(&raw).map_err(|e| e.to_string()))
    {
        Err(problem) => vec![finding(FindingKind::Frontmatter, "idea.md", problem)],
        Ok((fm, _)) if fm.slug != slug => vec![finding(
            FindingKind::Frontmatter,
            "idea.md",
            format!("slug {:?} does not match its folder", fm.slug),
        )],
        Ok(_) => Vec::new(),
    };

    let (facts, fact_findings) = read_facts(&dir.join("memory"))?;
    let fact_findings = fact_findings
        .into_iter()
        .map(|(file, detail)| finding(FindingKind::Frontmatter, &file, detail));

    let stems: BTreeSet<&str> = facts.iter().map(|(stem, _)| stem.as_str()).collect();
    let index = store::read_memory_index(vault_dir, slug)?;
    let coverage = memory_coverage(&stems, &index)
        .into_iter()
        .map(|detail| finding(FindingKind::MemoryIndex, "MEMORY.md", detail));

    let duplicates = duplicate_memories(&facts)
        .into_iter()
        .map(|detail| finding(FindingKind::DuplicateMemory, "memory/", detail));

    Ok(idea_findings
        .into_iter()
        .chain(fact_findings)
        .chain(coverage)
        .chain(duplicates)
        .collect())
}

/// Parse every `memory/*.md` one by one, so a broken fact is a finding rather than the end of
/// the scan. Returns the parsed facts keyed by file stem, plus `(file, problem)` pairs.
type ParsedFacts = (Vec<(String, MemoryFact)>, Vec<(String, String)>);

fn read_facts(memory_dir: &Path) -> Result<ParsedFacts, VaultError> {
    let entries = match fs::read_dir(memory_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), Vec::new())),
        Err(e) => return Err(e.into()),
    };
    let mut paths = entries
        .map(|entry| entry.map(|e| e.path()))
        .collect::<Result<Vec<_>, _>>()?;
    paths.retain(|p| p.extension().and_then(|e| e.to_str()) == Some("md"));
    paths.sort();

    let mut facts = Vec::new();
    let mut problems = Vec::new();
    for path in paths {
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()).map(str::to_owned) else {
            continue;
        };
        let file = format!("memory/{stem}.md");
        match read_file(&path)
            .and_then(|raw| frontmatter::parse_memory_fact(&raw).map_err(|e| e.to_string()))
        {
            Err(problem) => problems.push((file, problem)),
            Ok((fm, _)) if fm.slug != stem => problems.push((
                file,
                format!("slug {:?} does not match its file name", fm.slug),
            )),
            Ok((fm, body)) => facts.push((
                stem,
                MemoryFact {
                    frontmatter: fm,
                    body,
                },
            )),
        }
    }
    Ok((facts, problems))
}

/// Read one truth file, turning a per-file failure (not UTF-8, unreadable) into the problem text
/// of a finding, so one bad file never ends the scan.
fn read_file(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|e| format!("unreadable: {e}"))
}

/// Both directions of the `MEMORY.md` ↔ `memory/` pointer contract. Only well-formed
/// `- [title](memory/<slug>.md) — summary` lines count as listed (the same parse the app loads
/// on reopen); a malformed line is not reported itself, its fact shows up as not listed. A fact whose frontmatter
/// failed to parse is absent from `stems`, so it is reported once (as frontmatter) and a
/// `MEMORY.md` line pointing at it is reported here as dangling.
fn memory_coverage(stems: &BTreeSet<&str>, index: &MemoryIndex) -> Vec<String> {
    let listed: BTreeSet<&str> = index.entries.iter().map(|e| e.slug.as_str()).collect();
    let unlisted = stems
        .difference(&listed)
        .map(|s| format!("memory/{s}.md is not listed"));
    let dangling = listed
        .difference(stems)
        .map(|s| format!("lists memory/{s}.md, which is missing or unreadable"));
    unlisted.chain(dangling).collect()
}

/// Facts that repeat another fact's title or body, compared with case and whitespace folded.
fn duplicate_memories(facts: &[(String, MemoryFact)]) -> Vec<String> {
    let groups = |key: fn(&MemoryFact) -> &str| {
        let mut by_key: BTreeMap<String, Vec<&str>> = BTreeMap::new();
        for (stem, fact) in facts {
            let folded = fold(key(fact));
            if !folded.is_empty() {
                by_key.entry(folded).or_default().push(stem);
            }
        }
        by_key.into_values().filter(|stems| stems.len() > 1)
    };
    let titles =
        groups(|f| &f.frontmatter.title).map(|stems| format!("same title: {}", stems.join(", ")));
    let bodies = groups(|f| &f.body).map(|stems| format!("same body: {}", stems.join(", ")));
    titles.chain(bodies).collect()
}

fn fold(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDEA: &str = "---\ntitle: T\nslug: {slug}\nstate: stored\n\
created: 2026-07-07T10:00:00Z\nupdated: 2026-07-07T10:00:00Z\n---\nBody.\n";

    fn idea(vault: &Path, slug: &str) {
        fs::create_dir_all(vault.join(slug).join("memory")).unwrap();
        fs::write(
            vault.join(slug).join("idea.md"),
            IDEA.replace("{slug}", slug),
        )
        .unwrap();
    }

    fn fact(vault: &Path, idea: &str, slug: &str, title: &str, body: &str) {
        let raw = format!(
            "---\nslug: {slug}\ntitle: {title}\ncreated: 2026-07-07T10:00:00Z\n---\n{body}\n"
        );
        fs::write(vault.join(idea).join(format!("memory/{slug}.md")), raw).unwrap();
    }

    fn memory_md(vault: &Path, idea: &str, slugs: &[&str]) {
        let lines: String = slugs
            .iter()
            .map(|s| format!("- [{s}](memory/{s}.md) — summary\n"))
            .collect();
        fs::write(vault.join(idea).join("MEMORY.md"), lines).unwrap();
    }

    fn kinds(report: &Report) -> Vec<(FindingKind, &str)> {
        report
            .findings
            .iter()
            .map(|f| (f.kind, f.file.as_str()))
            .collect()
    }

    #[test]
    fn validate_clean_vault_has_no_findings() {
        let tmp = tempfile::tempdir().unwrap();
        idea(tmp.path(), "a");
        fact(tmp.path(), "a", "one", "One", "First.");
        fact(tmp.path(), "a", "two", "Two", "Second.");
        memory_md(tmp.path(), "a", &["one", "two"]);

        let report = validate_vault(tmp.path()).unwrap();
        assert_eq!(report.ideas, 1);
        assert_eq!(report.findings, vec![]);
    }

    #[test]
    fn validate_golden_vault_fixture_is_clean() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/golden-vault");
        let report = validate_vault(&fixture).unwrap();
        assert!(report.ideas > 0);
        assert_eq!(report.findings, vec![]);
    }

    #[test]
    fn validate_missing_vault_dir_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(validate_vault(&tmp.path().join("nope")).is_err());
    }

    #[test]
    fn validate_flags_broken_and_mismatched_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        idea(tmp.path(), "a");
        fs::write(tmp.path().join("a/idea.md"), "no fence at all\n").unwrap();
        idea(tmp.path(), "b");
        fs::write(
            tmp.path().join("b/idea.md"),
            IDEA.replace("{slug}", "not-b"),
        )
        .unwrap();
        idea(tmp.path(), "c");
        fact(tmp.path(), "c", "good", "Good", "Fine.");
        fact(tmp.path(), "c", "renamed", "Renamed", "Moved.");
        fs::rename(
            tmp.path().join("c/memory/renamed.md"),
            tmp.path().join("c/memory/other.md"),
        )
        .unwrap();
        fs::write(tmp.path().join("c/memory/broken.md"), "---\nslug: [\n---\n").unwrap();
        memory_md(tmp.path(), "c", &["good"]);

        let report = validate_vault(tmp.path()).unwrap();
        assert_eq!(
            kinds(&report),
            vec![
                (FindingKind::Frontmatter, "idea.md"),
                (FindingKind::Frontmatter, "idea.md"),
                (FindingKind::Frontmatter, "memory/broken.md"),
                (FindingKind::Frontmatter, "memory/other.md"),
            ]
        );
        assert!(report.findings[1].detail.contains("not-b"));
        assert!(report.findings[3].detail.contains("renamed"));
    }

    #[test]
    fn validate_flags_memory_index_coverage_both_ways() {
        let tmp = tempfile::tempdir().unwrap();
        idea(tmp.path(), "a");
        fact(tmp.path(), "a", "listed", "Listed", "In the index.");
        fact(tmp.path(), "a", "unlisted", "Unlisted", "Not in the index.");
        memory_md(tmp.path(), "a", &["listed", "ghost"]);

        let report = validate_vault(tmp.path()).unwrap();
        let details: Vec<&str> = report.findings.iter().map(|f| f.detail.as_str()).collect();
        assert_eq!(
            details,
            vec![
                "lists memory/ghost.md, which is missing or unreadable",
                "memory/unlisted.md is not listed",
            ]
        );
        assert!(report
            .findings
            .iter()
            .all(|f| f.kind == FindingKind::MemoryIndex));
    }

    #[test]
    fn validate_flags_facts_without_a_memory_md() {
        let tmp = tempfile::tempdir().unwrap();
        idea(tmp.path(), "a");
        fact(tmp.path(), "a", "orphan", "Orphan", "No index at all.");

        let report = validate_vault(tmp.path()).unwrap();
        assert_eq!(
            kinds(&report),
            vec![(FindingKind::MemoryIndex, "MEMORY.md")]
        );
    }

    #[test]
    fn validate_flags_an_idea_folder_that_lost_its_idea_md() {
        let tmp = tempfile::tempdir().unwrap();
        idea(tmp.path(), "kept");
        idea(tmp.path(), "headless");
        fs::remove_file(tmp.path().join("headless/idea.md")).unwrap();
        fs::write(tmp.path().join("headless/conversation.md"), "## user\nhi\n").unwrap();
        // Not idea folders: a stray directory, and app config under a dot-folder.
        fs::create_dir_all(tmp.path().join("notes")).unwrap();
        fs::write(tmp.path().join("notes/todo.txt"), "x").unwrap();
        fs::create_dir_all(tmp.path().join(".skills/memory")).unwrap();

        let report = validate_vault(tmp.path()).unwrap();
        assert_eq!(report.ideas, 1);
        assert_eq!(kinds(&report), vec![(FindingKind::Frontmatter, "idea.md")]);
        assert_eq!(report.findings[0].slug, "headless");
        assert!(
            report.findings[0].detail.contains("missing"),
            "{:?}",
            report.findings
        );
    }

    #[test]
    fn validate_reports_unreadable_files_and_keeps_going() {
        let tmp = tempfile::tempdir().unwrap();
        idea(tmp.path(), "a");
        fs::write(tmp.path().join("a/idea.md"), [0xff, 0xfe]).unwrap();
        idea(tmp.path(), "b");
        fact(tmp.path(), "b", "good", "Good", "Fine.");
        fs::write(tmp.path().join("b/memory/latin1.md"), [0x2d, 0xe9, 0xff]).unwrap();
        memory_md(tmp.path(), "b", &["good"]);
        idea(tmp.path(), "c");
        fact(tmp.path(), "c", "orphan", "Orphan", "Unlisted.");

        let report = validate_vault(tmp.path()).unwrap();
        assert_eq!(report.ideas, 3);
        assert_eq!(
            kinds(&report),
            vec![
                (FindingKind::Frontmatter, "idea.md"),
                (FindingKind::Frontmatter, "memory/latin1.md"),
                (FindingKind::MemoryIndex, "MEMORY.md"),
            ]
        );
        assert!(
            report.findings[0].detail.contains("unreadable"),
            "{:?}",
            report.findings
        );
        assert!(
            report.findings[1].detail.contains("unreadable"),
            "{:?}",
            report.findings
        );
    }

    #[test]
    fn validate_names_a_folder_with_an_invalid_slug_and_keeps_going() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("My Idea")).unwrap();
        fs::write(
            tmp.path().join("My Idea/idea.md"),
            IDEA.replace("{slug}", "my-idea"),
        )
        .unwrap();
        idea(tmp.path(), "b");
        fact(tmp.path(), "b", "orphan", "Orphan", "Unlisted.");

        let report = validate_vault(tmp.path()).unwrap();
        assert_eq!(
            kinds(&report),
            vec![
                (FindingKind::Frontmatter, "idea.md"),
                (FindingKind::MemoryIndex, "MEMORY.md"),
            ]
        );
        assert_eq!(report.findings[0].slug, "My Idea");
        assert!(
            report.findings[0].detail.contains("not a valid slug"),
            "{:?}",
            report.findings
        );
    }

    #[test]
    fn validate_flags_duplicate_memories_by_title_and_body() {
        let tmp = tempfile::tempdir().unwrap();
        idea(tmp.path(), "a");
        fact(
            tmp.path(),
            "a",
            "one",
            "Pricing floor",
            "Charge at least 5.",
        );
        fact(tmp.path(), "a", "two", "pricing   FLOOR", "Something else.");
        fact(tmp.path(), "a", "three", "Other", "charge at least\n5.");
        memory_md(tmp.path(), "a", &["one", "three", "two"]);

        let report = validate_vault(tmp.path()).unwrap();
        let details: Vec<&str> = report.findings.iter().map(|f| f.detail.as_str()).collect();
        assert_eq!(
            details,
            vec!["same body: one, three", "same title: one, two"]
        );
        assert!(report
            .findings
            .iter()
            .all(|f| f.kind == FindingKind::DuplicateMemory));
    }

    #[test]
    fn validate_report_prints_one_line_per_finding_and_a_summary() {
        let report = Report {
            ideas: 2,
            findings: vec![Finding {
                slug: "a".into(),
                kind: FindingKind::MemoryIndex,
                file: "MEMORY.md".into(),
                detail: "memory/x.md is not listed".into(),
            }],
        };
        let mut out = Vec::new();
        write_report(&report, &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "memory-index a/MEMORY.md: memory/x.md is not listed\n\
             validate: 2 idea(s), 1 finding(s)\n"
        );
    }
}
