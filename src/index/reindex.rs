//! Reindex — rebuild the derived SQLite index from `vault/**` (docs/03-data-model.md §D15).
//!
//! This is the operation that enforces the *reindex invariant* (ADR-0002): the whole index is
//! reconstructable from markdown alone. It runs inside a single transaction and returns counts so
//! callers (and the property test from docs/10-testing-strategy.md, below) can verify the rebuild.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{params, Connection};

use super::{queries, schema, IndexError, SNIPPET_MATCH_CLOSE, SNIPPET_MATCH_OPEN};
use crate::domain::links;
use crate::domain::slug as domain_slug;
use crate::vault::{store, walk};

/// Strip the [`SNIPPET_MATCH_OPEN`]/[`SNIPPET_MATCH_CLOSE`] sentinel codepoints from content
/// before it enters `search_fts`. These two Private-Use-Area codepoints are never legitimately
/// present in owner-authored markdown, but stripping here — at the one place all indexed text
/// funnels through — makes that a guarantee rather than an assumption, so `queries::search` can
/// use them as unambiguous match markers no matter what ends up on disk.
fn sanitized(content: &str) -> String {
    if content.contains(SNIPPET_MATCH_OPEN) || content.contains(SNIPPET_MATCH_CLOSE) {
        content
            .chars()
            .filter(|c| *c != SNIPPET_MATCH_OPEN && *c != SNIPPET_MATCH_CLOSE)
            .collect()
    } else {
        content.to_string()
    }
}

/// Row counts produced by a reindex, used for verification (D15).
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct ReindexCounts {
    pub ideas: usize,
    pub facts: usize,
    pub links: usize,
    /// Rows left in `fact_links` after resolution: every `[[idea#fact]]` (resolved or dangling)
    /// plus the bare in-fact links that resolved to a sibling fact.
    pub fact_links: usize,
}

