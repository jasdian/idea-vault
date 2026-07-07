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
    facts.sort_by(|a, b| b.frontmatter.created.cmp(&a.frontmatter.created));
    memory.extend(
        facts
            .iter()
            .map(|f| format!("{}: {}", f.frontmatter.title, f.body.trim())),
    );

    let turns = split_turns(&conversation);
    Ok(assemble_context(
        budget,
        ContextInput {
            idea_body: &idea.body,
            memory: &memory,
            turns: &turns,
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
}
