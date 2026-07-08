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
sentence body on the next line(s). When a fact builds on, constrains, or contradicts ANOTHER \
fact in your list, reference that other fact inside its body by its exact title in double \
square brackets, e.g. `this only holds if [[cheapest disproof comes first]]`. After the last \
fact, end with ONE final line `TAGS: <3-5 short lowercase topic tags, comma-separated>` \
classifying the idea (domain, kind, stage). Output nothing else.";

/// Cap on model-suggested tags merged into the idea per store (owner-set tags are never
/// removed; the model only ever adds).
const MAX_NEW_TAGS: usize = 5;

/// Parse the extraction output's final `TAGS: a, b, c` line into clean slug-form tags: split on
/// commas, slugify each (dropping junk the same way fact titles do), dedupe, cap. The LAST such
/// line wins if the model emits several (defensive, like [`parse_facts`]).
fn parse_tags(raw: &str) -> Vec<String> {
    let Some(line) = raw
        .lines()
        .rev()
        .find_map(|l| l.trim_start().strip_prefix("TAGS:"))
    else {
        return Vec::new();
    };
    let mut tags: Vec<String> = Vec::new();
    for token in line.split(',') {
        if let Some(tag) = domain_slug::try_slugify(token.trim()) {
            if !tags.iter().any(|t| t == &tag) {
                tags.push(tag);
            }
        }
        if tags.len() == MAX_NEW_TAGS {
            break;
        }
    }
    tags
}

/// Minimum title length (chars) for the unbracketed title-mention auto-link in
/// [`cross_link`] — short generic titles ("scope", "risks") would spray links everywhere.
const MIN_TITLE_LINK_CHARS: usize = 12;

