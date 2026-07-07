//! Store-time memory extraction (docs/06-concepts/memory.md D12).
//!
//! Fires on the `InDiscussion→Stored` (or `Reopened→Stored`) transition: consolidate the idea
//! body to the current best statement *first*, then distil a bounded set of durable facts from
//! the discussion. On re-store, merge + dedupe against existing `memory/*.md` — memory only
//! grows or consolidates, never silently drops (D9 invariant). All AI calls complete before the
//! first byte of truth is written, so an unreachable model leaves the vault untouched. Markdown
//! is written before any index upsert — the caller (web route) reindexes afterwards (ADR-0002).

use std::path::Path;

use chrono::Utc;

use crate::ai::budget::{assemble_context, ContextBudget, ContextInput};
use crate::ai::ollama::ChatMessage;
use crate::ai::LlmBackend;
use crate::domain::{links, slug as domain_slug};
use crate::domain::{IdeaState, MemoryFact, MemoryFactFrontmatter, MemoryIndex};
use crate::memory::load::split_turns;
use crate::memory::MemoryError;
use crate::vault::store;

/// Bounded fact set per extraction (D12: "a small number of high-value facts, not a transcript
/// dump") — extra candidates from the model are dropped.
pub const MAX_FACTS: usize = 7;

const CONSOLIDATE_INSTRUCTION: &str = "You are consolidating an idea after a working discussion. \
Rewrite the idea's current best statement as a short markdown document reflecting the \
conclusions reached. Output only the new statement, no preamble.";

const EXTRACT_INSTRUCTION: &str = "Extract the durable conclusions from this idea discussion as \
at most 7 facts. Format each fact EXACTLY as a line `FACT: <short title>` followed by a 1-3 \
sentence body on the next line(s). Output nothing else.";

/// What a Store produced, for the caller to render and log.
#[derive(Debug)]
pub struct StoreOutcome {
    pub consolidated_body: String,
    /// Newly written facts (existing ones are never rewritten or dropped).
    pub new_facts: usize,
    pub index: MemoryIndex,
}

/// Parse the model's `FACT: <title>` blocks into (title, body) pairs, capped at [`MAX_FACTS`].
/// Defensive: junk before the first `FACT:` line and empty titles/bodies are skipped — local
/// models are not reliable formatters.
fn parse_facts(raw: &str) -> Vec<(String, String)> {
    let mut facts: Vec<(String, String)> = Vec::new();
    let mut current: Option<(String, String)> = None;

    for line in raw.lines() {
        if let Some(title) = line.trim_start().strip_prefix("FACT:") {
            if let Some(done) = current.take() {
                facts.push(done);
            }
            let title = title.trim();
            if !title.is_empty() {
                current = Some((title.to_string(), String::new()));
            }
        } else if let Some((_, body)) = current.as_mut() {
            body.push_str(line);
            body.push('\n');
        }
    }
    if let Some(done) = current.take() {
        facts.push(done);
    }

    facts
        .into_iter()
        .map(|(t, b)| (t, b.trim().to_string()))
        .filter(|(_, b)| !b.is_empty())
        .take(MAX_FACTS)
        .collect()
}

