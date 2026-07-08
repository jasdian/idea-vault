//! Read queries over the derived index (docs/03-data-model.md §D6).
//!
//! These are pure reads of derived tables; they never mutate truth. If the index is stale a
//! reindex reconciles it (ADR-0002).

use std::collections::{HashMap, HashSet};

use rusqlite::Connection;

use super::{IndexError, SNIPPET_MATCH_CLOSE, SNIPPET_MATCH_OPEN};

/// One row of the idea list, projected for the vault overview UI.
#[derive(Debug, Clone)]
pub struct IdeaSummary {
    pub slug: String,
    pub title: String,
    pub state: String,
    pub updated_at: String,
    /// The idea's tags (alphabetical) — rendered as clickable filter chips on the list rows.
    pub tags: Vec<String>,
}

/// Split the space-joined tag column back into a list (empty string ⇒ no tags).
fn split_tags(joined: String) -> Vec<String> {
    joined.split_whitespace().map(str::to_string).collect()
}

/// A single full-text search result.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub slug: String,
    pub title: String,
    /// Plain text with the matched span(s) delimited by [`SNIPPET_MATCH_OPEN`]/
    /// [`SNIPPET_MATCH_CLOSE`] (Private-Use-Area sentinels, not HTML). The web layer must escape
    /// this string first, then translate the sentinel pair into highlight markup (e.g.
    /// `<mark>`) — never the other way around, or the markup itself would be escaped away. See
    /// the contract doc on the sentinel constants in `crate::index`.
    pub snippet: String,
    /// The `search_fts.kind` of this idea's best-ranked matching row (e.g. `"title"`,
    /// `"memory"`) — lets the UI show *why* an idea matched, not just that it did.
    pub kind: String,
}