/// Canonical TEXT form for timestamps in the index: RFC3339, whole seconds, `Z` suffix — the
/// same shape the frontmatter examples use (D8), so drift comparison is byte-stable.
fn ts(dt: &DateTime<Utc>) -> String {
    dt.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Cheap staleness check: does the vault differ from what the index reflects? Used for
/// startup-if-drift (D25).
///
/// Compares the per-idea tuple (slug, title, state, created, updated, tags) between disk
/// frontmatter and the `ideas`/`idea_tags` tables. This catches missing/extra/edited ideas —
/// the boot-relevant drift — and reports drift whenever the index's `user_version` differs from
/// [`schema::SCHEMA_VERSION`], so an index built before a table existed is rebuilt rather than
/// served with that table empty. It deliberately does not diff conversations or fact bodies
/// (post-write upserts keep those fresh; `POST /admin/reindex` is the manual override), and it
/// skips unparsable idea dirs the same way `reindex` does, so a malformed file never wedges boot.
pub fn check_drift(conn: &Connection, vault_dir: &Path) -> Result<bool, IndexError> {
    let stamped: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if stamped != schema::SCHEMA_VERSION {
        return Ok(true);
    }
    let mut disk: Vec<String> = Vec::new();
    for entry in walk::walk_ideas(vault_dir)? {
        let idea = match store::read_idea(vault_dir, &entry.slug) {
            Ok(idea) => idea,
            Err(e) => {
                tracing::warn!(slug = %entry.slug, error = %e, "skipping unparsable idea in drift check");
                continue;
            }
        };
        let fm = &idea.frontmatter;
        let mut tags = fm.tags.clone();
        tags.sort();
        disk.push(format!(
            "{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}",
            entry.slug,
            fm.title,
            fm.state.as_str(),
            ts(&fm.created),
            ts(&fm.updated),
            tags.join(",")
        ));
    }
    disk.sort();

    let mut stmt = conn.prepare(
        "SELECT i.slug, i.title, i.state, i.created_at, i.updated_at,
                COALESCE((SELECT GROUP_CONCAT(t.name, ',' ORDER BY t.name)
                          FROM tags t
                          JOIN idea_tags it ON it.tag_id = t.id
                          WHERE it.idea_id = i.id), '')
         FROM ideas i ORDER BY i.slug",
    )?;
    let indexed: Vec<String> = stmt
        .query_map([], |row| {
            Ok(format!(
                "{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}",
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?
        .collect::<Result<_, _>>()?;

    Ok(disk != indexed)
}

/// Rebuild the entire derived index from the vault, transactionally (the D15 sequence).
///
/// Full rebuild is the canonical path (incremental post-write upserts are an optimization layered
/// on top later); it must stay idempotent — `reindex(V) == reindex(reindex(V))` — and equal to a
/// rebuild into an empty database (ADR-0002). Unparsable idea dirs are skipped with a warning
/// (D24: parse errors surface but never take the whole rebuild down — the markdown truth is
/// intact either way); skipped ideas simply have no rows until fixed.
///
/// `[[slug]]` link sources (D23): the idea body, each memory-fact body, and each fact's
/// frontmatter `links:` list — deduplicated per source idea, first-occurrence order. An
/// `[[idea#fact]]` reference in the idea body or a fact body also counts as a link to `idea`.
/// The conversation transcript is indexed for search but deliberately not mined for backlinks
/// (chat text mentioning an idea is not a curated cross-reference).
///
/// `fact_links` rows: every `[[idea#fact]]` in the idea body (no source fact) or a fact body is
/// kept, resolved or dangling; a bare `[[x]]` / frontmatter `links:` entry inside a fact is a
/// candidate link to the sibling fact `x` and is kept only if that fact exists — otherwise it was
/// an idea link, which `backlinks` already records.
///
/// Guarded against the empty-vault wipe (ADR-0019): if the walk finds no ideas while the index
/// still holds some, this refuses with [`IndexError::RefusingEmptyRebuild`] rather than committing
/// the DELETEs. Use [`reindex_forced`] for a vault the owner genuinely emptied.
pub fn reindex(conn: &mut Connection, vault_dir: &Path) -> Result<ReindexCounts, IndexError> {
    reindex_inner(conn, vault_dir, false)
}

/// [`reindex`] without the empty-vault guard — the explicit "yes, I really did delete every idea"
/// path (`POST /admin/reindex?force=1`). Prefer [`reindex`] everywhere else.
pub fn reindex_forced(
    conn: &mut Connection,
    vault_dir: &Path,
) -> Result<ReindexCounts, IndexError> {
    reindex_inner(conn, vault_dir, true)
}

fn reindex_inner(
    conn: &mut Connection,
    vault_dir: &Path,
    force: bool,
) -> Result<ReindexCounts, IndexError> {
    // 1. Walk BEFORE opening the transaction, so an empty result can be vetoed without ever
    //    reaching the DELETEs.
    let entries = walk::walk_ideas(vault_dir)?;

    // ADR-0019: "no ideas on disk" is indistinguishable from "wrong vault_dir" at this layer —
    // `walk_ideas` maps a missing directory to an empty vault, and a boot race can bind an empty
    // ghost directory over the real one. Rebuilding from that input is a correct application of
    // the ADR-0002 invariant to the WRONG vault: every derived row is deleted and nothing
    // replaces it. Truth is markdown so nothing is destroyed, but the UI enumerates ideas from
    // the index alone, so the owner's whole vault silently disappears. Refuse instead.
    if !force && entries.is_empty() {
        let indexed: usize = conn.query_row("SELECT COUNT(*) FROM ideas", [], |row| row.get(0))?;
        if indexed > 0 {
            return Err(IndexError::RefusingEmptyRebuild {
                vault_dir: vault_dir.display().to_string(),
                indexed,
            });
        }
    }

    let tx = conn.transaction()?;
    let mut counts = ReindexCounts::default();

    let stamped: i64 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if stamped != schema::SCHEMA_VERSION {
        schema::recreate_schema(&tx)?;
    }

    // 2. Clear every derived table — full rebuild semantics.
    tx.execute_batch(
        "DELETE FROM edges;
         DELETE FROM idea_tags;
         DELETE FROM fact_links;
         DELETE FROM memory_facts;
         DELETE FROM backlinks;
         DELETE FROM search_fts;
         DELETE FROM tags;
         DELETE FROM ideas;",
    )?;

    // 3–9. Repopulate from the walk.
    for entry in entries {
        let idea = match store::read_idea(vault_dir, &entry.slug) {
            Ok(idea) => idea,
            Err(e) => {
                tracing::warn!(slug = %entry.slug, error = %e, "skipping unparsable idea during reindex");
                continue;
            }
        };
        let fm = &idea.frontmatter;
        if fm.slug != entry.slug {
            // D22: the folder name is the identity. A mismatched frontmatter slug is a malformed
            // vault edit — index under the folder name and surface the inconsistency.
            tracing::warn!(folder = %entry.slug, frontmatter = %fm.slug,
                "idea.md frontmatter slug differs from folder name; indexing under folder name");
        }

        tx.execute(
            "INSERT INTO ideas (slug, title, state, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                entry.slug,
                fm.title,
                fm.state.as_str(),
                ts(&fm.created),
                ts(&fm.updated)
            ],
        )?;
        let idea_id = tx.last_insert_rowid();
        counts.ideas += 1;

        // 6. Tags.
        for tag in &fm.tags {
            tx.execute("INSERT OR IGNORE INTO tags (name) VALUES (?1)", [tag])?;
            tx.execute(
                "INSERT OR IGNORE INTO idea_tags (idea_id, tag_id)
                 SELECT ?1, id FROM tags WHERE name = ?2",
                params![idea_id, tag],
            )?;
        }

        // 8. Search content — one `kind` row per field so queries::search can weight fields
        // independently (a title hit should outrank an equally bm25-scored body hit). Coverage
        // is now every owner-authored surface: title, tags, idea body, conversation transcript,
        // and (below, alongside the memory-facts loop) each fact's title+body — previously only
        // idea_body/conversation/artifact were indexed, leaving the title/tags/fact-body text the
        // owner actually wrote unsearchable.
        tx.execute(
            "INSERT INTO search_fts (idea_id, kind, ref, content) VALUES (?1, 'title', '', ?2)",
            params![idea_id, sanitized(&fm.title)],
        )?;
        if !fm.tags.is_empty() {
            // Space-joined so multi-word tags stay separable tokens; omitted entirely when there
            // are no tags rather than indexing an empty row.
            tx.execute(
                "INSERT INTO search_fts (idea_id, kind, ref, content) VALUES (?1, 'tags', '', ?2)",
                params![idea_id, sanitized(&fm.tags.join(" "))],
            )?;
        }
        tx.execute(
            "INSERT INTO search_fts (idea_id, kind, ref, content) VALUES (?1, 'idea_body', '', ?2)",
            params![idea_id, sanitized(&idea.body)],
        )?;
        let conversation = store::read_conversation(vault_dir, &entry.slug)?;
        if !conversation.is_empty() {
            tx.execute(
                "INSERT INTO search_fts (idea_id, kind, ref, content) VALUES (?1, 'conversation', '', ?2)",
                params![idea_id, sanitized(&conversation)],
            )?;
        }

        // 8b. Knowledge-extraction artifacts (`artifacts/*.md`, docs/adr/0015): searchable, but
        // never mined for backlinks (AI-generated text, same rationale as the conversation) and
        // no derived table — the `.html` report exports are excluded by `read_artifacts` itself.
        let artifacts = match store::read_artifacts(vault_dir, &entry.slug) {
            Ok(artifacts) => artifacts,
            Err(e) => {
                tracing::warn!(slug = %entry.slug, error = %e,
                    "skipping unparsable artifacts during reindex");
                Vec::new()
            }
        };
        for artifact in &artifacts {
            tx.execute(
                "INSERT INTO search_fts (idea_id, kind, ref, content) VALUES (?1, 'artifact', ?2, ?3)",
                params![
                    idea_id,
                    artifact.frontmatter.slug,
                    sanitized(&format!(
                        "{}\n\n{}",
                        artifact.frontmatter.title, artifact.body
                    ))
                ],
            )?;
        }

        // 7 + 9. Memory facts, `[[slug]]` link targets, and fact-link candidates.
        let mut targets: Vec<String> = links::extract_links(&idea.body);
        for fact_ref in links::extract_fact_refs(&idea.body) {
            insert_fact_link(&tx, idea_id, None, &fact_ref.idea, &fact_ref.fact, true)?;
            if fact_ref.idea != entry.slug {
                targets.push(fact_ref.idea);
            }
        }
        let facts = match store::read_memory_facts(vault_dir, &entry.slug) {
            Ok(facts) => facts,
            Err(e) => {
                tracing::warn!(slug = %entry.slug, error = %e,
                    "skipping unparsable memory facts during reindex");
                Vec::new()
            }
        };
        for fact in &facts {
            tx.execute(
                "INSERT INTO memory_facts (idea_id, slug, title, created_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    idea_id,
                    fact.frontmatter.slug,
                    fact.frontmatter.title,
                    ts(&fact.frontmatter.created)
                ],
            )?;
            let fact_id = tx.last_insert_rowid();
            counts.facts += 1;

            // Fact bodies are owner-authored durable truth (the `memory_facts` table and
            // MEMORY.md are index-only pointers, no body column) — one 'memory' search_fts row
            // per fact, title+body, so extracted facts are finally searchable like idea_body.
            tx.execute(
                "INSERT INTO search_fts (idea_id, kind, ref, content) VALUES (?1, 'memory', ?2, ?3)",
                params![
                    idea_id,
                    fact.frontmatter.slug,
                    sanitized(&format!("{}\n\n{}", fact.frontmatter.title, fact.body))
                ],
            )?;

            // (dst idea, dst fact, explicit) in first-occurrence order; a repeat of the same
            // destination keeps one row, explicit if any occurrence was.
            let mut fact_dsts: Vec<(String, String, bool)> = Vec::new();
            let mut add_dst = |dst_idea: &str, dst_fact: &str, explicit: bool| {
                if dst_idea == entry.slug && dst_fact == fact.frontmatter.slug {
                    return;
                }
                match fact_dsts
                    .iter_mut()
                    .find(|(i, f, _)| i == dst_idea && f == dst_fact)
                {
                    Some(existing) => existing.2 |= explicit,
                    None => fact_dsts.push((dst_idea.to_string(), dst_fact.to_string(), explicit)),
                }
            };
            for target in links::extract_links(&fact.body) {
                add_dst(&entry.slug, &target, false);
                targets.push(target);
            }
            for fact_ref in links::extract_fact_refs(&fact.body) {
                add_dst(&fact_ref.idea, &fact_ref.fact, true);
                if fact_ref.idea != entry.slug {
                    targets.push(fact_ref.idea);
                }
            }
            for target in &fact.frontmatter.links {
                // Frontmatter `links:` entries are author-provided strings — hold them to the
                // same canonical-slug bar as `[[slug]]` tokens.
                if domain_slug::is_valid(target) {
                    add_dst(&entry.slug, target, false);
                    targets.push(target.clone());
                }
            }
            for (dst_idea, dst_fact, explicit) in &fact_dsts {
                insert_fact_link(&tx, idea_id, Some(fact_id), dst_idea, dst_fact, *explicit)?;
            }
        }

        let mut seen: Vec<String> = Vec::new();
        for target in targets {
            if seen.contains(&target) {
                continue;
            }
            tx.execute(
                "INSERT INTO backlinks (source_idea_id, target_slug, target_idea_id)
                 VALUES (?1, ?2, NULL)",
                params![idea_id, target],
            )?;
            counts.links += 1;
            seen.push(target);
        }
    }

    // 10. Resolve targets by slug — NULL stays for forward/dangling references (D23), and a
    // later reindex re-resolves once the target idea exists.
    tx.execute(
        "UPDATE backlinks
         SET target_idea_id = (SELECT id FROM ideas WHERE slug = target_slug)",
        [],
    )?;

    // 11. Resolve fact links by (idea slug, fact slug), again leaving NULL for dangling refs.
    // Unresolved bare candidates are dropped: they named an idea, not a sibling fact.
    tx.execute_batch(
        "UPDATE fact_links
         SET dst_fact_id = (SELECT f.id FROM memory_facts f
                            JOIN ideas i ON i.id = f.idea_id
                            WHERE i.slug = fact_links.dst_idea_slug
                              AND f.slug = fact_links.dst_fact_slug
                            ORDER BY f.id LIMIT 1);
         DELETE FROM fact_links WHERE explicit = 0 AND dst_fact_id IS NULL;",
    )?;
    counts.fact_links = tx.query_row("SELECT COUNT(*) FROM fact_links", [], |row| row.get(0))?;

    // 12. Derive the typed idea-to-idea `edges` from the now-resolved links.
    derive_link_edges(&tx)?;
    derive_tag_edges(&tx)?;
    derive_lexical_edges(&tx)?;

    tx.pragma_update(None, "user_version", schema::SCHEMA_VERSION)?;
    tx.commit()?;
    Ok(counts)
}

// Derives the `type = 'link'` rows of `edges` (D6, D23) from resolved `backlinks` and resolved
// cross-idea `fact_links`, at weight 1.0. One row per unordered idea pair, stored with
// `src_idea_id < dst_idea_id`; self-pairs are excluded. `detail` names the linking direction by
// slug (`a → b` one-way, `a ↔ b` in alphabetical order when both link each other), followed by the
// sorted `idea#fact` targets of any fact-level refs, so it never depends on row or id order.
fn derive_link_edges(tx: &rusqlite::Transaction<'_>) -> Result<(), IndexError> {
    struct Pair {
        forward: bool,
        backward: bool,
        facts: BTreeSet<String>,
    }

    let mut stmt = tx.prepare(
        "SELECT s.id, s.slug, t.id, t.slug, NULL
         FROM backlinks b
         JOIN ideas s ON s.id = b.source_idea_id
         JOIN ideas t ON t.id = b.target_idea_id
         WHERE s.id <> t.id
         UNION ALL
         SELECT s.id, s.slug, t.id, t.slug, t.slug || '#' || df.slug
         FROM fact_links fl
         JOIN ideas s ON s.id = fl.src_idea_id
         JOIN memory_facts df ON df.id = fl.dst_fact_id
         JOIN ideas t ON t.id = df.idea_id
         WHERE s.id <> t.id",
    )?;
    let mut pairs: BTreeMap<(i64, i64), (String, String, Pair)> = BTreeMap::new();
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let (src, src_slug, dst, dst_slug): (i64, String, i64, String) =
            (row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?);
        let fact_ref: Option<String> = row.get(4)?;
        let (lo, hi, lo_slug, hi_slug, forward) = if src < dst {
            (src, dst, src_slug, dst_slug, true)
        } else {
            (dst, src, dst_slug, src_slug, false)
        };
        let (_, _, pair) = pairs.entry((lo, hi)).or_insert_with(|| {
            let pair = Pair {
                forward: false,
                backward: false,
                facts: BTreeSet::new(),
            };
            (lo_slug, hi_slug, pair)
        });
        if forward {
            pair.forward = true;
        } else {
            pair.backward = true;
        }
        pair.facts.extend(fact_ref);
    }
    drop(rows);
    drop(stmt);

    for ((lo, hi), (lo_slug, hi_slug, pair)) in pairs {
        let mut detail = match (pair.forward, pair.backward) {
            (true, true) if lo_slug > hi_slug => format!("{hi_slug} ↔ {lo_slug}"),
            (true, true) => format!("{lo_slug} ↔ {hi_slug}"),
            (true, false) => format!("{lo_slug} → {hi_slug}"),
            _ => format!("{hi_slug} → {lo_slug}"),
        };
        if !pair.facts.is_empty() {
            let facts: Vec<&str> = pair.facts.iter().map(String::as_str).collect();
            detail.push_str(&format!(" ({})", facts.join(", ")));
        }
        tx.execute(
            "INSERT INTO edges (src_idea_id, dst_idea_id, type, weight, detail)
             VALUES (?1, ?2, 'link', 1.0, ?3)",
            params![lo, hi, detail],
        )?;
    }
    Ok(())
}

// Derives the `type = 'tag'` rows of `edges` from exact shared tag names. A shared tag carried by
// `df` of `n` ideas weighs `0.3 * ln(n / df) / ln(n)`, so a tag every idea carries weighs nothing;
// a pair weighs the sum over its shared tags, capped at 1.0, and a zero-weight pair gets no row.
// Tags are visited in name order so the float sum, and hence the stored weight, never depends on
// row order. `detail` is the shared tag names, sorted.
fn derive_tag_edges(tx: &rusqlite::Transaction<'_>) -> Result<(), IndexError> {
    const TAG_WEIGHT: f64 = 0.3;
    const MAX_WEIGHT: f64 = 1.0;

    let idea_count: i64 = tx.query_row("SELECT COUNT(*) FROM ideas", [], |row| row.get(0))?;
    if idea_count < 2 {
        return Ok(());
    }
    let n = idea_count as f64;

    let mut carriers: BTreeMap<String, Vec<i64>> = BTreeMap::new();
    let mut stmt = tx.prepare(
        "SELECT t.name, it.idea_id FROM idea_tags it JOIN tags t ON t.id = it.tag_id
         ORDER BY t.name, it.idea_id",
    )?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        carriers.entry(row.get(0)?).or_default().push(row.get(1)?);
    }
    drop(rows);
    drop(stmt);

    let mut pairs: BTreeMap<(i64, i64), (f64, Vec<&str>)> = BTreeMap::new();
    for (name, ideas) in &carriers {
        let weight = TAG_WEIGHT * (n / ideas.len() as f64).ln() / n.ln();
        if ideas.len() < 2 || weight <= 0.0 {
            continue;
        }
        for (i, lo) in ideas.iter().enumerate() {
            for hi in &ideas[i + 1..] {
                let (sum, names) = pairs.entry((*lo, *hi)).or_default();
                *sum += weight;
                names.push(name);
            }
        }
    }

    for ((lo, hi), (sum, names)) in pairs {
        tx.execute(
            "INSERT INTO edges (src_idea_id, dst_idea_id, type, weight, detail)
             VALUES (?1, ?2, 'tag', ?3, ?4)",
            params![lo, hi, sum.min(MAX_WEIGHT), names.join(", ")],
        )?;
    }
    Ok(())
}

const LEXICAL_EDGE_TOP_K: usize = 2;
const LEXICAL_EDGE_MIN_SHARED: usize = 3;
const LEXICAL_EDGE_WEIGHT: f64 = 0.19;
const LEXICAL_EDGE_MAX_TERMS: usize = 5;

// Derives the `type = 'lexical'` rows of `edges` from the fair lexical baseline, with no model
// call (ADR-0012). Each idea, in slug order, proposes its top `LEXICAL_EDGE_TOP_K` hits of
// `queries::lexical_baseline`; a hit is kept only if at least `LEXICAL_EDGE_MIN_SHARED` distinct
// query terms of the proposer occur in the hit's title, tags, idea body or memory rows, not
// counting a term every idea contains (IDF 0). For a pair already joined by a `link` edge the
// words of both slugs do not count either, so a `[[slug]]` link or a mention of the other idea
// is not counted twice and neither slug reaches `detail`; unlinked pairs keep their title words.
// A kept hit weighs `LEXICAL_EDGE_WEIGHT * score / top score`, so in (0, LEXICAL_EDGE_WEIGHT],
// below the tag weight, and a two-hop lexical-only path scores half of that, under the
// related-ideas floor. One row per unordered pair: the larger of the two directional weights,
// and `detail` the union of both directions' shared terms, sorted, at most
// `LEXICAL_EDGE_MAX_TERMS`. Everything is keyed by BTreeMap, so no weight or detail depends on
// row, id or hash order. The corpus is loaded once, inside the reindex transaction, after every
// `search_fts` row is written. Its bm25 weights and vocabulary cover only the eligible rows, so
// neither depends on conversation or artifact volume; the copy's `INSERT … SELECT` still scans
// every `search_fts` row.
fn derive_lexical_edges(tx: &rusqlite::Transaction<'_>) -> Result<(), IndexError> {
    let mut ids: BTreeMap<String, i64> = BTreeMap::new();
    let mut stmt = tx.prepare("SELECT slug, id FROM ideas")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        ids.insert(row.get(0)?, row.get(1)?);
    }
    drop(rows);
    drop(stmt);
    if ids.len() < 2 {
        return Ok(());
    }
    let mut linked: BTreeSet<(i64, i64)> = BTreeSet::new();
    let mut stmt = tx.prepare("SELECT src_idea_id, dst_idea_id FROM edges WHERE type = 'link'")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        linked.insert((row.get(0)?, row.get(1)?));
    }
    drop(rows);
    drop(stmt);
    let corpus = queries::LexicalCorpus::load(tx)?;

    let mut pairs: BTreeMap<(i64, i64), (f64, BTreeSet<String>)> = BTreeMap::new();
    for (slug, &id) in &ids {
        let terms = corpus.query_terms(id);
        let hits = corpus.hits(slug, &terms, LEXICAL_EDGE_TOP_K)?;
        let Some(top) = hits.first().map(|hit| hit.score) else {
            continue;
        };
        if top <= 0.0 {
            continue;
        }
        for hit in &hits {
            let Some(&other) = ids.get(&hit.idea_slug) else {
                continue;
            };
            let key = (id.min(other), id.max(other));
            let slug_words: BTreeSet<String> = if linked.contains(&key) {
                [slug.as_str(), hit.idea_slug.as_str()]
                    .into_iter()
                    .flat_map(|s| s.split(|c: char| !c.is_alphanumeric()))
                    .filter(|word| !word.is_empty())
                    .map(str::to_lowercase)
                    .collect()
            } else {
                BTreeSet::new()
            };
            let shared: BTreeSet<&String> = terms
                .iter()
                .filter(|term| corpus.contains(other, term))
                .filter(|term| !corpus.in_every_idea(term))
                .filter(|term| !slug_words.contains(*term))
                .collect();
            if shared.len() < LEXICAL_EDGE_MIN_SHARED {
                continue;
            }
            let weight = LEXICAL_EDGE_WEIGHT * hit.score / top;
            let (max, detail) = pairs.entry(key).or_insert((0.0, BTreeSet::new()));
            *max = max.max(weight);
            detail.extend(shared.into_iter().cloned());
        }
    }

    for ((lo, hi), (weight, terms)) in pairs {
        let detail: Vec<&str> = terms
            .iter()
            .take(LEXICAL_EDGE_MAX_TERMS)
            .map(String::as_str)
            .collect();
        tx.execute(
            "INSERT INTO edges (src_idea_id, dst_idea_id, type, weight, detail)
             VALUES (?1, ?2, 'lexical', ?3, ?4)",
            params![lo, hi, weight, detail.join(", ")],
        )?;
    }
    Ok(())
}

