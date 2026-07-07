//! Read/write `idea.md`, `conversation.md`, `memory/*.md`, and `MEMORY.md` — the vault's on-disk
//! contract (docs/03-data-model.md D7). Write order is always markdown (truth) first, then any
//! index upsert happens in the caller (`index` module); this module never touches SQLite
//! (docs/03-data-model.md "Consistency & failure model", docs/adr/0002).
//!
//! Whole-file writes go through a temp-file + rename so a crash mid-write can never leave a
//! half-written truth file; `conversation.md` is the one append-only file and is never rewritten.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::domain::memory::MemoryIndexEntry;
use crate::domain::{frontmatter, Idea, MemoryFact, MemoryIndex};
use crate::vault::VaultError;

/// Ensure `dir` exists, creating all missing parent components. Idempotent — succeeds if the
/// directory already exists.
pub fn ensure_vault_dir(dir: &Path) -> Result<(), VaultError> {
    fs::create_dir_all(dir)?;
    Ok(())
}

/// Validate `slug` at the filesystem boundary and join it onto `vault_dir`. Every public
/// function takes slugs from callers (URLs, AI-extracted titles) — a slug that fails
/// `domain::slug::is_valid` must never reach a path join (no `../`, no separators).
fn checked_idea_dir(vault_dir: &Path, slug: &str) -> Result<PathBuf, VaultError> {
    if !crate::domain::slug::is_valid(slug) {
        return Err(VaultError::InvalidSlug(slug.to_string()));
    }
    Ok(vault_dir.join(slug))
}

/// Write `contents` to `path` via a unique sibling `*.tmp-*` file + rename, so truth files are
/// never left half-written and concurrent writers to the same target cannot consume each
/// other's temp file. The suffix keeps temp files out of every `.md`-extension scan.
fn write_atomic(path: &Path, contents: &str) -> Result<(), VaultError> {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".tmp-{}-{}", std::process::id(), n));
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, contents)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Parse `vault/<slug>/idea.md` into an `Idea` (frontmatter + body).
pub fn read_idea(vault_dir: &Path, slug: &str) -> Result<Idea, VaultError> {
    let path = checked_idea_dir(vault_dir, slug)?.join("idea.md");
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(VaultError::IdeaNotFound(slug.to_string()))
        }
        Err(e) => return Err(e.into()),
    };
    let (fm, body) = frontmatter::parse_idea(&raw)?;
    Ok(Idea {
        frontmatter: fm,
        body,
    })
}

/// Write `vault/<slug>/idea.md` from an `Idea`, creating the idea directory on first write
/// (Draft creation, D10). Truth-first: this must complete before any caller performs an index
/// upsert (docs/03-data-model.md "Write order"). The slug is the folder name and is never
/// changed by this call (D22).
pub fn write_idea(vault_dir: &Path, idea: &Idea) -> Result<(), VaultError> {
    let dir = checked_idea_dir(vault_dir, &idea.frontmatter.slug)?;
    fs::create_dir_all(&dir)?;
    let rendered = frontmatter::emit_idea(&idea.frontmatter, &idea.body)?;
    write_atomic(&dir.join("idea.md"), &rendered)
}

/// Append one turn of markdown to `vault/<slug>/conversation.md`, creating it on the first turn.
/// `conversation.md` is append-only across every discussion state (docs/04-state-machine.md
/// Invariants) — Store and Reopen only ever append here, never rewrite or truncate.
pub fn append_conversation(
    vault_dir: &Path,
    slug: &str,
    turn_markdown: &str,
) -> Result<(), VaultError> {
    let dir = checked_idea_dir(vault_dir, slug)?;
    if !dir.is_dir() {
        return Err(VaultError::IdeaNotFound(slug.to_string()));
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("conversation.md"))?;
    file.write_all(turn_markdown.as_bytes())?;
    Ok(())
}

/// Read `vault/<slug>/conversation.md`. An idea that has not entered discussion yet has no
/// conversation file — that reads as the empty transcript, not an error.
pub fn read_conversation(vault_dir: &Path, slug: &str) -> Result<String, VaultError> {
    match fs::read_to_string(checked_idea_dir(vault_dir, slug)?.join("conversation.md")) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e.into()),
    }
}