/// Run the full D12 store pipeline for `slug`: consolidate → distil → merge/dedupe → write
/// truth (idea.md body + `state=stored`, new `memory/*.md`, rebuilt MEMORY.md).
///
/// The conversation itself is never touched (append-only invariant). Guard conditions ("≥1 turn
/// exists", D9) are the caller's job. The caller must reindex afterwards — truth first.
pub async fn extract_and_store(
    ollama: &LlmBackend,
    ai_semaphore: &tokio::sync::Semaphore,
    vault_dir: &Path,
    slug: &str,
    budget: ContextBudget,
) -> Result<StoreOutcome, MemoryError> {
    let mut idea = store::read_idea(vault_dir, slug)?;
    let conversation = store::read_conversation(vault_dir, slug)?;
    let existing = store::read_memory_facts(vault_dir, slug)?;

    let turns = split_turns(&conversation);
    let context = assemble_context(
        budget,
        ContextInput {
            idea_body: &idea.body,
            memory: &[],
            turns: &turns,
        },
    );

    // Both AI calls happen BEFORE any write: a model failure aborts the store with truth
    // intact. One permit covers exactly the two sequential calls (one bounded operation,
    // ADR-0006) and is released before parsing/writes — callers must NOT already hold one.
    let (consolidated, facts_raw) = {
        let _permit = ai_semaphore
            .acquire()
            .await
            .map_err(|_| MemoryError::SemaphoreClosed)?;
        let consolidated = ollama
            .chat(vec![ChatMessage {
                role: "user".to_string(),
                content: format!("{CONSOLIDATE_INSTRUCTION}\n\n{}", context.text),
            }])
            .await?;
        let facts_raw = ollama
            .chat(vec![ChatMessage {
                role: "user".to_string(),
                content: format!("{EXTRACT_INSTRUCTION}\n\n{}", context.text),
            }])
            .await?;
        (consolidated, facts_raw)
    };

    // Consolidate-then-distil (D12): the body is rewritten to the best statement; an empty
    // model response falls back to keeping the current body rather than erasing truth.
    let consolidated = consolidated.trim();
    if !consolidated.is_empty() {
        idea.body = format!("{consolidated}\n");
    }

    // Merge + dedupe by fact slug: a re-extracted fact whose title slugifies to an existing
    // fact's slug is a duplicate and is skipped; existing facts are never dropped (D9).
    let mut taken: Vec<String> = existing
        .iter()
        .map(|f| f.frontmatter.slug.clone())
        .collect();
    let now = Utc::now();
    let mut new_facts: Vec<MemoryFact> = Vec::new();
    let candidates = parse_facts(&facts_raw);
    if candidates.is_empty() && !facts_raw.trim().is_empty() {
        // A "successful" store that extracted nothing is usually the model ignoring the FACT:
        // format — surface it rather than silently storing factless (D24: surface, not swallow).
        tracing::warn!(slug, "fact extraction yielded no parseable FACT: blocks");
    }
    for (title, body) in candidates {
        let fact_slug = match domain_slug::try_slugify(&title) {
            // A junk title (emoji-only, symbols) must not alias a real fact via the shared
            // "idea" fallback — give it its own disambiguated slug instead of dedupe-skipping.
            None => domain_slug::disambiguate("fact", |c| taken.iter().any(|s| s == c)),
            Some(base) => {
                if taken.iter().any(|s| s == &base) {
                    continue; // duplicate of an existing fact — dedupe (memory only grows)
                }
                base
            }
        };
        taken.push(fact_slug.clone());
        new_facts.push(MemoryFact {
            frontmatter: MemoryFactFrontmatter {
                slug: fact_slug,
                title,
                tags: Vec::new(),
                created: now,
                links: links::extract_links(&body),
            },
            body: format!("{body}\n"),
        });
    }

    // Writes, in D12 order: consolidated idea.md (state=stored) → facts → MEMORY.md.
    idea.frontmatter.state = IdeaState::Stored;
    idea.frontmatter.updated = now;
    store::write_idea(vault_dir, &idea)?;
    for fact in &new_facts {
        store::write_memory_fact(vault_dir, slug, fact)?;
    }
    let index = store::rebuild_memory_index(vault_dir, slug)?;

    Ok(StoreOutcome {
        consolidated_body: idea.body,
        new_facts: new_facts.len(),
        index,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_facts_reads_fact_blocks_and_skips_junk() {
        let raw = "Sure! Here are the facts:\n\
                   FACT: First insight\nThe body of one.\n\
                   FACT:   \nno title, skipped\n\
                   FACT: Second insight\nBody two\nstill body two.\n";
        let facts = parse_facts(raw);
        assert_eq!(facts.len(), 2);
        assert_eq!(facts[0].0, "First insight");
        assert_eq!(facts[0].1, "The body of one.");
        assert_eq!(facts[1].1, "Body two\nstill body two.");
    }

    #[test]
    fn parse_facts_caps_at_max_facts_and_drops_bodyless() {
        let mut raw = String::from("FACT: Bodyless\nFACT: Kept\nbody\n");
        for i in 0..10 {
            raw.push_str(&format!("FACT: extra {i}\nbody {i}\n"));
        }
        let facts = parse_facts(&raw);
        assert_eq!(facts.len(), MAX_FACTS, "cap is exact, not off-by-one");
        assert_eq!(facts[0].0, "Kept", "bodyless fact dropped");
    }

    #[test]
    fn junk_title_never_aliases_a_real_fact_slug() {
        // Both an emoji-only and a symbol-only title would fall back to "idea" under plain
        // slugify — they must get distinct fact slugs, not dedupe-collide with each other
        // (exercised through try_slugify + disambiguate in extract_and_store's merge loop).
        assert_eq!(crate::domain::slug::try_slugify("🎯🎯"), None);
        assert_eq!(crate::domain::slug::try_slugify("!!!"), None);
        assert_eq!(
            crate::domain::slug::try_slugify("Idea"),
            Some("idea".to_string())
        );
    }
}