// Inserts one unresolved `fact_links` row; step 11 of `reindex` resolves or drops it.
fn insert_fact_link(
    tx: &rusqlite::Transaction<'_>,
    src_idea_id: i64,
    src_fact_id: Option<i64>,
    dst_idea: &str,
    dst_fact: &str,
    explicit: bool,
) -> Result<(), IndexError> {
    tx.execute(
        "INSERT INTO fact_links
             (src_idea_id, src_fact_id, dst_idea_slug, dst_fact_slug, dst_fact_id, explicit)
         VALUES (?1, ?2, ?3, ?4, NULL, ?5)",
        params![src_idea_id, src_fact_id, dst_idea, dst_fact, explicit],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::domain::{Idea, IdeaFrontmatter, IdeaState, MemoryFact, MemoryFactFrontmatter};
    use crate::index::queries::{self, FactLink};
    use crate::index::schema;

    fn dt(h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 7, h, 0, 0).unwrap()
    }

    fn idea(slug: &str, title: &str, state: IdeaState, tags: &[&str], body: &str) -> Idea {
        Idea {
            frontmatter: IdeaFrontmatter {
                title: title.into(),
                slug: slug.into(),
                state,
                tags: tags.iter().map(|t| t.to_string()).collect(),
                sources: vec![],
                created: dt(10),
                updated: dt(11),
                extra: Default::default(),
            },
            body: body.into(),
        }
    }

    fn fact(slug: &str, title: &str, links: &[&str], body: &str) -> MemoryFact {
        MemoryFact {
            frontmatter: MemoryFactFrontmatter {
                slug: slug.into(),
                title: title.into(),
                tags: vec![],
                created: dt(12),
                links: links.iter().map(|l| l.to_string()).collect(),
            },
            body: body.into(),
        }
    }

    fn artifact(slug: &str, title: &str, body: &str) -> crate::domain::Artifact {
        crate::domain::Artifact {
            frontmatter: crate::domain::ArtifactFrontmatter {
                slug: slug.into(),
                title: title.into(),
                kind: crate::domain::ArtifactKind::Finding,
                lens: Some("extract-key-decisions".into()),
                created: dt(13),
                model: "test".into(),
                revises: None,
                version: None,
                answered: Vec::new(),
                recipe: None,
            },
            body: body.into(),
        }
    }

    /// Fixture per docs/10-testing-strategy.md: mixed states, tags, facts, `[[slug]]` links
    /// including dangling and forward references, fact links (a resolving cross-idea
    /// `[[beta#durable-one]]` in alpha's body, a dangling `[[beta#no-such-fact]]` in an alpha fact,
    /// and a bare same-idea `[[durable-one]]` in a beta fact), plus a conversation transcript and
    /// a knowledge-extraction artifact (docs/adr/0015). A third idea, delta, keeps every shared tag
    /// below full coverage, so the IDF-weighted tag edges (alpha–beta on `risk`, alpha–delta on
    /// `markets`) are non-zero.
    fn build_fixture_vault(vault: &Path) {
        store::write_idea(
            vault,
            &idea(
                "alpha",
                "Alpha",
                IdeaState::InDiscussion,
                &["markets", "risk"],
                "Alpha builds on [[beta]] but also on [[ghost-idea]] (not created yet).\n\
                 Its core rests on [[beta#durable-one]].\n",
            ),
        )
        .unwrap();
        store::write_memory_fact(
            vault,
            "alpha",
            &fact(
                "alpha-note",
                "Alpha note",
                &[],
                "Parked question waiting on [[beta#no-such-fact]].\n",
            ),
        )
        .unwrap();
        store::append_conversation(vault, "alpha", "## user\nrun it into the ground\n").unwrap();

        store::write_idea(
            vault,
            &idea(
                "beta",
                "Beta",
                IdeaState::Stored,
                &["risk"],
                "Beta statement mentions [[alpha]].\n",
            ),
        )
        .unwrap();
        store::write_idea(
            vault,
            &idea(
                "delta",
                "Delta",
                IdeaState::Draft,
                &["markets"],
                "Delta is a standalone statement.\n",
            ),
        )
        .unwrap();

        store::write_memory_fact(
            vault,
            "beta",
            &fact(
                "durable-one",
                "Durable one",
                &["alpha", "Not A Slug"],
                "Conclusion referencing [[alpha]] again and [[gamma]].\n",
            ),
        )
        .unwrap();
        store::write_memory_fact(
            vault,
            "beta",
            &fact(
                "durable-two",
                "Durable two",
                &["durable-one"],
                "Refines [[durable-one]].\n",
            ),
        )
        .unwrap();

        // One knowledge-extraction artifact (searchable truth) and its derived .html export
        // (never indexed). The artifact body mentions [[beta]] — deliberately NOT a backlink.
        store::write_artifact(
            vault,
            "alpha",
            &artifact(
                "20260708-193045-key-decisions",
                "Key decisions",
                "- keep the flywheel; see [[beta]]\n",
            ),
        )
        .unwrap();
        store::write_artifact_html(
            vault,
            "alpha",
            "20260708-193045-report",
            "<!DOCTYPE html><p>UNINDEXED-REPORT</p>",
        )
        .unwrap();
    }

    /// Normalized, id-free snapshot of every derived table. Row ids are allocation order and may
    /// differ between rebuilds — equality must be judged on natural keys only.
    fn snapshot(conn: &Connection) -> Vec<String> {
        let mut out = Vec::new();
        let mut push_query = |sql: &str| {
            let mut stmt = conn.prepare(sql).unwrap();
            let mut rows = stmt.query([]).unwrap();
            while let Some(row) = rows.next().unwrap() {
                let mut line = String::new();
                for i in 0..row.as_ref().column_count() {
                    let v: Option<String> = row.get(i).unwrap();
                    line.push_str(v.as_deref().unwrap_or("<NULL>"));
                    line.push('\u{1f}');
                }
                out.push(line);
            }
        };
        push_query(
            "SELECT 'idea', slug, title, state, created_at, updated_at FROM ideas ORDER BY slug",
        );
        push_query(
            "SELECT 'tag', i.slug, t.name FROM idea_tags it
             JOIN ideas i ON i.id = it.idea_id JOIN tags t ON t.id = it.tag_id
             ORDER BY i.slug, t.name",
        );
        push_query(
            "SELECT 'fact', i.slug, f.slug, f.title, f.created_at FROM memory_facts f
             JOIN ideas i ON i.id = f.idea_id ORDER BY i.slug, f.slug",
        );
        push_query(
            "SELECT 'backlink', s.slug, b.target_slug, t.slug FROM backlinks b
             JOIN ideas s ON s.id = b.source_idea_id
             LEFT JOIN ideas t ON t.id = b.target_idea_id
             ORDER BY s.slug, b.target_slug",
        );
        push_query(
            "SELECT 'fact_link', s.slug, sf.slug, fl.dst_idea_slug, fl.dst_fact_slug,
                    di.slug || '#' || df.slug, CAST(fl.explicit AS TEXT)
             FROM fact_links fl
             JOIN ideas s ON s.id = fl.src_idea_id
             LEFT JOIN memory_facts sf ON sf.id = fl.src_fact_id
             LEFT JOIN memory_facts df ON df.id = fl.dst_fact_id
             LEFT JOIN ideas di ON di.id = df.idea_id
             ORDER BY s.slug, sf.slug, fl.dst_idea_slug, fl.dst_fact_slug, fl.explicit",
        );
        push_query(
            "SELECT 'edge', s.slug, d.slug, e.type, CAST(e.weight AS TEXT), e.detail FROM edges e
             JOIN ideas s ON s.id = e.src_idea_id JOIN ideas d ON d.id = e.dst_idea_id
             ORDER BY s.slug, d.slug, e.type",
        );
        push_query(
            "SELECT 'fts', i.slug, s.kind, s.ref, s.content FROM search_fts s
             JOIN ideas i ON i.id = s.idea_id ORDER BY i.slug, s.kind, s.ref",
        );
        out
    }

    fn mem_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply_schema(&conn).unwrap();
        conn
    }

    #[test]
    fn keystone_reindex_is_idempotent_and_rebuildable_from_disk_alone() {
        let tmp = tempfile::tempdir().unwrap();
        build_fixture_vault(tmp.path());

        // reindex(V) …
        let mut conn = mem_conn();
        let counts1 = reindex(&mut conn, tmp.path()).unwrap();
        let snap1 = snapshot(&conn);

        // … == reindex(reindex(V)) (idempotent, same connection)
        let counts2 = reindex(&mut conn, tmp.path()).unwrap();
        assert_eq!(counts1, counts2);
        assert_eq!(snap1, snapshot(&conn));

        // drop(index); reindex(V) == index(V) (rebuildable from the vault alone)
        let mut fresh = mem_conn();
        reindex(&mut fresh, tmp.path()).unwrap();
        assert_eq!(snap1, snapshot(&fresh));
    }

    #[test]
    fn reindex_idempotent_with_new_artifact_kinds() {
        use crate::domain::ArtifactKind;
        let tmp = tempfile::tempdir().unwrap();
        build_fixture_vault(tmp.path());
        // A workflow run's stage artifacts (docs/adr/0034) are ordinary artifacts: searchable,
        // rebuilt from disk like every other kind, never a derived table of their own.
        for (slug, kind, body) in [
            (
                "20260930-101500-design-panel-1-ground",
                ArtifactKind::GroundMap,
                "grounded groundmapneedle",
            ),
            (
                "20260930-101500-design-panel-2-panel",
                ArtifactKind::Scorecard,
                "scored scorecardneedle",
            ),
            (
                "20260930-101500-design-panel-run",
                ArtifactKind::WorkflowRun,
                "stages runrecordneedle",
            ),
        ] {
            let mut a = artifact(slug, "Stage", body);
            a.frontmatter.kind = kind;
            a.frontmatter.lens = None;
            store::write_artifact(tmp.path(), "alpha", &a).unwrap();
        }

        let mut conn = mem_conn();
        let counts1 = reindex(&mut conn, tmp.path()).unwrap();
        let snap1 = snapshot(&conn);
        let counts2 = reindex(&mut conn, tmp.path()).unwrap();
        assert_eq!(counts1, counts2);
        assert_eq!(snap1, snapshot(&conn));
        let mut fresh = mem_conn();
        reindex(&mut fresh, tmp.path()).unwrap();
        assert_eq!(snap1, snapshot(&fresh));

        for needle in ["groundmapneedle", "scorecardneedle", "runrecordneedle"] {
            let refs: Vec<String> = conn
                .prepare(
                    "SELECT ref FROM search_fts WHERE kind = 'artifact' AND search_fts MATCH ?1",
                )
                .unwrap()
                .query_map([needle], |r| r.get(0))
                .unwrap()
                .map(Result::unwrap)
                .collect();
            assert_eq!(refs.len(), 1, "{needle}: {refs:?}");
        }
    }

    fn write_linked_ideas(vault: &Path, ideas: &[(&str, &str)]) {
        for (slug, body) in ideas {
            store::write_idea(vault, &idea(slug, slug, IdeaState::InDiscussion, &[], body))
                .unwrap();
        }
    }

    type EdgeRow = (String, String, String, f64, String);

    fn edge_rows(conn: &Connection) -> Vec<EdgeRow> {
        let mut stmt = conn
            .prepare(
                "SELECT s.slug, d.slug, e.type, e.weight, e.detail,
                        e.src_idea_id < e.dst_idea_id
                 FROM edges e
                 JOIN ideas s ON s.id = e.src_idea_id JOIN ideas d ON d.id = e.dst_idea_id",
            )
            .unwrap();
        let mut rows: Vec<EdgeRow> = stmt
            .query_map([], |r| {
                assert!(r.get::<_, bool>(5)?, "edge is not stored src < dst");
                let (a, b): (String, String) = (r.get(0)?, r.get(1)?);
                let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
                Ok((lo, hi, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect();
        rows.sort_by(|x, y| (&x.0, &x.1, &x.2).cmp(&(&y.0, &y.1, &y.2)));
        rows
    }

    fn edge(a: &str, b: &str, detail: &str) -> EdgeRow {
        (a.into(), b.into(), "link".into(), 1.0, detail.into())
    }

    #[test]
    fn edges_link_edges_are_undirected_canonical_and_exclude_self() {
        let tmp = tempfile::tempdir().unwrap();
        write_linked_ideas(
            tmp.path(),
            &[
                ("alpha", "Builds on [[beta]] and, oddly, on [[alpha]]."),
                ("beta", "Mentions [[alpha]] back."),
                ("gamma", "Points at [[alpha]] only."),
                ("delta", "Points at [[ghost-idea]], which does not exist."),
            ],
        );
        let mut conn = mem_conn();
        reindex(&mut conn, tmp.path()).unwrap();

        assert_eq!(
            edge_rows(&conn),
            vec![
                edge("alpha", "beta", "alpha ↔ beta"),
                edge("alpha", "gamma", "gamma → alpha"),
            ]
        );
    }

    #[test]
    fn edges_fact_ref_yields_a_link_edge() {
        let tmp = tempfile::tempdir().unwrap();
        write_linked_ideas(
            tmp.path(),
            &[
                (
                    "alpha",
                    "Rests on [[beta#durable-one]] and [[ghost#nothing]].",
                ),
                ("beta", "Standalone statement."),
            ],
        );
        store::write_memory_fact(
            tmp.path(),
            "beta",
            &fact("durable-one", "Durable one", &[], "A conclusion."),
        )
        .unwrap();
        let mut conn = mem_conn();
        reindex(&mut conn, tmp.path()).unwrap();

        assert_eq!(
            edge_rows(&conn),
            vec![edge("alpha", "beta", "alpha → beta (beta#durable-one)")]
        );
    }

    #[test]
    fn edges_rebuild_from_disk_alone_is_identical() {
        let tmp = tempfile::tempdir().unwrap();
        build_fixture_vault(tmp.path());
        let mut conn = mem_conn();
        reindex(&mut conn, tmp.path()).unwrap();
        let before = edge_rows(&conn);
        assert!(before.iter().any(|edge| edge.2 == "link"));
        assert!(before.iter().any(|edge| edge.2 == "tag"));

        let mut fresh = mem_conn();
        reindex(&mut fresh, tmp.path()).unwrap();
        assert_eq!(before, edge_rows(&fresh));

        reindex(&mut conn, tmp.path()).unwrap();
        assert_eq!(before, edge_rows(&conn));
    }

    fn write_tagged_ideas(vault: &Path, ideas: &[(&str, &[&str], &str)]) {
        for (slug, tags, body) in ideas {
            store::write_idea(
                vault,
                &idea(slug, slug, IdeaState::InDiscussion, tags, body),
            )
            .unwrap();
        }
    }

    fn tag_edge_rows(conn: &Connection) -> Vec<(String, String, f64, String)> {
        edge_rows(conn)
            .into_iter()
            .filter(|edge| edge.2 == "tag")
            .map(|(a, b, _, weight, detail)| (a, b, (weight * 1000.0).round() / 1000.0, detail))
            .collect()
    }

    fn tag_edge(a: &str, b: &str, weight: f64, detail: &str) -> (String, String, f64, String) {
        (a.into(), b.into(), weight, detail.into())
    }

    #[test]
    fn shared_tag_edge_weights_are_idf_scaled_and_capped() {
        let tmp = tempfile::tempdir().unwrap();
        write_tagged_ideas(
            tmp.path(),
            &[
                ("a", &["t1", "t2", "t3", "t4", "t5", "trio"], "Statement."),
                ("b", &["t1", "t2", "t3", "t4", "t5"], "Statement."),
                ("c", &["pair", "trio"], "Statement."),
                ("d", &["pair"], "Statement."),
                ("e", &["trio"], "Statement."),
                ("f", &[], "Statement."),
                ("g", &[], "Statement."),
                ("h", &[], "Statement."),
                ("i", &[], "Statement."),
                ("j", &[], "Statement."),
            ],
        );
        let mut conn = mem_conn();
        reindex(&mut conn, tmp.path()).unwrap();

        assert_eq!(
            tag_edge_rows(&conn),
            vec![
                tag_edge("a", "b", 1.0, "t1, t2, t3, t4, t5"),
                tag_edge("a", "c", 0.157, "trio"),
                tag_edge("a", "e", 0.157, "trio"),
                tag_edge("c", "d", 0.21, "pair"),
                tag_edge("c", "e", 0.157, "trio"),
            ]
        );
    }

    #[test]
    fn shared_tag_edge_ignores_a_tag_every_idea_carries() {
        let tmp = tempfile::tempdir().unwrap();
        write_tagged_ideas(
            tmp.path(),
            &[
                ("a", &["everything", "niche"], "Statement."),
                ("b", &["everything", "niche"], "Statement."),
                ("c", &["everything"], "Statement."),
            ],
        );
        let mut conn = mem_conn();
        reindex(&mut conn, tmp.path()).unwrap();
        assert_eq!(
            tag_edge_rows(&conn),
            vec![tag_edge("a", "b", 0.111, "niche")]
        );

        let only_universal = tempfile::tempdir().unwrap();
        write_tagged_ideas(
            only_universal.path(),
            &[
                ("a", &["everything"], "Statement."),
                ("b", &["everything"], "Statement."),
            ],
        );
        let mut conn = mem_conn();
        reindex(&mut conn, only_universal.path()).unwrap();
        assert!(tag_edge_rows(&conn).is_empty());
    }

    #[test]
    fn shared_tag_edge_is_exact_match_only() {
        let tmp = tempfile::tempdir().unwrap();
        write_tagged_ideas(
            tmp.path(),
            &[
                ("a", &["system-design"], "Statement."),
                ("b", &["systems-design"], "Statement."),
                ("c", &["unrelated"], "Statement."),
            ],
        );
        let mut conn = mem_conn();
        reindex(&mut conn, tmp.path()).unwrap();
        assert!(tag_edge_rows(&conn).is_empty());
    }

    #[test]
    fn tag_near_duplicates_reports_system_design_drift() {
        let tmp = tempfile::tempdir().unwrap();
        write_tagged_ideas(
            tmp.path(),
            &[
                ("a", &["system-design", "rust"], "Statement."),
                ("b", &["systems-design"], "Statement."),
                ("c", &["system-design", "trust"], "Statement."),
            ],
        );
        let mut conn = mem_conn();
        reindex(&mut conn, tmp.path()).unwrap();

        assert_eq!(
            queries::tag_near_duplicates(&conn).unwrap(),
            vec![queries::TagNearDuplicate {
                a: "system-design".into(),
                b: "systems-design".into(),
                a_ideas: vec!["a".into(), "c".into()],
                b_ideas: vec!["b".into()],
            }]
        );
    }

    #[test]
    fn own_tag_near_duplicates_equal_the_filtered_full_report() {
        let tmp = tempfile::tempdir().unwrap();
        write_tagged_ideas(
            tmp.path(),
            &[
                ("a", &["system-design", "rust"], "Statement."),
                ("b", &["systems-design"], "Statement."),
                ("c", &["strategy"], "Statement."),
                ("d", &["strategies"], "Statement."),
            ],
        );
        let mut conn = mem_conn();
        reindex(&mut conn, tmp.path()).unwrap();

        let own: Vec<String> = vec!["system-design".into(), "rust".into()];
        let expected: Vec<queries::TagNearDuplicate> = queries::tag_near_duplicates(&conn)
            .unwrap()
            .into_iter()
            .filter(|p| own.contains(&p.a) || own.contains(&p.b))
            .collect();
        let carriers = queries::tag_carriers(&conn).unwrap();
        let actual = queries::own_tag_near_duplicates(&own, &carriers);

        assert_eq!(actual, expected);
        assert_eq!(actual.len(), 1);
        assert_eq!(
            (actual[0].a.as_str(), actual[0].b.as_str()),
            ("system-design", "systems-design")
        );
        assert!(actual
            .iter()
            .all(|p| p.a != "strategy" && p.b != "strategy"));
        assert!(queries::tag_near_duplicates(&conn)
            .unwrap()
            .iter()
            .any(|p| p.a == "strategies" && p.b == "strategy"));
    }

    #[test]
    fn own_tag_near_duplicates_lists_a_pair_of_two_own_tags_once() {
        let tmp = tempfile::tempdir().unwrap();
        write_tagged_ideas(
            tmp.path(),
            &[("a", &["system-design", "systems-design"], "Statement.")],
        );
        let mut conn = mem_conn();
        reindex(&mut conn, tmp.path()).unwrap();

        let own: Vec<String> = vec!["system-design".into(), "systems-design".into()];
        let carriers = queries::tag_carriers(&conn).unwrap();
        assert_eq!(
            queries::own_tag_near_duplicates(&own, &carriers),
            queries::tag_near_duplicates(&conn).unwrap()
        );
    }

    #[test]
    fn shared_tag_edge_and_link_sum_in_related_ideas() {
        let tmp = tempfile::tempdir().unwrap();
        write_tagged_ideas(
            tmp.path(),
            &[
                ("a", &["shared"], "Builds on [[b]]."),
                ("b", &["shared"], "Statement."),
                ("c", &[], "Statement."),
            ],
        );
        let mut conn = mem_conn();
        reindex(&mut conn, tmp.path()).unwrap();

        let related = queries::related_ideas(&conn, "a", 10).unwrap();
        assert_eq!(related.len(), 1);
        assert_eq!(related[0].slug, "b");
        assert_eq!(related[0].hops, 1);
        assert_eq!((related[0].score * 1000.0).round() / 1000.0, 1.111);
        assert_eq!(
            related[0].reasons,
            vec!["link: a → b; tag: shared".to_string()]
        );
    }

    #[test]
    fn related_excludes_own_slug() {
        let tmp = tempfile::tempdir().unwrap();
        write_linked_ideas(
            tmp.path(),
            &[
                ("a", "Links [[b]] and itself [[a]]."),
                ("b", "Links [[a]]."),
            ],
        );
        let mut conn = mem_conn();
        reindex(&mut conn, tmp.path()).unwrap();

        let of_a = queries::related_ideas(&conn, "a", 10).unwrap();
        assert_eq!(
            of_a.iter().map(|r| r.slug.as_str()).collect::<Vec<_>>(),
            ["b"]
        );
        assert_eq!(of_a[0].hops, 1);
        let of_b = queries::related_ideas(&conn, "b", 10).unwrap();
        assert_eq!(
            of_b.iter().map(|r| r.slug.as_str()).collect::<Vec<_>>(),
            ["a"]
        );
    }

    #[test]
    fn related_ideas_two_hop_ranks_below_direct() {
        let tmp = tempfile::tempdir().unwrap();
        write_linked_ideas(
            tmp.path(),
            &[
                ("a", "Links [[b]] and [[d]]."),
                ("b", "Links [[c]]."),
                ("c", "Leaf."),
                ("d", "Leaf."),
            ],
        );
        let mut conn = mem_conn();
        reindex(&mut conn, tmp.path()).unwrap();

        let related = queries::related_ideas(&conn, "a", 10).unwrap();
        assert_eq!(
            related,
            vec![
                queries::RelatedIdea {
                    slug: "b".into(),
                    title: "b".into(),
                    score: 1.0,
                    hops: 1,
                    reasons: vec!["link: a → b".into()],
                },
                queries::RelatedIdea {
                    slug: "d".into(),
                    title: "d".into(),
                    score: 1.0,
                    hops: 1,
                    reasons: vec!["link: a → d".into()],
                },
                queries::RelatedIdea {
                    slug: "c".into(),
                    title: "c".into(),
                    score: 0.5,
                    hops: 2,
                    reasons: vec!["via b (link: b → c)".into()],
                },
            ]
        );
        assert_eq!(queries::related_ideas(&conn, "a", 2).unwrap().len(), 2);
    }

    #[test]
    fn related_ideas_keeps_minimal_hop_count() {
        let tmp = tempfile::tempdir().unwrap();
        write_linked_ideas(
            tmp.path(),
            &[
                ("a", "Links [[b]] and [[c]]."),
                ("b", "Links [[c]]."),
                ("c", "Leaf."),
            ],
        );
        let mut conn = mem_conn();
        reindex(&mut conn, tmp.path()).unwrap();

        let related = queries::related_ideas(&conn, "a", 10).unwrap();
        assert_eq!(
            related
                .iter()
                .map(|r| (r.slug.as_str(), r.hops, r.score))
                .collect::<Vec<_>>(),
            [("b", 1, 1.0), ("c", 1, 1.0)]
        );
    }

    #[test]
    fn related_ideas_unknown_slug_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        write_linked_ideas(tmp.path(), &[("a", "Links [[b]]."), ("b", "Leaf.")]);
        let mut conn = mem_conn();
        reindex(&mut conn, tmp.path()).unwrap();

        assert!(queries::related_ideas(&conn, "no-such-idea", 10)
            .unwrap()
            .is_empty());
    }

    fn graph_conn(edges: &[(&str, &str, &str, f64)]) -> Connection {
        let conn = mem_conn();
        for slug in ["a", "b", "c", "d"] {
            conn.execute(
                "INSERT INTO ideas (slug, title, state, created_at, updated_at)
                 VALUES (?1, ?1, 'draft', '', '')",
                [slug],
            )
            .unwrap();
        }
        for (src, dst, kind, weight) in edges {
            conn.execute(
                "INSERT INTO edges (src_idea_id, dst_idea_id, type, weight, detail)
                 SELECT s.id, d.id, ?3, ?4, ?1 || '-' || ?2
                 FROM ideas s, ideas d WHERE s.slug = ?1 AND d.slug = ?2",
                rusqlite::params![src, dst, kind, weight],
            )
            .unwrap();
        }
        conn
    }

    fn scored(conn: &Connection, slug: &str, limit: usize) -> Vec<(String, u32, f64)> {
        queries::related_ideas(conn, slug, limit)
            .unwrap()
            .into_iter()
            .map(|r| (r.slug, r.hops, (r.score * 1000.0).round() / 1000.0))
            .collect()
    }

    #[test]
    fn related_ideas_scores_a_pair_by_the_sum_of_its_edge_types_at_both_hops() {
        let conn = graph_conn(&[
            ("a", "b", "link", 1.0),
            ("a", "b", "tag", 0.3),
            ("b", "c", "link", 1.0),
            ("b", "c", "tag", 0.3),
            ("a", "d", "tag", 0.2),
        ]);

        assert_eq!(
            scored(&conn, "a", 10),
            vec![
                ("b".to_string(), 1, 1.3),
                ("c".to_string(), 2, 0.65),
                ("d".to_string(), 1, 0.2),
            ]
        );
        let b = &queries::related_ideas(&conn, "a", 10).unwrap()[0];
        assert_eq!(b.reasons, vec!["link: a-b; tag: a-b".to_string()]);
        assert!(scored(&conn, "a", 0).is_empty());
    }

    #[test]
    fn edges_schema_rejects_non_canonical_rows() {
        let conn = graph_conn(&[]);
        let insert = |src: &str, dst: &str| {
            conn.execute(
                "INSERT INTO edges (src_idea_id, dst_idea_id, type, weight, detail)
                 SELECT s.id, d.id, 'tag', 1.0, 'x' FROM ideas s, ideas d
                 WHERE s.slug = ?1 AND d.slug = ?2",
                [src, dst],
            )
        };
        assert!(insert("a", "a").is_err(), "self-pair");
        assert!(insert("b", "a").is_err(), "reversed pair");
        assert!(insert("a", "b").is_ok());
    }

    /// The ADR-0019 regression test: the 2026-07 ghost-mount incident in miniature. An empty vault
    /// (whether truly emptied or merely unmounted) must not be allowed to delete a populated index.
    /// Asserts the index is byte-identical afterwards — the DELETEs must never have committed,
    /// which is the actual data property; the error type alone would not prove it.
    #[test]
    fn reindex_refuses_to_wipe_a_populated_index_from_an_empty_vault() {
        let tmp = tempfile::tempdir().unwrap();
        build_fixture_vault(tmp.path());
        let mut conn = mem_conn();
        let counts = reindex(&mut conn, tmp.path()).unwrap();
        assert!(counts.ideas > 0, "fixture should seed ideas");
        let before = snapshot(&conn);

        // The vault "disappears" — exactly what a ghost bind mount looks like from in here.
        for entry in std::fs::read_dir(tmp.path()).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                std::fs::remove_dir_all(&path).unwrap();
            }
        }

        match reindex(&mut conn, tmp.path()) {
            Err(IndexError::RefusingEmptyRebuild { indexed, .. }) => {
                assert_eq!(indexed, counts.ideas)
            }
            other => panic!("expected RefusingEmptyRebuild, got {other:?}"),
        }
        assert_eq!(before, snapshot(&conn), "the index must be untouched");
        let stamped: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            stamped,
            schema::SCHEMA_VERSION,
            "the refusal keeps the stamp"
        );
    }

    /// The escape hatch, and proof ADR-0002's unconditional rebuild identity is still available:
    /// a vault the owner genuinely emptied does rebuild to zero when asked explicitly.
    #[test]
    fn reindex_forced_wipes_an_empty_vault() {
        let tmp = tempfile::tempdir().unwrap();
        build_fixture_vault(tmp.path());
        let mut conn = mem_conn();
        reindex(&mut conn, tmp.path()).unwrap();

        for entry in std::fs::read_dir(tmp.path()).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                std::fs::remove_dir_all(&path).unwrap();
            }
        }

        let counts = reindex_forced(&mut conn, tmp.path()).unwrap();
        assert_eq!(counts.ideas, 0);
        assert_eq!(
            snapshot(&conn),
            snapshot(&mem_conn()),
            "index rebuilt empty"
        );
    }

    /// The guard must not over-trigger: an empty vault with an empty index is a legitimate first
    /// run, not a fault. Nothing is at risk, so nothing is refused.
    #[test]
    fn reindex_of_an_empty_vault_into_an_empty_index_is_allowed() {
        let tmp = tempfile::tempdir().unwrap();
        let mut conn = mem_conn();
        let counts = reindex(&mut conn, tmp.path()).expect("empty vault + empty index is fine");
        assert_eq!(counts.ideas, 0);
    }

    #[test]
    fn counts_and_backlink_resolution_match_the_fixture() {
        let tmp = tempfile::tempdir().unwrap();
        build_fixture_vault(tmp.path());
        let mut conn = mem_conn();

        let counts = reindex(&mut conn, tmp.path()).unwrap();
        // alpha: [[beta]] (body, plus both [[beta#..]] refs, deduped), [[ghost-idea]] — beta:
        // [[alpha]] (body + fact, deduped) + [[gamma]] (fact body) + the bare fact slug
        // [[durable-one]] (dangling at idea level); the frontmatter "Not A Slug" is rejected.
        // Fact links: alpha body -> beta#durable-one, alpha-note -> beta#no-such-fact,
        // durable-two -> durable-one.
        assert_eq!(
            counts,
            ReindexCounts {
                ideas: 3,
                facts: 3,
                links: 5,
                fact_links: 3,
            }
        );

        // Resolution: existing targets get target_idea_id, dangling/forward stay NULL.
        let resolved: Vec<(String, String, Option<String>)> = {
            let mut stmt = conn
                .prepare(
                    "SELECT s.slug, b.target_slug, t.slug FROM backlinks b
                     JOIN ideas s ON s.id = b.source_idea_id
                     LEFT JOIN ideas t ON t.id = b.target_idea_id
                     ORDER BY s.slug, b.target_slug",
                )
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        assert_eq!(
            resolved,
            vec![
                ("alpha".into(), "beta".into(), Some("beta".into())),
                ("alpha".into(), "ghost-idea".into(), None),
                ("beta".into(), "alpha".into(), Some("alpha".into())),
                ("beta".into(), "durable-one".into(), None),
                ("beta".into(), "gamma".into(), None),
            ]
        );
    }

    #[test]
    fn fact_links_resolve_cross_idea_reference() {
        let tmp = tempfile::tempdir().unwrap();
        build_fixture_vault(tmp.path());
        let mut conn = mem_conn();
        reindex(&mut conn, tmp.path()).unwrap();

        // Resolved means dst_fact_id names the fact with that slug inside that other idea.
        let resolved_cross_idea: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM fact_links fl
                 JOIN ideas s ON s.id = fl.src_idea_id
                 JOIN memory_facts df ON df.id = fl.dst_fact_id
                 JOIN ideas di ON di.id = df.idea_id
                 WHERE di.slug = fl.dst_idea_slug AND df.slug = fl.dst_fact_slug
                   AND di.slug <> s.slug",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(resolved_cross_idea, 1);

        assert_eq!(
            queries::fact_links_from(&conn, "alpha").unwrap(),
            vec![
                FactLink {
                    src_fact: None,
                    dst_idea: "beta".into(),
                    dst_fact: "durable-one".into(),
                    resolved: true,
                },
                FactLink {
                    src_fact: Some("alpha-note".into()),
                    dst_idea: "beta".into(),
                    dst_fact: "no-such-fact".into(),
                    resolved: false,
                },
            ]
        );
    }

    #[test]
    fn fact_links_bare_same_idea_link_resolves_and_idea_links_are_not_duplicated() {
        let tmp = tempfile::tempdir().unwrap();
        build_fixture_vault(tmp.path());
        let mut conn = mem_conn();
        reindex(&mut conn, tmp.path()).unwrap();

        // durable-two names durable-one twice (body + frontmatter) — one row, resolved, bare.
        assert_eq!(
            queries::fact_links_from(&conn, "beta").unwrap(),
            vec![FactLink {
                src_fact: Some("durable-two".into()),
                dst_idea: "beta".into(),
                dst_fact: "durable-one".into(),
                resolved: true,
            }]
        );
        let explicit: i64 = conn
            .query_row(
                "SELECT explicit FROM fact_links WHERE dst_fact_slug = 'durable-one'
                   AND src_fact_id IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(explicit, 0);

        // durable-one's bare [[alpha]]/[[gamma]] are idea links: they stay in backlinks only.
        let idea_targets_as_facts: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM fact_links WHERE dst_fact_slug IN ('alpha', 'gamma')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(idea_targets_as_facts, 0);
        let beta_backlinks: Vec<String> = queries::links_from(&conn, "beta")
            .unwrap()
            .into_iter()
            .map(|l| l.target_slug)
            .collect();
        assert_eq!(beta_backlinks, ["alpha", "gamma", "durable-one"]);
    }

    #[test]
    fn fact_links_dangling_explicit_ref_stays_unresolved_and_resolves_later() {
        let tmp = tempfile::tempdir().unwrap();
        build_fixture_vault(tmp.path());
        let mut conn = mem_conn();
        reindex(&mut conn, tmp.path()).unwrap();

        let dangling = |conn: &Connection| -> Option<bool> {
            queries::fact_links_from(conn, "alpha")
                .unwrap()
                .into_iter()
                .find(|l| l.dst_fact == "no-such-fact")
                .map(|l| l.resolved)
        };
        assert_eq!(dangling(&conn), Some(false), "kept, unresolved");

        store::write_memory_fact(
            tmp.path(),
            "beta",
            &fact("no-such-fact", "Now it exists", &[], "Created later.\n"),
        )
        .unwrap();
        reindex(&mut conn, tmp.path()).unwrap();
        assert_eq!(
            dangling(&conn),
            Some(true),
            "re-resolved on the next reindex"
        );
    }

    fn backlink_rows(conn: &Connection) -> Vec<(String, String)> {
        let mut stmt = conn
            .prepare(
                "SELECT s.slug, b.target_slug FROM backlinks b
                 JOIN ideas s ON s.id = b.source_idea_id ORDER BY s.slug, b.target_slug",
            )
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    fn resolved_fact_link_rows(conn: &Connection) -> Vec<(String, String, String, String)> {
        let mut stmt = conn
            .prepare(
                "SELECT si.slug, COALESCE(sf.slug, ''), di.slug, df.slug FROM fact_links fl
                 JOIN ideas si ON si.id = fl.src_idea_id
                 LEFT JOIN memory_facts sf ON sf.id = fl.src_fact_id
                 JOIN memory_facts df ON df.id = fl.dst_fact_id
                 JOIN ideas di ON di.id = df.idea_id
                 ORDER BY 1, 2, 3, 4",
            )
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    fn owned(rows: &[(&str, &str, &str, &str)]) -> Vec<(String, String, String, String)> {
        rows.iter()
            .map(|(a, b, c, d)| (a.to_string(), b.to_string(), c.to_string(), d.to_string()))
            .collect()
    }

    #[test]
    fn fact_links_ref_into_own_idea_adds_no_self_backlink() {
        let tmp = tempfile::tempdir().unwrap();
        store::write_idea(
            tmp.path(),
            &idea(
                "solo",
                "Solo",
                IdeaState::Draft,
                &[],
                "See [[solo#core]].\n",
            ),
        )
        .unwrap();
        store::write_memory_fact(
            tmp.path(),
            "solo",
            &fact("core", "Core", &[], "Restated in [[solo#core-two]].\n"),
        )
        .unwrap();
        store::write_memory_fact(tmp.path(), "solo", &fact("core-two", "Two", &[], "x\n")).unwrap();
        let mut conn = mem_conn();
        reindex(&mut conn, tmp.path()).unwrap();

        assert_eq!(backlink_rows(&conn), Vec::<(String, String)>::new());
        assert_eq!(
            resolved_fact_link_rows(&conn),
            owned(&[
                ("solo", "", "solo", "core"),
                ("solo", "core", "solo", "core-two")
            ])
        );
    }

    #[test]
    fn fact_links_fact_linking_to_itself_is_not_a_row() {
        let tmp = tempfile::tempdir().unwrap();
        store::write_idea(
            tmp.path(),
            &idea("solo", "Solo", IdeaState::Draft, &[], "body\n"),
        )
        .unwrap();
        store::write_memory_fact(
            tmp.path(),
            "solo",
            &fact(
                "loop",
                "Loop",
                &["loop"],
                "Echo [[loop]] and [[solo#loop]].\n",
            ),
        )
        .unwrap();
        let mut conn = mem_conn();
        reindex(&mut conn, tmp.path()).unwrap();

        assert_eq!(resolved_fact_link_rows(&conn), owned(&[]));
    }

    #[test]
    fn fact_links_fact_ref_alone_adds_an_inbound_idea_backlink() {
        let tmp = tempfile::tempdir().unwrap();
        store::write_idea(
            tmp.path(),
            &idea("src", "Src", IdeaState::Draft, &[], "Only [[dst#claim]].\n"),
        )
        .unwrap();
        store::write_idea(
            tmp.path(),
            &idea("dst", "Dst", IdeaState::Draft, &[], "body\n"),
        )
        .unwrap();
        store::write_memory_fact(tmp.path(), "dst", &fact("claim", "Claim", &[], "x\n")).unwrap();
        let mut conn = mem_conn();
        reindex(&mut conn, tmp.path()).unwrap();

        assert_eq!(
            backlink_rows(&conn),
            vec![("src".to_string(), "dst".to_string())]
        );
        assert_eq!(
            queries::backlinks_for(&conn, "dst").unwrap(),
            vec!["src".to_string()]
        );
    }

    #[test]
    fn fact_links_bare_link_resolves_within_its_own_idea_when_slugs_collide() {
        let tmp = tempfile::tempdir().unwrap();
        for slug in ["one", "two"] {
            store::write_idea(
                tmp.path(),
                &idea(slug, slug, IdeaState::Draft, &[], "body\n"),
            )
            .unwrap();
            store::write_memory_fact(tmp.path(), slug, &fact("shared", "Shared", &[], "x\n"))
                .unwrap();
        }
        store::write_memory_fact(
            tmp.path(),
            "two",
            &fact("pointer", "Pointer", &[], "Builds on [[shared]].\n"),
        )
        .unwrap();
        let mut conn = mem_conn();
        reindex(&mut conn, tmp.path()).unwrap();

        assert_eq!(
            resolved_fact_link_rows(&conn),
            owned(&[("two", "pointer", "two", "shared")])
        );
    }

    #[test]
    fn forward_reference_resolves_on_a_later_reindex() {
        let tmp = tempfile::tempdir().unwrap();
        build_fixture_vault(tmp.path());
        let mut conn = mem_conn();
        reindex(&mut conn, tmp.path()).unwrap();

        // The dangling [[ghost-idea]] target gets created later …
        store::write_idea(
            tmp.path(),
            &idea("ghost-idea", "Ghost", IdeaState::Draft, &[], "now real\n"),
        )
        .unwrap();
        reindex(&mut conn, tmp.path()).unwrap();

        // … and the next reindex re-resolves it (D23).
        let resolved: Option<String> = conn
            .query_row(
                "SELECT t.slug FROM backlinks b
                 JOIN ideas s ON s.id = b.source_idea_id
                 LEFT JOIN ideas t ON t.id = b.target_idea_id
                 WHERE s.slug = 'alpha' AND b.target_slug = 'ghost-idea'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(resolved, Some("ghost-idea".into()));
    }

    #[test]
    fn deleted_target_reverts_backlink_to_unresolved() {
        let tmp = tempfile::tempdir().unwrap();
        build_fixture_vault(tmp.path());
        let mut conn = mem_conn();
        reindex(&mut conn, tmp.path()).unwrap();

        // alpha -> beta resolves while beta exists; deleting `vault/beta/` must revert it to
        // NULL on the next rebuild (D23 re-resolution works in both directions).
        std::fs::remove_dir_all(tmp.path().join("beta")).unwrap();
        reindex(&mut conn, tmp.path()).unwrap();

        let resolved: Option<String> = conn
            .query_row(
                "SELECT t.slug FROM backlinks b
                 JOIN ideas s ON s.id = b.source_idea_id
                 LEFT JOIN ideas t ON t.id = b.target_idea_id
                 WHERE s.slug = 'alpha' AND b.target_slug = 'beta'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(resolved, None);
    }

    #[test]
    fn fts_covers_idea_body_and_conversation() {
        let tmp = tempfile::tempdir().unwrap();
        build_fixture_vault(tmp.path());
        let mut conn = mem_conn();
        reindex(&mut conn, tmp.path()).unwrap();

        let kind: String = conn
            .query_row(
                "SELECT kind FROM search_fts WHERE search_fts MATCH 'ground'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kind, "conversation");
        let kind: String = conn
            .query_row(
                "SELECT kind FROM search_fts WHERE search_fts MATCH 'statement'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kind, "idea_body");
    }

    #[test]
    fn sanitized_strips_snippet_sentinels_but_leaves_ordinary_text_alone() {
        // Defensive half of the snippet-sentinel contract (docs on SNIPPET_MATCH_OPEN/CLOSE):
        // even if a sentinel codepoint somehow reached indexed content, it must never survive
        // into search_fts, or queries::search's snippet() marking would become ambiguous.
        let poisoned = format!("before {SNIPPET_MATCH_OPEN}mid{SNIPPET_MATCH_CLOSE} after");
        assert_eq!(sanitized(&poisoned), "before mid after");
        assert_eq!(sanitized("ordinary café text"), "ordinary café text");
    }

    #[test]
    fn fts_covers_title_tags_and_memory_fact_bodies() {
        // Coverage regression: title, tags, and memory-fact bodies were previously never written
        // to search_fts at all (memory_facts has no body column — fact bodies were unsearchable
        // truth). Three terms, each planted in exactly one of those three surfaces and nowhere
        // else in the fixture, prove all three are now indexed under the right `kind`.
        let tmp = tempfile::tempdir().unwrap();
        store::write_idea(
            tmp.path(),
            &idea(
                "gamma",
                "Zoravian Cascade",
                IdeaState::Draft,
                &["ephemeral-widgets"],
                "A plain idea body with no special vocabulary.\n",
            ),
        )
        .unwrap();
        store::write_memory_fact(
            tmp.path(),
            "gamma",
            &fact(
                "insight-one",
                "Insight one",
                &[],
                "The durable conclusion mentions quixotic phrasing nowhere else.\n",
            ),
        )
        .unwrap();

        let mut conn = mem_conn();
        reindex(&mut conn, tmp.path()).unwrap();

        let kind: String = conn
            .query_row(
                "SELECT kind FROM search_fts WHERE search_fts MATCH 'zoravian'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kind, "title");

        let kind: String = conn
            .query_row(
                "SELECT kind FROM search_fts WHERE search_fts MATCH 'ephemeral'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kind, "tags");

        let kind: String = conn
            .query_row(
                "SELECT kind FROM search_fts WHERE search_fts MATCH 'quixotic'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kind, "memory");
    }

    #[test]
    fn fts_covers_artifacts_but_never_mines_them_for_backlinks() {
        let tmp = tempfile::tempdir().unwrap();
        build_fixture_vault(tmp.path());
        let mut conn = mem_conn();
        reindex(&mut conn, tmp.path()).unwrap();

        // "flywheel" appears only in the artifact body.
        let kind: String = conn
            .query_row(
                "SELECT kind FROM search_fts WHERE search_fts MATCH 'flywheel'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kind, "artifact");
        let artifact_ref: String = conn
            .query_row(
                "SELECT ref FROM search_fts WHERE search_fts MATCH 'flywheel'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(artifact_ref, "20260708-193045-key-decisions");

        // The artifact's [[beta]] link is NOT a backlink (alpha's only targets come from its
        // body and facts: beta + ghost-idea).
        let alpha_links: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM backlinks b JOIN ideas s ON s.id = b.source_idea_id
                 WHERE s.slug = 'alpha'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(alpha_links, 2);
    }

    #[test]
    fn html_artifact_export_is_excluded_from_the_index() {
        let tmp = tempfile::tempdir().unwrap();
        build_fixture_vault(tmp.path());
        let mut conn = mem_conn();
        reindex(&mut conn, tmp.path()).unwrap();
        let snap = snapshot(&conn);

        // The fixture writes a .html export; like compacted.md, a derived file must never
        // change the index or become searchable (docs/adr/0015).
        assert!(
            !snap.iter().any(|r| r.contains("UNINDEXED-REPORT")),
            "the .html report export is never searchable"
        );
        // And a second export appearing later does not perturb a rebuild.
        store::write_artifact_html(tmp.path(), "beta", "late-report", "<p>UNINDEXED-REPORT</p>")
            .unwrap();
        let mut fresh = mem_conn();
        reindex(&mut fresh, tmp.path()).unwrap();
        assert_eq!(snap, snapshot(&fresh));
        assert!(!check_drift(&fresh, tmp.path()).unwrap());
    }

    #[test]
    fn check_drift_false_after_reindex_true_after_edit_or_on_empty_db() {
        let tmp = tempfile::tempdir().unwrap();
        build_fixture_vault(tmp.path());
        let mut conn = mem_conn();
        conn.pragma_update(None, "user_version", schema::SCHEMA_VERSION)
            .unwrap();

        // Empty index + non-empty vault = drift.
        assert!(check_drift(&conn, tmp.path()).unwrap());

        reindex(&mut conn, tmp.path()).unwrap();
        assert!(!check_drift(&conn, tmp.path()).unwrap());

        // Edit an idea (bump `updated`) — drift until the next reindex.
        let mut edited = idea(
            "alpha",
            "Alpha",
            IdeaState::InDiscussion,
            &["markets", "risk"],
            "edited body\n",
        );
        edited.frontmatter.updated = dt(23);
        store::write_idea(tmp.path(), &edited).unwrap();
        assert!(check_drift(&conn, tmp.path()).unwrap());

        reindex(&mut conn, tmp.path()).unwrap();
        assert!(!check_drift(&conn, tmp.path()).unwrap());
    }

    #[test]
    fn reindex_rebuilds_a_derived_table_whose_layout_predates_the_current_schema() {
        let tmp = tempfile::tempdir().unwrap();
        build_fixture_vault(tmp.path());
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE edges (src_idea_id INTEGER, dst_idea_id INTEGER);")
            .unwrap();
        schema::apply_schema(&conn).unwrap();

        reindex(&mut conn, tmp.path()).unwrap();

        let edges: i64 = conn
            .query_row("SELECT COUNT(*) FROM edges WHERE detail <> ''", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(edges > 0);
        assert!(!check_drift(&conn, tmp.path()).unwrap());
    }

    #[test]
    fn reindex_upgrades_a_search_table_without_the_ref_column() {
        let tmp = tempfile::tempdir().unwrap();
        build_fixture_vault(tmp.path());
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE VIRTUAL TABLE search_fts USING fts5(
                 idea_id UNINDEXED, kind UNINDEXED, content);
             PRAGMA user_version = 3;",
        )
        .unwrap();
        schema::apply_schema(&conn).unwrap();
        assert!(check_drift(&conn, tmp.path()).unwrap());

        reindex(&mut conn, tmp.path()).unwrap();

        assert!(!crate::index::queries::search(&conn, "flywheel")
            .unwrap()
            .is_empty());
        let hits = crate::index::queries::vault_search(&conn, "refines", Some("alpha"), 5).unwrap();
        let got: Vec<_> = hits
            .iter()
            .map(|h| (h.idea_slug.as_str(), h.fact_slug.as_str()))
            .collect();
        assert_eq!(got, [("beta", "durable-two")]);
    }

    #[test]
    fn check_drift_true_when_the_index_predates_the_current_schema() {
        let tmp = tempfile::tempdir().unwrap();
        build_fixture_vault(tmp.path());
        let mut conn = mem_conn();
        reindex(&mut conn, tmp.path()).unwrap();
        assert!(!check_drift(&conn, tmp.path()).unwrap());

        // An index built by an older binary: idea metadata matches, the derived tables it
        // never knew about are empty.
        conn.execute_batch("DELETE FROM edges; DELETE FROM fact_links; PRAGMA user_version = 0;")
            .unwrap();
        assert!(check_drift(&conn, tmp.path()).unwrap());

        reindex(&mut conn, tmp.path()).unwrap();
        assert!(!check_drift(&conn, tmp.path()).unwrap());
        let edges: i64 = conn
            .query_row("SELECT COUNT(*) FROM edges", [], |r| r.get(0))
            .unwrap();
        assert!(edges > 0);
    }

    #[test]
    fn compacted_md_sidecar_is_excluded_from_the_index() {
        use crate::domain::{Compacted, CompactedFrontmatter};
        let tmp = tempfile::tempdir().unwrap();
        build_fixture_vault(tmp.path());
        let mut conn = mem_conn();
        let before = {
            reindex(&mut conn, tmp.path()).unwrap();
            snapshot(&conn)
        };

        // Drop a compacted.md sidecar next to an idea — reindex reads only idea.md /
        // conversation.md / memory/*.md, so a derived summary must never change the index
        // (auto-compact keeps the reindex invariant trivially intact, docs/adr/0012).
        store::write_compacted(
            tmp.path(),
            "alpha",
            &Compacted {
                frontmatter: CompactedFrontmatter {
                    compacted_through: 1,
                    covered_bytes: 10,
                    turn_count_at_compaction: 1,
                    model: "test".into(),
                    updated: dt(12),
                },
                summary: "## Decisions\n- UNINDEXED-SUMMARY\n".into(),
            },
        )
        .unwrap();

        let mut fresh = mem_conn();
        reindex(&mut fresh, tmp.path()).unwrap();
        let after = snapshot(&fresh);
        assert_eq!(before, after, "compacted.md does not affect the index");
        assert!(
            !after.iter().any(|r| r.contains("UNINDEXED-SUMMARY")),
            "the rolling summary is never searchable"
        );
        // And it does not register as drift.
        assert!(!check_drift(&fresh, tmp.path()).unwrap());
    }

    #[test]
    fn unparsable_idea_is_skipped_not_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        build_fixture_vault(tmp.path());
        // A malformed idea dir: has idea.md, but no valid frontmatter fence.
        std::fs::create_dir_all(tmp.path().join("broken")).unwrap();
        std::fs::write(tmp.path().join("broken/idea.md"), "no fence at all\n").unwrap();

        let mut conn = mem_conn();
        let counts = reindex(&mut conn, tmp.path()).unwrap();
        assert_eq!(counts.ideas, 3); // broken is skipped, the rest indexed

        // And the skip is stable: drift check ignores it the same way.
        assert!(!check_drift(&conn, tmp.path()).unwrap());
    }

    #[test]
    fn reindex_skips_an_idea_from_a_newer_format_and_keeps_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        build_fixture_vault(tmp.path());
        std::fs::create_dir_all(tmp.path().join("future")).unwrap();
        std::fs::write(
            tmp.path().join("future/idea.md"),
            "---\nversion: 2\ntitle: Future\nslug: future\nstate: draft\n\
created: 2026-07-07T10:00:00Z\nupdated: 2026-07-07T10:00:00Z\n---\n\nFrom a newer build.\n",
        )
        .unwrap();

        let mut conn = mem_conn();
        let counts = reindex(&mut conn, tmp.path()).unwrap();
        assert_eq!(counts.ideas, 3); // the too-new idea is skipped, never rewritten or half-indexed
        assert!(!check_drift(&conn, tmp.path()).unwrap());
        assert!(std::fs::read_to_string(tmp.path().join("future/idea.md"))
            .unwrap()
            .starts_with("---\nversion: 2\n"));
    }

    fn lexical_edge_rows(conn: &Connection) -> Vec<(String, String, f64, String)> {
        edge_rows(conn)
            .into_iter()
            .filter(|edge| edge.2 == "lexical")
            .map(|(a, b, _, weight, detail)| (a, b, weight, detail))
            .collect()
    }

    fn write_hub_vault(vault: &Path) {
        write_linked_ideas(
            vault,
            &[
                ("hub", "Wombat quasar tundra hubonly."),
                ("n1", "Wombat quasar tundra kelp fjord brine."),
                ("n2", "Wombat quasar tundra kelp fjord brine."),
                ("n3", "Wombat quasar tundra kelp fjord brine."),
                ("n4", "Wombat quasar tundra kelp fjord brine."),
                ("n5", "Wombat quasar tundra kelp fjord brine."),
                ("f1", "Solitary tangerine."),
                ("f2", "Distant glacier."),
            ],
        );
    }

    #[test]
    fn lexical_edge_joins_two_ideas_sharing_three_rare_words() {
        let tmp = tempfile::tempdir().unwrap();
        write_linked_ideas(
            tmp.path(),
            &[
                ("north", "Quokka lantern fjord zephyr common filler."),
                ("south", "Quokka lantern fjord marmot common filler."),
                ("east", "Common filler only here."),
            ],
        );
        let mut conn = mem_conn();
        reindex(&mut conn, tmp.path()).unwrap();

        let rows = lexical_edge_rows(&conn);
        assert_eq!(rows.len(), 1, "{rows:?}");
        let (a, b, weight, detail) = &rows[0];
        assert_eq!((a.as_str(), b.as_str()), ("north", "south"));
        assert_eq!(
            *weight, LEXICAL_EDGE_WEIGHT,
            "each side is the other's top hit"
        );
        assert_eq!(detail, "fjord, lantern, quokka");
    }

    #[test]
    fn lexical_edge_needs_three_shared_terms() {
        let tmp = tempfile::tempdir().unwrap();
        write_linked_ideas(
            tmp.path(),
            &[
                ("north", "Quokka lantern zephyr common filler."),
                ("south", "Quokka lantern marmot common filler."),
                ("east", "Common filler here."),
            ],
        );
        let mut conn = mem_conn();
        reindex(&mut conn, tmp.path()).unwrap();

        assert_eq!(
            queries::lexical_baseline(&conn, "north", 3)
                .unwrap()
                .iter()
                .map(|hit| hit.idea_slug.as_str())
                .collect::<Vec<_>>(),
            ["south"],
            "the pair is a baseline hit, so only the shared-term guard can drop it"
        );
        assert!(lexical_edge_rows(&conn).is_empty());
    }

    #[test]
    fn lexical_edge_caps_each_idea_at_two_outgoing_candidates() {
        let tmp = tempfile::tempdir().unwrap();
        write_hub_vault(tmp.path());
        let mut conn = mem_conn();
        reindex(&mut conn, tmp.path()).unwrap();

        assert_eq!(
            queries::lexical_baseline(&conn, "hub", 10).unwrap().len(),
            5,
            "all five neighbours are baseline hits of the hub"
        );
        for n in ["n1", "n2", "n3", "n4", "n5"] {
            assert!(
                queries::lexical_baseline(&conn, n, 3)
                    .unwrap()
                    .iter()
                    .all(|hit| hit.idea_slug != "hub"),
                "{n} must not propose the hub, so every hub edge is the hub's own proposal"
            );
        }
        let touching_hub: Vec<(String, String, f64, String)> = lexical_edge_rows(&conn)
            .into_iter()
            .filter(|(a, b, _, _)| a == "hub" || b == "hub")
            .collect();
        assert_eq!(
            touching_hub,
            vec![
                (
                    "hub".into(),
                    "n1".into(),
                    LEXICAL_EDGE_WEIGHT,
                    "quasar, tundra, wombat".into()
                ),
                (
                    "hub".into(),
                    "n2".into(),
                    LEXICAL_EDGE_WEIGHT,
                    "quasar, tundra, wombat".into()
                ),
            ]
        );
    }

    #[test]
    fn lexical_edge_rows_are_byte_identical_across_reindexes() {
        let tmp = tempfile::tempdir().unwrap();
        write_hub_vault(tmp.path());
        let raw = |conn: &Connection| -> Vec<(i64, i64, String, u64, String)> {
            let mut stmt = conn
                .prepare("SELECT * FROM edges WHERE type = 'lexical' ORDER BY 1, 2")
                .unwrap();
            stmt.query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get::<_, f64>(3)?.to_bits(),
                    r.get(4)?,
                ))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect()
        };

        let mut conn = mem_conn();
        reindex(&mut conn, tmp.path()).unwrap();
        let first = raw(&conn);
        assert!(!first.is_empty());
        assert!(
            first
                .iter()
                .all(|row| f64::from_bits(row.3) > 0.0
                    && f64::from_bits(row.3) <= LEXICAL_EDGE_WEIGHT)
        );

        reindex(&mut conn, tmp.path()).unwrap();
        assert_eq!(first, raw(&conn));

        let mut fresh = mem_conn();
        reindex(&mut fresh, tmp.path()).unwrap();
        assert_eq!(first, raw(&fresh));
    }

    #[test]
    fn lexical_edge_related_excludes_own_slug() {
        let tmp = tempfile::tempdir().unwrap();
        write_linked_ideas(
            tmp.path(),
            &[
                (
                    "north",
                    "Builds on [[south]] with quokka lantern fjord zephyr.",
                ),
                ("south", "North quokka lantern fjord marmot."),
                ("east", "Unrelated glacier."),
            ],
        );
        let mut conn = mem_conn();
        reindex(&mut conn, tmp.path()).unwrap();

        let of_north = queries::related_ideas(&conn, "north", 10).unwrap();
        assert_eq!(
            of_north
                .iter()
                .map(|r| (r.slug.as_str(), r.hops, r.reasons.clone()))
                .collect::<Vec<_>>(),
            [(
                "south",
                1,
                vec!["lexical: fjord, lantern, quokka; link: north → south".to_string()]
            )]
        );
        let of_south = queries::related_ideas(&conn, "south", 10).unwrap();
        assert_eq!(
            of_south.iter().map(|r| r.slug.as_str()).collect::<Vec<_>>(),
            ["north"]
        );
    }

    #[test]
    fn lexical_edge_ignores_a_word_every_title_carries() {
        let tmp = tempfile::tempdir().unwrap();
        for (slug, title, body) in [
            ("north", "Project north", "Quokka lantern zephyr filler."),
            ("south", "Project south", "Quokka lantern marmot filler."),
            ("east", "Project east", "Filler here."),
        ] {
            store::write_idea(
                tmp.path(),
                &idea(slug, title, IdeaState::InDiscussion, &[], body),
            )
            .unwrap();
        }
        let mut conn = mem_conn();
        reindex(&mut conn, tmp.path()).unwrap();

        assert_eq!(
            queries::lexical_baseline(&conn, "north", 3).unwrap()[0].idea_slug,
            "south",
            "the pair is the top baseline hit, so only the shared-term guards can drop it"
        );
        assert!(
            lexical_edge_rows(&conn).is_empty(),
            "{:?}",
            lexical_edge_rows(&conn)
        );
    }

    #[test]
    fn lexical_edge_does_not_count_a_linked_slug_as_a_shared_term() {
        let tmp = tempfile::tempdir().unwrap();
        write_linked_ideas(
            tmp.path(),
            &[
                ("north", "Builds on [[south]] with quokka lantern."),
                ("south", "Quokka lantern marmot."),
                ("east", "Unrelated glacier."),
            ],
        );
        let mut conn = mem_conn();
        reindex(&mut conn, tmp.path()).unwrap();

        assert_eq!(
            queries::lexical_baseline(&conn, "north", 3).unwrap()[0].idea_slug,
            "south",
            "the pair is the top baseline hit, so only the shared-term guards can drop it"
        );
        assert!(edge_rows(&conn).iter().any(|edge| edge.2 == "link"));
        assert!(
            lexical_edge_rows(&conn).is_empty(),
            "{:?}",
            lexical_edge_rows(&conn)
        );
    }

    #[test]
    fn lexical_edge_counts_shared_title_words_of_unlinked_ideas() {
        let tmp = tempfile::tempdir().unwrap();
        for (slug, title, body) in [
            (
                "solar-kiln-ceramic-quokka",
                "Solar Kiln Ceramic Quokka",
                "Pottery wheel.",
            ),
            (
                "solar-kiln-ceramic-marmot",
                "Solar Kiln Ceramic Marmot",
                "Brick oven.",
            ),
            ("glacier-harbor", "Glacier Harbor", "Unrelated tide."),
        ] {
            store::write_idea(
                tmp.path(),
                &idea(slug, title, IdeaState::InDiscussion, &[], body),
            )
            .unwrap();
        }
        let mut conn = mem_conn();
        reindex(&mut conn, tmp.path()).unwrap();

        assert!(edge_rows(&conn).iter().all(|edge| edge.2 != "link"));
        assert_eq!(
            lexical_edge_rows(&conn),
            vec![(
                "solar-kiln-ceramic-marmot".to_string(),
                "solar-kiln-ceramic-quokka".to_string(),
                LEXICAL_EDGE_WEIGHT,
                "ceramic, kiln, solar".to_string()
            )]
        );
    }

    #[test]
    fn lexical_edge_two_hop_lexical_only_path_stays_below_the_related_floor() {
        let tmp = tempfile::tempdir().unwrap();
        write_linked_ideas(
            tmp.path(),
            &[
                ("north", "Quokka lantern fjord zephyr."),
                ("middle", "Quokka lantern fjord marmot tundra kelp."),
                ("south", "Marmot tundra kelp glacier."),
                ("east", "Unrelated tide pool."),
            ],
        );
        let mut conn = mem_conn();
        reindex(&mut conn, tmp.path()).unwrap();

        let edges: Vec<(String, String, String)> = edge_rows(&conn)
            .into_iter()
            .map(|(a, b, kind, _, _)| (a, b, kind))
            .collect();
        assert_eq!(
            edges,
            vec![
                ("middle".into(), "north".into(), "lexical".into()),
                ("middle".into(), "south".into(), "lexical".into()),
            ]
        );
        let related = scored(&conn, "north", 10);
        assert_eq!(related[0], ("middle".to_string(), 1, LEXICAL_EDGE_WEIGHT));
        let south = related
            .iter()
            .find(|(slug, _, _)| slug == "south")
            .expect("south is reachable in two hops");
        assert_eq!(south.1, 2);
        assert!(
            south.2 < 0.1,
            "two-hop lexical-only score {} must stay under memory::related::MIN_RELATED_SCORE (0.1)",
            south.2
        );
    }
}