/// List every indexed idea, most-recently-updated first.
pub fn list_ideas(conn: &Connection) -> Result<Vec<IdeaSummary>, IndexError> {
    // Tags ride along as one space-joined column (tag names are slug-alphabet, so a space can
    // never appear inside one) — a second query per row would be a needless N+1.
    let mut stmt = conn.prepare(
        "SELECT i.slug, i.title, i.state, i.updated_at,
                COALESCE((SELECT GROUP_CONCAT(name, ' ') FROM
                            (SELECT t.name FROM idea_tags it JOIN tags t ON t.id = it.tag_id
                             WHERE it.idea_id = i.id ORDER BY t.name)), '')
         FROM ideas i ORDER BY i.updated_at DESC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(IdeaSummary {
            slug: row.get(0)?,
            title: row.get(1)?,
            state: row.get(2)?,
            updated_at: row.get(3)?,
            tags: split_tags(row.get::<_, String>(4)?),
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// One outbound `[[slug]]` link from an idea (D23): the raw target plus whether the last reindex
/// resolved it to an existing idea (`false` = forward/dangling reference).
#[derive(Debug, Clone, PartialEq)]
pub struct LinkTarget {
    pub target_slug: String,
    pub resolved: bool,
}

/// Turn raw user input into an FTS5 MATCH expression that can never be a syntax error: each
/// whitespace token becomes a quoted phrase with a `*` prefix wildcard (`"term"*`), embedded
/// quotes doubled, NUL bytes stripped (a `%00` in a query param is valid UTF-8 but terminates
/// SQLite's string parser mid-phrase). Implicit AND between tokens. Returns `None` for input
/// with no tokens.
fn fts_query(raw: &str) -> Option<String> {
    let tokens: Vec<String> = raw
        .split_whitespace()
        .map(|t| format!("\"{}\"*", t.replace('\0', "").replace('"', "\"\"")))
        .filter(|t| t != "\"\"*")
        .collect();
    if tokens.is_empty() {
        None
    } else {
        Some(tokens.join(" "))
    }
}

/// Per-`kind` bm25 multiplier — the field-weighting half of "google-style" ranking.
///
/// **Sign convention, read this first:** SQLite FTS5's `bm25()` returns an already-negative
/// score where *more negative is a better match* (verified empirically against this crate's
/// bundled SQLite: a short document with a dense match scores more negative than a long one with
/// a sparse match) — the reverse of the textbook positive-BM25 convention, but consistent with
/// this module's long-standing `ORDER BY bm25(search_fts)` (ascending = best first). Consequence:
/// to make a `kind` rank *better*, its multiplier must be *larger*, because a larger multiplier
/// pushes an already-negative number further from zero (more negative), not closer to it. That
/// is the opposite of what "weight" suggests at a glance, hence this comment.
///
/// Values (title strongest → artifact weakest): a title hit is almost always exactly what the
/// owner typed the query to find, so it dominates. Tags and memory-fact text are short, curated,
/// high-signal — the owner deliberately wrote a tag or distilled a fact, unlike the sprawling
/// idea body/conversation, which stay at the 1.0 baseline. Artifacts are AI-generated synthesis
/// (docs/adr/0015) — useful, but the least "the owner's own words" of the searchable surfaces, so
/// they sit below baseline.
const WEIGHT_TITLE: f64 = 4.0;
const WEIGHT_TAGS: f64 = 2.5;
const WEIGHT_MEMORY: f64 = 2.0;
const WEIGHT_BASELINE: f64 = 1.0; // idea_body, conversation, and any future/unknown kind
const WEIGHT_ARTIFACT: f64 = 0.85;

fn kind_weight(kind: &str) -> f64 {
    match kind {
        "title" => WEIGHT_TITLE,
        "tags" => WEIGHT_TAGS,
        "memory" => WEIGHT_MEMORY,
        "artifact" => WEIGHT_ARTIFACT,
        _ => WEIGHT_BASELINE,
    }
}

/// Backlink prior — the "google" part (PageRank-flavored, not literally PageRank: a simple
/// inbound-`[[slug]]`-count prior is plenty at this corpus size). `log(1 + inbound)` so the first
/// few backlinks matter far more than the hundredth (diminishing returns, not a popularity
/// contest), and the raw count is capped before the log so one absurdly-linked idea can't buy an
/// unbounded boost. The coefficient is deliberately small relative to a `kind_weight` swing (0.85
/// to 4.0, a ~4.7x range): at the cap, the maximum possible boost is
/// `BACKLINK_BOOST * ln(1 + BACKLINK_CAP)` ≈ 0.15 * ln(26) ≈ 0.49, well under a single
/// `kind_weight` step — so backlinks can decide a near-tie between comparably-relevant ideas, but
/// can never let a popular idea leapfrog a clearly better textual match.
const BACKLINK_BOOST: f64 = 0.15;
const BACKLINK_CAP: i64 = 25;

/// Multi-document-corroboration bonus — a small per-*extra-distinct-kind* nudge so an idea that
/// matches the query in several independent fields (e.g. both its body and its conversation)
/// outranks one that matches, at similar bm25, in only one. Same self-bounding logic as the
/// backlink prior: capped at 5 extra kinds (there are only 6 kinds total), so the maximum bonus
/// (`0.05 * 5` = 0.25) stays well under a `kind_weight` step and can only break near-ties.
const CORROBORATION_BONUS: f64 = 0.05;

/// How many raw `search_fts` rows to pull before weighting/dedup/re-ranking. Generous relative to
/// the final 50-hit cap: this is a single-owner vault (not a web-scale corpus), so a wide
/// pre-aggregation window is cheap, and it matters here specifically because the backlink and
/// corroboration adjustments below need to see *every* matching kind for the top ideas, not just
/// whichever kind bm25 alone ranked first.
const PRE_AGGREGATION_LIMIT: usize = 500;
/// Final cap on distinct ideas returned, unchanged from the original single-field ranking.
const MAX_HITS: usize = 50;

/// One aggregated candidate: the best-ranked matching row for an idea, plus everything the
/// re-ranking pass needs about that idea's other matches.
struct Candidate {
    title: String,
    best_kind: String,
    best_snippet: String,
    /// Lowest (best) `bm25(search_fts) * kind_weight(kind)` seen across this idea's rows.
    best_weighted: f64,
    /// Every distinct `kind` label matched, e.g. an idea with two memory facts that both match
    /// contributes two rows but exactly one entry ("memory") here — corroboration counts
    /// independent *fields*, not row volume within a field.
    kinds: HashSet<String>,
    inbound: i64,
}

/// Full-text search over every owner-authored `search_fts` surface (title, tags, idea body,
/// conversation, memory-fact bodies, and knowledge-extraction artifacts), joined back to `ideas`
/// for slug/title (R8), ranked google-style: bm25 per matching row, scaled by [`kind_weight`],
/// nudged by an inbound-backlink prior and a multi-kind-corroboration bonus (see the constants
/// above for the exact algebra and why each adjustment is bounded), then deduplicated to one best
/// hit per idea. This is one SQL query (fetch + raw bm25 + inbound count) followed by a small
/// Rust post-pass (weighting, dedup-with-aggregation, final sort) — the weighting/boost math
/// lives in Rust rather than SQL because SQLite's bundled build here has no `LN`/`LOG` function,
/// and duplicating the kind_weight CASE in both SQL and Rust would be two things to keep in sync.
///
/// The snippet is plain text with [`SNIPPET_MATCH_OPEN`]/[`SNIPPET_MATCH_CLOSE`] sentinel
/// delimiters around matched spans (see the doc comment on those constants) — not HTML. The web
/// layer must escape first, then translate the sentinels into markup.
pub fn search(conn: &Connection, query: &str) -> Result<Vec<SearchHit>, IndexError> {
    let Some(match_expr) = fts_query(query) else {
        return Ok(Vec::new());
    };

    let mut stmt = conn.prepare(
        "SELECT i.slug, i.title, s.kind,
                snippet(search_fts, 2, ?2, ?3, '…', 12),
                bm25(search_fts),
                (SELECT COUNT(*) FROM backlinks bl WHERE bl.target_idea_id = i.id)
         FROM search_fts s
         JOIN ideas i ON i.id = s.idea_id
         WHERE search_fts MATCH ?1
         ORDER BY bm25(search_fts)
         LIMIT ?4",
    )?;
    let rows = stmt.query_map(
        rusqlite::params![
            &match_expr,
            SNIPPET_MATCH_OPEN.to_string(),
            SNIPPET_MATCH_CLOSE.to_string(),
            PRE_AGGREGATION_LIMIT as i64,
        ],
        |row| {
            Ok((
                row.get::<_, String>(0)?, // slug
                row.get::<_, String>(1)?, // title
                row.get::<_, String>(2)?, // kind
                row.get::<_, String>(3)?, // snippet (sentinel-delimited)
                row.get::<_, f64>(4)?,    // raw bm25 for this row
                row.get::<_, i64>(5)?,    // inbound backlink count for this idea
            ))
        },
    )?;

    // Aggregate per idea: `order` preserves first-seen order (== ascending raw-bm25 scan order,
    // a reasonable base ordering) so the final stable sort's tie-breaking is deterministic rather
    // than dependent on HashMap iteration order.
    let mut by_slug: HashMap<String, Candidate> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for row in rows {
        let (slug, title, kind, snippet, raw_bm25, inbound) = row?;
        let weighted = raw_bm25 * kind_weight(&kind);
        match by_slug.get_mut(&slug) {
            Some(c) => {
                c.kinds.insert(kind.clone());
                if weighted < c.best_weighted {
                    c.best_weighted = weighted;
                    c.best_kind = kind;
                    c.best_snippet = snippet;
                }
            }
            None => {
                order.push(slug.clone());
                let mut kinds = HashSet::new();
                kinds.insert(kind.clone());
                by_slug.insert(
                    slug,
                    Candidate {
                        title,
                        best_kind: kind,
                        best_snippet: snippet,
                        best_weighted: weighted,
                        kinds,
                        inbound,
                    },
                );
            }
        }
    }

    let mut scored: Vec<(f64, SearchHit)> = order
        .into_iter()
        .map(|slug| {
            let c = by_slug
                .remove(&slug)
                .expect("slug was just pushed to order");
            let backlink_adjustment =
                BACKLINK_BOOST * (1.0 + c.inbound.min(BACKLINK_CAP) as f64).ln();
            let corroboration_adjustment = CORROBORATION_BONUS * (c.kinds.len() - 1) as f64;
            let final_score = c.best_weighted - backlink_adjustment - corroboration_adjustment;
            (
                final_score,
                SearchHit {
                    slug,
                    title: c.title,
                    snippet: c.best_snippet,
                    kind: c.best_kind,
                },
            )
        })
        .collect();
    // Stable sort: ties (identical final_score) keep the original bm25-scan order rather than an
    // arbitrary one.
    scored.sort_by(|a, b| a.0.total_cmp(&b.0));
    scored.truncate(MAX_HITS);

    Ok(scored.into_iter().map(|(_, hit)| hit).collect())
}

/// Inbound direction of D23: distinct slugs of ideas that link *to* `slug` via `[[slug]]`,
/// sorted. Matches on `target_slug`, so it also answers "who links to this not-yet-created
/// idea?" for forward references.
pub fn backlinks_for(conn: &Connection, slug: &str) -> Result<Vec<String>, IndexError> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT s.slug FROM backlinks b
         JOIN ideas s ON s.id = b.source_idea_id
         WHERE b.target_slug = ?1
         ORDER BY s.slug",
    )?;
    let rows = stmt.query_map([slug], |row| row.get(0))?;
    rows.collect::<Result<_, _>>().map_err(Into::into)
}

/// Outbound direction of D23: every `[[slug]]` target this idea links to, in first-occurrence
/// order (insertion order from reindex), with its resolution status.
pub fn links_from(conn: &Connection, slug: &str) -> Result<Vec<LinkTarget>, IndexError> {
    let mut stmt = conn.prepare(
        "SELECT b.target_slug, b.target_idea_id IS NOT NULL FROM backlinks b
         JOIN ideas s ON s.id = b.source_idea_id
         WHERE s.slug = ?1
         ORDER BY b.id",
    )?;
    let rows = stmt.query_map([slug], |row| {
        Ok(LinkTarget {
            target_slug: row.get(0)?,
            resolved: row.get(1)?,
        })
    })?;
    rows.collect::<Result<_, _>>().map_err(Into::into)
}

/// Every idea carrying `tag` in its frontmatter, most-recently-updated first.
pub fn ideas_with_tag(conn: &Connection, tag: &str) -> Result<Vec<IdeaSummary>, IndexError> {
    let mut stmt = conn.prepare(
        "SELECT i.slug, i.title, i.state, i.updated_at,
                COALESCE((SELECT GROUP_CONCAT(name, ' ') FROM
                            (SELECT t2.name FROM idea_tags it2 JOIN tags t2 ON t2.id = it2.tag_id
                             WHERE it2.idea_id = i.id ORDER BY t2.name)), '')
         FROM ideas i
         JOIN idea_tags it ON it.idea_id = i.id
         JOIN tags t ON t.id = it.tag_id
         WHERE t.name = ?1
         ORDER BY i.updated_at DESC",
    )?;
    let rows = stmt.query_map([tag], |row| {
        Ok(IdeaSummary {
            slug: row.get(0)?,
            title: row.get(1)?,
            state: row.get(2)?,
            updated_at: row.get(3)?,
            tags: split_tags(row.get::<_, String>(4)?),
        })
    })?;
    rows.collect::<Result<_, _>>().map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::schema::apply_schema;

    fn insert_idea(conn: &Connection, slug: &str, title: &str, state: &str, updated: &str) {
        conn.execute(
            "INSERT INTO ideas (slug, title, state, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![slug, title, state, updated, updated],
        )
        .unwrap();
    }

    #[test]
    fn list_ideas_orders_by_updated_desc() {
        let conn = Connection::open_in_memory().unwrap();
        apply_schema(&conn).unwrap();

        insert_idea(&conn, "old", "Old", "stored", "2026-07-01T00:00:00Z");
        insert_idea(&conn, "new", "New", "in_discussion", "2026-07-07T00:00:00Z");
        insert_idea(&conn, "mid", "Mid", "draft", "2026-07-04T00:00:00Z");

        let rows = list_ideas(&conn).unwrap();
        let slugs: Vec<_> = rows.iter().map(|r| r.slug.as_str()).collect();
        assert_eq!(slugs, ["new", "mid", "old"]);
        assert_eq!(rows[0].title, "New");
        assert_eq!(rows[0].state, "in_discussion");
    }

    // The query tests below go through the real pipeline — vault writes → reindex → query — so
    // the join shapes are proven against rows reindex actually produces, not hand-inserted ones.

    use chrono::{TimeZone, Utc};
    use std::path::Path;

    use crate::domain::{Idea, IdeaFrontmatter, IdeaState, MemoryFact, MemoryFactFrontmatter};
    use crate::index::reindex::reindex;
    use crate::vault::store;

    fn write_fixture_idea(
        vault: &Path,
        slug: &str,
        title: &str,
        tags: &[&str],
        body: &str,
        hour: u32,
    ) {
        store::write_idea(
            vault,
            &Idea {
                frontmatter: IdeaFrontmatter {
                    title: title.into(),
                    slug: slug.into(),
                    state: IdeaState::InDiscussion,
                    tags: tags.iter().map(|t| t.to_string()).collect(),
                    created: Utc.with_ymd_and_hms(2026, 7, 7, hour, 0, 0).unwrap(),
                    updated: Utc.with_ymd_and_hms(2026, 7, 7, hour, 0, 0).unwrap(),
                },
                body: body.into(),
            },
        )
        .unwrap();
    }

    fn indexed_fixture() -> (tempfile::TempDir, Connection) {
        let tmp = tempfile::tempdir().unwrap();
        write_fixture_idea(
            tmp.path(),
            "alpha",
            "Alpha market",
            &["markets", "risk"],
            "Alpha explores incentives and links [[beta]] plus [[ghost-idea]].\n",
            10,
        );
        write_fixture_idea(
            tmp.path(),
            "beta",
            "Beta",
            &["risk"],
            "Beta statement, nothing shared with the other body.\n",
            11,
        );
        store::append_conversation(
            tmp.path(),
            "beta",
            "## user\nlet us discuss incentives here too\n",
        )
        .unwrap();
        store::write_memory_fact(
            tmp.path(),
            "beta",
            &MemoryFact {
                frontmatter: MemoryFactFrontmatter {
                    slug: "durable".into(),
                    title: "Durable".into(),
                    tags: vec![],
                    created: Utc.with_ymd_and_hms(2026, 7, 7, 12, 0, 0).unwrap(),
                    links: vec!["alpha".into()],
                },
                body: "Fact body.\n".into(),
            },
        )
        .unwrap();

        let mut conn = Connection::open_in_memory().unwrap();
        apply_schema(&conn).unwrap();
        reindex(&mut conn, tmp.path()).unwrap();
        (tmp, conn)
    }

    #[test]
    fn search_hits_body_and_conversation_deduped_per_idea() {
        let (_tmp, conn) = indexed_fixture();

        // "incentives" appears in alpha's body AND beta's conversation → one hit per idea.
        let hits = search(&conn, "incentives").unwrap();
        let mut slugs: Vec<_> = hits.iter().map(|h| h.slug.as_str()).collect();
        slugs.sort();
        assert_eq!(slugs, ["alpha", "beta"]);
        assert!(hits.iter().all(|h| !h.snippet.is_empty()));
        assert!(hits.iter().all(|h| !h.title.is_empty()));
    }

    #[test]
    fn search_prefix_matches_partial_terms() {
        let (_tmp, conn) = indexed_fixture();
        // Search-as-you-type (R8, keyup-delayed): "incent" must already match "incentives".
        let hits = search(&conn, "incent").unwrap();
        assert!(!hits.is_empty());
    }

    #[test]
    fn search_never_errors_on_fts_hostile_input() {
        let (_tmp, conn) = indexed_fixture();
        for hostile in [
            "\"unbalanced",
            "AND OR NOT (",
            "a*b\"c",
            "-",
            "( ) \" \"",
            "a\0b",      // embedded NUL (%00 in a query param) — terminates SQLite's parser
            "\0",        // NUL-only token must vanish, not become an empty phrase
            "content:x", // column-filter syntax must be treated as a literal term
        ] {
            search(&conn, hostile).unwrap();
        }
        assert!(search(&conn, "").unwrap().is_empty());
        assert!(search(&conn, "   ").unwrap().is_empty());
        assert!(search(&conn, "\0 \0").unwrap().is_empty());
    }

    #[test]
    fn same_idea_matching_both_kinds_yields_one_best_hit() {
        let tmp = tempfile::tempdir().unwrap();
        // "zebra" appears once in a long body but densely in a short conversation — bm25 must
        // rank the conversation row better, and dedup keeps that best row's snippet.
        write_fixture_idea(
            tmp.path(),
            "solo",
            "Solo",
            &[],
            "A very long body sentence that mentions zebra exactly once among many many \
             other filler words stretching the document length considerably onward.\n",
            10,
        );
        store::append_conversation(tmp.path(), "solo", "## user\nzebra zebra zebra\n").unwrap();

        let mut conn = Connection::open_in_memory().unwrap();
        apply_schema(&conn).unwrap();
        reindex(&mut conn, tmp.path()).unwrap();

        let hits = search(&conn, "zebra").unwrap();
        assert_eq!(hits.len(), 1, "one hit per idea, not one per kind");
        assert_eq!(hits[0].kind, "conversation");
        // Strip the sentinel match-markers before the content check (they wrap each individual
        // "zebra" token, so the raw snippet is no longer one contiguous "zebra zebra" run).
        let plain: String = hits[0]
            .snippet
            .chars()
            .filter(|c| *c != SNIPPET_MATCH_OPEN && *c != SNIPPET_MATCH_CLOSE)
            .collect();
        assert!(
            plain.contains("zebra zebra"),
            "kept the best-ranked (conversation) snippet, got: {}",
            hits[0].snippet
        );
    }

    // Coverage: end-to-end (vault → reindex → search) proof that each newly-indexed surface is
    // actually reachable through search(), not just present in search_fts.

    #[test]
    fn search_finds_title_only_match() {
        let tmp = tempfile::tempdir().unwrap();
        write_fixture_idea(
            tmp.path(),
            "gizmo",
            "Voltarian Registry",
            &[],
            "Nothing special in the body.\n",
            10,
        );

        let mut conn = Connection::open_in_memory().unwrap();
        apply_schema(&conn).unwrap();
        reindex(&mut conn, tmp.path()).unwrap();

        let hits = search(&conn, "voltarian").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].slug, "gizmo");
        assert_eq!(hits[0].kind, "title");
    }

    #[test]
    fn search_finds_tag_only_match() {
        let tmp = tempfile::tempdir().unwrap();
        write_fixture_idea(
            tmp.path(),
            "gizmo",
            "Gizmo",
            &["thermovoric"],
            "Nothing special in the body.\n",
            10,
        );

        let mut conn = Connection::open_in_memory().unwrap();
        apply_schema(&conn).unwrap();
        reindex(&mut conn, tmp.path()).unwrap();

        let hits = search(&conn, "thermovoric").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].slug, "gizmo");
        assert_eq!(hits[0].kind, "tags");
    }

    #[test]
    fn search_finds_memory_fact_body_only_match() {
        let tmp = tempfile::tempdir().unwrap();
        write_fixture_idea(
            tmp.path(),
            "gizmo",
            "Gizmo",
            &[],
            "Nothing special in the body.\n",
            10,
        );
        store::write_memory_fact(
            tmp.path(),
            "gizmo",
            &MemoryFact {
                frontmatter: MemoryFactFrontmatter {
                    slug: "insight".into(),
                    title: "Insight".into(),
                    tags: vec![],
                    created: Utc.with_ymd_and_hms(2026, 7, 7, 11, 0, 0).unwrap(),
                    links: vec![],
                },
                body: "The plutonian variance was the deciding factor.\n".into(),
            },
        )
        .unwrap();

        let mut conn = Connection::open_in_memory().unwrap();
        apply_schema(&conn).unwrap();
        reindex(&mut conn, tmp.path()).unwrap();

        // "memory_facts" (the derived table) has no body column at all — this is the previously
        // impossible search: the durable fact text itself, not just its title.
        let hits = search(&conn, "plutonian").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].slug, "gizmo");
        assert_eq!(hits[0].kind, "memory");
    }

    // Ranking: field weighting, the backlink prior, and multi-kind corroboration. These insert
    // directly into `search_fts`/`backlinks` (like `insert_idea` above) rather than going through
    // the vault, specifically so the two rows under comparison have byte-identical `content` —
    // and therefore, since `idea_id`/`kind` are UNINDEXED, mathematically identical raw bm25 —
    // isolating the ranking adjustment under test from any bm25 noise.

    #[test]
    fn title_match_ranks_above_body_match_at_identical_bm25() {
        let conn = Connection::open_in_memory().unwrap();
        apply_schema(&conn).unwrap();

        insert_idea(&conn, "title-match", "T", "draft", "2026-07-07T10:00:00Z");
        let title_id = conn.last_insert_rowid();
        insert_idea(&conn, "body-match", "B", "draft", "2026-07-07T10:00:00Z");
        let body_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO search_fts (idea_id, kind, content) \
             VALUES (?1, 'title', 'zephyrion device')",
            rusqlite::params![title_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO search_fts (idea_id, kind, content) \
             VALUES (?1, 'idea_body', 'zephyrion device')",
            rusqlite::params![body_id],
        )
        .unwrap();

        let hits = search(&conn, "zephyrion").unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(
            hits[0].slug, "title-match",
            "a title hit must outrank an equally bm25-scored body hit"
        );
        assert_eq!(hits[0].kind, "title");
    }

    #[test]
    fn backlink_prior_breaks_a_near_tie() {
        let conn = Connection::open_in_memory().unwrap();
        apply_schema(&conn).unwrap();

        // "quiet" is inserted (and so scanned/rowid-ordered) first: absent the backlink boost,
        // the identical-bm25 tie-break would already favor it, so the assertion below can only
        // pass because of the boost, not by incidental insertion order.
        insert_idea(&conn, "quiet", "Quiet", "draft", "2026-07-07T10:00:00Z");
        let quiet_id = conn.last_insert_rowid();
        insert_idea(&conn, "popular", "Popular", "draft", "2026-07-07T10:00:00Z");
        let popular_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO search_fts (idea_id, kind, content) \
             VALUES (?1, 'idea_body', 'wombatron listing')",
            rusqlite::params![quiet_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO search_fts (idea_id, kind, content) \
             VALUES (?1, 'idea_body', 'wombatron listing')",
            rusqlite::params![popular_id],
        )
        .unwrap();

        for src in ["src-a", "src-b", "src-c"] {
            insert_idea(&conn, src, src, "draft", "2026-07-07T10:00:00Z");
            let src_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO backlinks (source_idea_id, target_slug, target_idea_id) \
                 VALUES (?1, 'popular', ?2)",
                rusqlite::params![src_id, popular_id],
            )
            .unwrap();
        }

        let hits = search(&conn, "wombatron").unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(
            hits[0].slug, "popular",
            "3 inbound backlinks must break an otherwise-tied bm25 match"
        );
    }

    #[test]
    fn multi_kind_corroboration_beats_single_kind_at_similar_bm25() {
        let conn = Connection::open_in_memory().unwrap();
        apply_schema(&conn).unwrap();

        // "single" inserted first for the same tie-break-direction reason as above.
        insert_idea(&conn, "single", "Single", "draft", "2026-07-07T10:00:00Z");
        let single_id = conn.last_insert_rowid();
        insert_idea(&conn, "multi", "Multi", "draft", "2026-07-07T10:00:00Z");
        let multi_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO search_fts (idea_id, kind, content) \
             VALUES (?1, 'idea_body', 'wombazzle notes')",
            rusqlite::params![single_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO search_fts (idea_id, kind, content) \
             VALUES (?1, 'idea_body', 'wombazzle notes')",
            rusqlite::params![multi_id],
        )
        .unwrap();
        // "multi" also matches via its conversation — a second, independently-weighted-the-same
        // (baseline 1.0) kind, so its best single-row bm25 is no better than "single"'s; only the
        // corroboration bonus can decide the order.
        conn.execute(
            "INSERT INTO search_fts (idea_id, kind, content) \
             VALUES (?1, 'conversation', 'wombazzle notes')",
            rusqlite::params![multi_id],
        )
        .unwrap();

        let hits = search(&conn, "wombazzle").unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(
            hits[0].slug, "multi",
            "matching in 2 distinct kinds must outrank matching in 1 at similar bm25"
        );
    }

    #[test]
    fn backlinks_both_directions_including_dangling() {
        let (_tmp, conn) = indexed_fixture();

        // Inbound: beta is linked from alpha's body; alpha from beta's fact frontmatter.
        assert_eq!(backlinks_for(&conn, "beta").unwrap(), ["alpha"]);
        assert_eq!(backlinks_for(&conn, "alpha").unwrap(), ["beta"]);
        // Inbound to a not-yet-created idea (forward ref) still answers.
        assert_eq!(backlinks_for(&conn, "ghost-idea").unwrap(), ["alpha"]);
        assert!(backlinks_for(&conn, "nobody").unwrap().is_empty());

        // Outbound: alpha links beta (resolved) and ghost-idea (dangling), in occurrence order.
        assert_eq!(
            links_from(&conn, "alpha").unwrap(),
            vec![
                LinkTarget {
                    target_slug: "beta".into(),
                    resolved: true
                },
                LinkTarget {
                    target_slug: "ghost-idea".into(),
                    resolved: false
                },
            ]
        );
    }

    #[test]
    fn ideas_with_tag_filters_and_orders() {
        let (_tmp, conn) = indexed_fixture();

        let risk: Vec<_> = ideas_with_tag(&conn, "risk")
            .unwrap()
            .into_iter()
            .map(|i| i.slug)
            .collect();
        assert_eq!(risk, ["beta", "alpha"]); // beta updated later → first

        let markets: Vec<_> = ideas_with_tag(&conn, "markets")
            .unwrap()
            .into_iter()
            .map(|i| i.slug)
            .collect();
        assert_eq!(markets, ["alpha"]);
        assert!(ideas_with_tag(&conn, "nope").unwrap().is_empty());
    }
}