/// Write one `vault/<idea_slug>/memory/<fact-slug>.md` file, creating `memory/` on first write.
/// Memory only appears on the transition to `Stored` — `Draft` has no memory
/// (docs/04-state-machine.md Invariants). Merging/dedupe against existing facts on re-store is
/// the caller's (`memory::extract`) responsibility (D12).
pub fn write_memory_fact(
    vault_dir: &Path,
    idea_slug: &str,
    fact: &MemoryFact,
) -> Result<(), VaultError> {
    let idea_dir = checked_idea_dir(vault_dir, idea_slug)?;
    if !crate::domain::slug::is_valid(&fact.frontmatter.slug) {
        return Err(VaultError::InvalidSlug(fact.frontmatter.slug.clone()));
    }
    // Memory belongs to an existing idea (D7: an idea dir always contains idea.md). Writing a
    // fact into a fabricated dir would create an orphan that `walk_ideas` never discovers.
    if !idea_dir.join("idea.md").is_file() {
        return Err(VaultError::IdeaNotFound(idea_slug.to_string()));
    }
    let dir = idea_dir.join("memory");
    fs::create_dir_all(&dir)?;
    let rendered = frontmatter::emit_memory_fact(&fact.frontmatter, &fact.body)?;
    write_atomic(
        &dir.join(format!("{}.md", fact.frontmatter.slug)),
        &rendered,
    )
}

/// Read and parse every `vault/<idea_slug>/memory/*.md` fact, sorted by fact slug (deterministic
/// order for MEMORY.md rebuilds and reindex). A missing `memory/` dir is an empty fact set.
pub fn read_memory_facts(vault_dir: &Path, idea_slug: &str) -> Result<Vec<MemoryFact>, VaultError> {
    let dir = checked_idea_dir(vault_dir, idea_slug)?.join("memory");
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };

    let mut facts = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let raw = fs::read_to_string(&path)?;
        let (fm, body) = frontmatter::parse_memory_fact(&raw)?;
        facts.push(MemoryFact {
            frontmatter: fm,
            body,
        });
    }
    facts.sort_by(|a, b| a.frontmatter.slug.cmp(&b.frontmatter.slug));
    Ok(facts)
}

/// One MEMORY.md line: `- [<title>](memory/<slug>.md) — <summary>` (docs/06-concepts/memory.md:
/// one-line-per-fact pointer index, the cheap always-on context on reopen).
fn memory_index_line(fact: &MemoryFact) -> (MemoryIndexEntry, String) {
    // Both title and summary are collapsed to a single line — a YAML multi-line title must not
    // break the one-line-per-fact contract of MEMORY.md.
    let title = fact
        .frontmatter
        .title
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or(&fact.frontmatter.slug)
        .to_string();
    let summary = fact
        .body
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or(&title)
        .to_string();
    let line = format!(
        "- [{}](memory/{}.md) — {}\n",
        title, fact.frontmatter.slug, summary
    );
    (
        MemoryIndexEntry {
            slug: fact.frontmatter.slug.clone(),
            summary,
        },
        line,
    )
}

/// Read and parse `vault/<idea_slug>/MEMORY.md` directly — the cheap always-on index load of
/// D13, with no rescan of the `memory/*.md` fact bodies. Missing file = empty index (an idea
/// that was never stored has no memory). Lines that don't match the
/// `- [Title](memory/<slug>.md) — <summary>` shape are skipped defensively.
pub fn read_memory_index(vault_dir: &Path, idea_slug: &str) -> Result<MemoryIndex, VaultError> {
    let raw = match fs::read_to_string(checked_idea_dir(vault_dir, idea_slug)?.join("MEMORY.md")) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(MemoryIndex {
                entries: Vec::new(),
            })
        }
        Err(e) => return Err(e.into()),
    };

    let mut entries = Vec::new();
    for line in raw.lines() {
        let Some(rest) = line.strip_prefix("- [") else {
            continue;
        };
        let Some((_title, rest)) = rest.split_once("](memory/") else {
            continue;
        };
        let Some((slug, summary)) = rest.split_once(".md) — ") else {
            continue;
        };
        // A title containing the separator literals would shift the split; slug validation
        // turns that into a skipped line instead of silently wrong data.
        if !crate::domain::slug::is_valid(slug) {
            continue;
        }
        entries.push(MemoryIndexEntry {
            slug: slug.to_string(),
            summary: summary.to_string(),
        });
    }
    Ok(MemoryIndex { entries })
}