/// Cross-link one freshly-extracted fact against the whole known fact set (this batch + what is
/// already on disk) — the deterministic leaf behind "link related memories with `[[slug]]`"
/// (docs/03). The model can't know slugs (they are derived server-side from titles), so the
/// prompt asks it to reference related facts by TITLE in `[[…]]`; this pass makes those
/// references canonical and mines the frontmatter `links`:
/// 1. Every `[[text]]` whose slugified text matches a known fact becomes `[[that-slug]]` in the
///    persisted body — so `extract_links`, reindex's backlink mining, and a reader's mental
///    model all see the same canonical form. Unknown references stay verbatim (dangling links
///    are legal, same as idea bodies).
/// 2. A body that quotes another fact's exact title unbracketed (case-insensitive, and only for
///    titles ≥ [`MIN_TITLE_LINK_CHARS`] so short generic titles don't spray links) links to it
///    too — small local models often mention the related fact but forget the brackets.
///
/// Returns the normalized body and the deduped link list (self-references dropped).
fn cross_link(self_slug: &str, body: &str, known: &[(String, String)]) -> (String, Vec<String>) {
    // Pass 1: normalize [[Title Form]] → [[slug]].
    let mut normalized = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(start) = rest.find("[[") {
        let after_open = &rest[start + 2..];
        let Some(end) = after_open.find("]]") else {
            break;
        };
        let inner = &after_open[..end];
        let canonical = domain_slug::try_slugify(inner)
            .filter(|candidate| known.iter().any(|(s, _)| s == candidate))
            .unwrap_or_else(|| inner.to_string());
        normalized.push_str(&rest[..start]);
        normalized.push_str("[[");
        normalized.push_str(&canonical);
        normalized.push_str("]]");
        rest = &after_open[end + 2..];
    }
    normalized.push_str(rest);

    let mut fact_links = links::extract_links(&normalized);
    // Pass 2: unbracketed title mentions.
    let lower = normalized.to_lowercase();
    for (k_slug, k_title) in known {
        if k_slug == self_slug || fact_links.iter().any(|l| l == k_slug) {
            continue;
        }
        if k_title.chars().count() >= MIN_TITLE_LINK_CHARS
            && lower.contains(&k_title.to_lowercase())
        {
            fact_links.push(k_slug.clone());
        }
    }
    fact_links.retain(|l| l != self_slug);
    (normalized, fact_links)
}

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
        } else if line.trim_start().starts_with("TAGS:") {
            // The trailing tag line (parsed separately by `parse_tags`) is metadata, not the
            // last fact's body.
            if let Some(done) = current.take() {
                facts.push(done);
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
            // Store distils the FULL verbatim transcript (the high-fidelity backstop); it never
            // substitutes the lossy rolling summary and never reads compacted.md (docs/adr/0012).
            summary: None,
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
    // First pass: settle every new fact's slug (the batch's slugs must all be known before any
    // body can be cross-linked against them).
    let mut drafts: Vec<(String, String, String)> = Vec::new(); // (slug, title, body)
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
        drafts.push((fact_slug, title, body));
    }

    // Second pass: cross-link (the memory-graph leaf — "link related memories with [[slug]]").
    // Candidates are the batch itself plus what is already on disk, so a re-store can link new
    // facts back into old ones.
    let known: Vec<(String, String)> = existing
        .iter()
        .map(|f| (f.frontmatter.slug.clone(), f.frontmatter.title.clone()))
        .chain(drafts.iter().map(|(s, t, _)| (s.clone(), t.clone())))
        .collect();
    for (fact_slug, title, body) in drafts {
        let (body, fact_links) = cross_link(&fact_slug, &body, &known);
        new_facts.push(MemoryFact {
            frontmatter: MemoryFactFrontmatter {
                slug: fact_slug,
                title,
                tags: Vec::new(),
                created: now,
                links: fact_links,
            },
            body: format!("{body}\n"),
        });
    }

    // Merge model-suggested tags (additive only: owner-set tags are never removed, and a
    // re-store can only grow the set — same "memory only grows" posture as facts). The tags
    // land in idea.md frontmatter, which reindex mines into the tags tables and the
    // `kind='tags'` search rows.
    for tag in parse_tags(&facts_raw) {
        if !idea.frontmatter.tags.iter().any(|t| t == &tag) {
            idea.frontmatter.tags.push(tag);
        }
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
    fn parse_tags_reads_the_last_tags_line_slugified_and_capped() {
        let raw = "FACT: One\nBody.\nTAGS: ignored, earlier\nFACT: Two\nBody two.\n\
                   TAGS: Trading Tools, MVP!, trading tools, a, b, c, d, e\n";
        assert_eq!(
            parse_tags(raw),
            vec!["trading-tools", "mvp", "a", "b", "c"],
            "last line wins; slugified; deduped; capped at MAX_NEW_TAGS"
        );
        assert!(
            parse_tags("FACT: One\nBody.\n").is_empty(),
            "no TAGS line ⇒ none"
        );
    }

    #[test]
    fn parse_facts_does_not_swallow_the_tags_line_into_a_body() {
        let facts = parse_facts("FACT: One\nBody line.\nTAGS: x, y\n");
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].1, "Body line.", "TAGS: is metadata, not body");
    }

    #[test]
    fn cross_link_normalizes_title_refs_and_mines_links() {
        let known = vec![
            (
                "cheapest-disproof-comes-first".to_string(),
                "Cheapest disproof comes first".to_string(),
            ),
            (
                "sim-live-parity-is-the-game".to_string(),
                "Sim live parity is the game".to_string(),
            ),
        ];
        let (body, links) = cross_link(
            "self-fact",
            "This only holds if [[Cheapest Disproof Comes First]] is done.",
            &known,
        );
        assert!(body.contains("[[cheapest-disproof-comes-first]]"), "{body}");
        assert_eq!(links, vec!["cheapest-disproof-comes-first"]);
    }

    #[test]
    fn cross_link_auto_links_unbracketed_title_mentions() {
        let known = vec![(
            "sim-live-parity-is-the-game".to_string(),
            "Sim live parity is the game".to_string(),
        )];
        let (_, links) = cross_link(
            "self-fact",
            "Remember that sim live parity is the game here.",
            &known,
        );
        assert_eq!(links, vec!["sim-live-parity-is-the-game"]);
    }

    #[test]
    fn cross_link_skips_self_short_titles_and_unknown_refs() {
        let known = vec![
            (
                "self-fact".to_string(),
                "A fact that mentions itself right here".to_string(),
            ),
            ("scope".to_string(), "scope".to_string()), // < MIN_TITLE_LINK_CHARS
        ];
        let (body, links) = cross_link(
            "self-fact",
            "A fact that mentions itself right here; the scope is unclear; see [[some-unknown-thing]].",
            &known,
        );
        // The dangling-but-valid-slug ref IS a link (same contract as idea bodies: the
        // backlinks table tolerates unresolved targets) — what must NOT appear is the self
        // link or the too-short title mention.
        assert_eq!(links, vec!["some-unknown-thing"]);
        assert!(
            body.contains("[[some-unknown-thing]]"),
            "unknown ref stays verbatim"
        );
    }

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
