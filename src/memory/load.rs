//! Reopen-time memory context reload (docs/06-concepts/memory.md D13).
//!
//! Truth-idempotent: this module only reads. The state flip to `reopened` is the web route's
//! write (via `vault::store::write_idea`); body and memory change only on Store (D9).

use std::path::Path;

use crate::ai::budget::{assemble_context, AssembledContext, ContextBudget, ContextInput};
use crate::memory::MemoryError;
use crate::vault::store;

/// Turn splitting is owned by `vault::store` (it owns the conversation.md format); re-exported
/// here for `memory`-internal callers.
pub use crate::vault::store::split_turns;

/// Assemble the context block for `Stored→Reopened` (D13), under `budget` (D21):
///
/// - `MEMORY.md` is always loaded first (the cheap one-line-per-fact index) — its lines are the
///   highest-ranked memory entries;
/// - full fact bodies follow, most recent first, pulled in only as budget allows;
/// - the most recent conversation turns fill whatever remains (trimmed from the oldest).
///
/// Returns the assembled context plus inclusion counts; the caller feeds `.text` into the next
/// chat turn so the AI "remembers".
pub fn load_context(
    vault_dir: &Path,
    slug: &str,
    budget: ContextBudget,
) -> Result<AssembledContext, MemoryError> {
    let idea = store::read_idea(vault_dir, slug)?;
    let conversation = store::read_conversation(vault_dir, slug)?;

    // Index first (cheap, always) …
    let index = store::read_memory_index(vault_dir, slug)?;
    let mut memory: Vec<String> = index
        .entries
        .iter()
        .map(|e| format!("[[{}]] — {}", e.slug, e.summary))
        .collect();

    // … then full fact bodies selectively, most recent first (D13 "by relevance/recency").
    let mut facts = store::read_memory_facts(vault_dir, slug)?;
    facts.sort_by_key(|b| std::cmp::Reverse(b.frontmatter.created));
    memory.extend(
        facts
            .iter()
            .map(|f| format!("{}: {}", f.frontmatter.title, f.body.trim())),
    );

    // Auto-compact (docs/adr/0012): if a valid `compacted.md` covers a prefix of the transcript,
    // feed its rolling summary + the verbatim tail instead of the whole history. Pure-read, no
    // LLM, no lock — the fold already happened in the prior job phase. `effective_window` does the
    // O(k) fingerprint check; a mismatch (mutated prefix) falls back to the full transcript.
    let turns = split_turns(&conversation);
    let compacted = store::read_compacted(vault_dir, slug)?;
    let win = crate::memory::compact::effective_window(&turns, compacted.as_ref());
    let (summary, tail): (Option<&str>, &[String]) = match win.applied {
        Some(k) => (compacted.as_ref().map(|c| c.summary.as_str()), &turns[k..]),
        None => (None, &turns[..]),
    };
    Ok(assemble_context(
        budget,
        ContextInput {
            idea_body: &idea.body,
            memory: &memory,
            summary,
            turns: tail,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_turns_splits_on_headings_keeping_content() {
        let convo = "## user\nfirst question\nmore\n## assistant\nanswer\n## user\nfollow-up\n";
        let turns = split_turns(convo);
        assert_eq!(turns.len(), 3);
        assert_eq!(turns[0], "## user\nfirst question\nmore\n");
        assert_eq!(turns[2], "## user\nfollow-up\n");
    }

    #[test]
    fn split_turns_handles_empty_and_headingless_input() {
        assert!(split_turns("").is_empty());
        assert!(split_turns("\n\n").is_empty());
        let turns = split_turns("no heading, just text\n");
        assert_eq!(turns.len(), 1);
    }

    use crate::domain::{Compacted, CompactedFrontmatter, Idea, IdeaFrontmatter, IdeaState};
    use chrono::{TimeZone, Utc};

    fn seed_idea(vault: &Path, slug: &str) {
        store::write_idea(
            vault,
            &Idea {
                frontmatter: IdeaFrontmatter {
                    title: "T".into(),
                    slug: slug.into(),
                    state: IdeaState::InDiscussion,
                    tags: vec![],
                    created: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
                    updated: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
                },
                body: "idea body\n".into(),
            },
        )
        .unwrap();
    }

    fn prefix_bytes(turns: &[String], k: usize) -> usize {
        turns.iter().take(k).map(|t| t.trim_end().len() + 1).sum()
    }

    #[test]
    fn load_context_applies_summary_and_tail_on_valid_fingerprint() {
        let tmp = tempfile::tempdir().unwrap();
        seed_idea(tmp.path(), "i");
        for msg in ["first", "second", "third", "fourth"] {
            store::append_turn(tmp.path(), "i", "user", msg).unwrap();
        }
        let convo = store::read_conversation(tmp.path(), "i").unwrap();
        let turns = split_turns(&convo);
        let k = 2;
        store::write_compacted(
            tmp.path(),
            "i",
            &Compacted {
                frontmatter: CompactedFrontmatter {
                    compacted_through: k,
                    covered_bytes: prefix_bytes(&turns, k),
                    turn_count_at_compaction: turns.len(),
                    model: "test".into(),
                    updated: Utc::now(),
                },
                summary: "## Decisions\n- ROLLED-UP-HEAD".into(),
            },
        )
        .unwrap();

        let out = load_context(tmp.path(), "i", ContextBudget::new(16 * 1024)).unwrap();
        assert!(out.included_summary);
        assert!(out.text.contains("ROLLED-UP-HEAD"));
        // The folded head turns are gone; the verbatim tail remains.
        assert!(!out.text.contains("first") && !out.text.contains("second"));
        assert!(out.text.contains("third") && out.text.contains("fourth"));
    }

    #[test]
    fn load_context_falls_back_to_full_transcript_on_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        seed_idea(tmp.path(), "i");
        for msg in ["first", "second", "third"] {
            store::append_turn(tmp.path(), "i", "user", msg).unwrap();
        }
        // Deliberately wrong covered_bytes → stale fingerprint → full-transcript fallback.
        store::write_compacted(
            tmp.path(),
            "i",
            &Compacted {
                frontmatter: CompactedFrontmatter {
                    compacted_through: 2,
                    covered_bytes: 999_999,
                    turn_count_at_compaction: 3,
                    model: "test".into(),
                    updated: Utc::now(),
                },
                summary: "STALE-SUMMARY".into(),
            },
        )
        .unwrap();

        let out = load_context(tmp.path(), "i", ContextBudget::new(16 * 1024)).unwrap();
        assert!(!out.included_summary);
        assert!(!out.text.contains("STALE-SUMMARY"));
        assert!(out.text.contains("first") && out.text.contains("third"));
    }
}