/// Rebuild `vault/<idea_slug>/MEMORY.md` (the one-line-per-fact pointer index) by scanning
/// `vault/<idea_slug>/memory/*.md`, and return the resulting `MemoryIndex`. Entries are sorted
/// by fact slug so rebuilds are deterministic. With no facts on disk this returns an empty index
/// and writes nothing (memory first appears on the transition to `Stored`, D12).
pub fn rebuild_memory_index(vault_dir: &Path, idea_slug: &str) -> Result<MemoryIndex, VaultError> {
    let facts = read_memory_facts(vault_dir, idea_slug)?;
    if facts.is_empty() {
        return Ok(MemoryIndex {
            entries: Vec::new(),
        });
    }

    let mut entries = Vec::with_capacity(facts.len());
    let mut rendered = String::new();
    for fact in &facts {
        let (entry, line) = memory_index_line(fact);
        entries.push(entry);
        rendered.push_str(&line);
    }
    write_atomic(
        &checked_idea_dir(vault_dir, idea_slug)?.join("MEMORY.md"),
        &rendered,
    )?;
    Ok(MemoryIndex { entries })
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;
    use crate::domain::{IdeaFrontmatter, IdeaState, MemoryFactFrontmatter};

    fn sample_idea(slug: &str) -> Idea {
        Idea {
            frontmatter: IdeaFrontmatter {
                title: "Distributed idea market".into(),
                slug: slug.into(),
                state: IdeaState::Draft,
                tags: vec!["markets".into()],
                created: Utc.with_ymd_and_hms(2026, 7, 7, 10, 15, 0).unwrap(),
                updated: Utc.with_ymd_and_hms(2026, 7, 7, 11, 40, 0).unwrap(),
            },
            body: "The current best statement.\n".into(),
        }
    }

    fn sample_fact(slug: &str, title: &str, body: &str) -> MemoryFact {
        MemoryFact {
            frontmatter: MemoryFactFrontmatter {
                slug: slug.into(),
                title: title.into(),
                tags: vec![],
                created: Utc.with_ymd_and_hms(2026, 7, 7, 12, 0, 0).unwrap(),
                links: vec![],
            },
            body: body.into(),
        }
    }

    #[test]
    fn write_then_read_idea_round_trips_and_creates_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let idea = sample_idea("distributed-idea-market");

        write_idea(tmp.path(), &idea).unwrap();
        assert!(tmp.path().join("distributed-idea-market/idea.md").is_file());
        // No stray temp file left behind by the atomic write.
        let leftovers: Vec<_> = fs::read_dir(tmp.path().join("distributed-idea-market"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n != "idea.md")
            .collect();
        assert!(leftovers.is_empty(), "stray files: {leftovers:?}");

        let read = read_idea(tmp.path(), "distributed-idea-market").unwrap();
        assert_eq!(read, idea);
    }

    #[test]
    fn read_idea_missing_is_idea_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let err = read_idea(tmp.path(), "nope").unwrap_err();
        assert!(matches!(err, VaultError::IdeaNotFound(s) if s == "nope"));
    }

    #[test]
    fn conversation_is_append_only_and_created_on_first_turn() {
        let tmp = tempfile::tempdir().unwrap();
        write_idea(tmp.path(), &sample_idea("i")).unwrap();

        assert_eq!(read_conversation(tmp.path(), "i").unwrap(), "");

        append_conversation(tmp.path(), "i", "## user\nfirst\n").unwrap();
        let after_first = read_conversation(tmp.path(), "i").unwrap();

        append_conversation(tmp.path(), "i", "## assistant\nsecond\n").unwrap();
        let after_second = read_conversation(tmp.path(), "i").unwrap();

        // Append-only: earlier content is a strict prefix, never truncated or rewritten.
        assert!(after_second.starts_with(&after_first));
        assert_eq!(after_second, "## user\nfirst\n## assistant\nsecond\n");
    }

    #[test]
    fn append_conversation_to_missing_idea_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let err = append_conversation(tmp.path(), "ghost", "x").unwrap_err();
        assert!(matches!(err, VaultError::IdeaNotFound(_)));
    }

    #[test]
    fn memory_facts_round_trip_and_index_rebuild_is_deterministic() {
        let tmp = tempfile::tempdir().unwrap();
        write_idea(tmp.path(), &sample_idea("i")).unwrap();

        // Written out of slug order on purpose — reads and MEMORY.md must sort by slug.
        write_memory_fact(
            tmp.path(),
            "i",
            &sample_fact("b-second", "Second fact", "Durable conclusion two.\n"),
        )
        .unwrap();
        write_memory_fact(
            tmp.path(),
            "i",
            &sample_fact("a-first", "First fact", "\nDurable conclusion one.\n"),
        )
        .unwrap();

        let facts = read_memory_facts(tmp.path(), "i").unwrap();
        assert_eq!(facts.len(), 2);
        assert_eq!(facts[0].frontmatter.slug, "a-first");
        assert_eq!(facts[1].frontmatter.slug, "b-second");

        let index = rebuild_memory_index(tmp.path(), "i").unwrap();
        assert_eq!(index.entries.len(), 2);
        assert_eq!(index.entries[0].slug, "a-first");
        assert_eq!(index.entries[0].summary, "Durable conclusion one.");

        let memory_md = fs::read_to_string(tmp.path().join("i/MEMORY.md")).unwrap();
        assert_eq!(
            memory_md,
            "- [First fact](memory/a-first.md) — Durable conclusion one.\n\
             - [Second fact](memory/b-second.md) — Durable conclusion two.\n"
        );
    }

    #[test]
    fn write_memory_fact_to_missing_idea_errors_and_creates_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let err =
            write_memory_fact(tmp.path(), "ghost", &sample_fact("f", "F", "b\n")).unwrap_err();
        assert!(matches!(err, VaultError::IdeaNotFound(_)));
        // No orphan `vault/ghost/` dir fabricated (D7: an idea dir always contains idea.md).
        assert!(!tmp.path().join("ghost").exists());
    }

    #[test]
    fn hostile_slugs_are_rejected_before_any_path_join() {
        let tmp = tempfile::tempdir().unwrap();

        assert!(matches!(
            read_idea(tmp.path(), "../escape").unwrap_err(),
            VaultError::InvalidSlug(_)
        ));
        assert!(matches!(
            append_conversation(tmp.path(), "a/b", "x").unwrap_err(),
            VaultError::InvalidSlug(_)
        ));

        let mut idea = sample_idea("ok");
        idea.frontmatter.slug = "../escape".into();
        assert!(matches!(
            write_idea(tmp.path(), &idea).unwrap_err(),
            VaultError::InvalidSlug(_)
        ));

        write_idea(tmp.path(), &sample_idea("i")).unwrap();
        assert!(matches!(
            write_memory_fact(tmp.path(), "i", &sample_fact("../evil", "E", "b\n")).unwrap_err(),
            VaultError::InvalidSlug(_)
        ));
    }

    #[test]
    fn rebuild_memory_index_twice_is_byte_identical() {
        let tmp = tempfile::tempdir().unwrap();
        write_idea(tmp.path(), &sample_idea("i")).unwrap();
        write_memory_fact(tmp.path(), "i", &sample_fact("f", "Fact", "Conclusion.\n")).unwrap();

        let first = rebuild_memory_index(tmp.path(), "i").unwrap();
        let first_bytes = fs::read_to_string(tmp.path().join("i/MEMORY.md")).unwrap();
        let second = rebuild_memory_index(tmp.path(), "i").unwrap();
        let second_bytes = fs::read_to_string(tmp.path().join("i/MEMORY.md")).unwrap();

        assert_eq!(first, second);
        assert_eq!(first_bytes, second_bytes);
    }

    #[test]
    fn multi_line_title_collapses_to_one_memory_md_line() {
        let tmp = tempfile::tempdir().unwrap();
        write_idea(tmp.path(), &sample_idea("i")).unwrap();
        write_memory_fact(
            tmp.path(),
            "i",
            &sample_fact("f", "First line\nsecond line", "Body.\n"),
        )
        .unwrap();

        rebuild_memory_index(tmp.path(), "i").unwrap();
        let memory_md = fs::read_to_string(tmp.path().join("i/MEMORY.md")).unwrap();
        assert_eq!(memory_md, "- [First line](memory/f.md) — Body.\n");
        assert_eq!(memory_md.lines().count(), 1);
    }

    #[test]
    fn rebuild_memory_index_with_no_facts_is_empty_and_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        write_idea(tmp.path(), &sample_idea("i")).unwrap();

        let index = rebuild_memory_index(tmp.path(), "i").unwrap();
        assert!(index.entries.is_empty());
        assert!(!tmp.path().join("i/MEMORY.md").exists());
    }

    #[test]
    fn ensure_vault_dir_creates_nested_subpath_and_is_idempotent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let target = tmp.path().join("nested").join("vault");
        assert!(!target.exists());

        ensure_vault_dir(&target).expect("first create should succeed");
        assert!(target.is_dir());

        ensure_vault_dir(&target).expect("second call should be idempotent");
        assert!(target.is_dir());
    }
}
