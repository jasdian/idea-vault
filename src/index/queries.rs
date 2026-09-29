//! Read queries over the derived index (docs/03-data-model.md §D6).
//!
//! These are pure reads of derived tables; they never mutate truth. If the index is stale a
//! reindex reconciles it (ADR-0002).

use std::collections::{BTreeMap, HashMap, HashSet};

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
/// inbound-`[[slug]]`-count prior, where a cross-idea `[[slug#fact]]` also counts, is plenty
/// at this corpus size). `log(1 + inbound)` so the first few backlinks matter far more than the hundredth (diminishing returns, not a popularity
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

/// One memory fact returned by [`vault_search`].
#[derive(Debug, Clone, PartialEq)]
pub struct FactHit {
    pub idea_slug: String,
    pub fact_slug: String,
    pub fact_title: String,
    /// Raw FTS5 `bm25()` of the fact row; negative, more negative is a better match.
    pub bm25: f64,
    /// `-bm25` plus the backlink prior of the fact's idea; higher is better.
    pub score: f64,
}

/// Fact-level retrieval over `kind = 'memory'` rows across every idea except `exclude_slug`.
///
/// `score = -bm25 + BACKLINK_BOOST * ln(1 + min(inbound, BACKLINK_CAP))`, where `inbound` is the
/// number of resolved backlinks targeting the fact's idea, the same prior [`search`] applies.
/// Results are ordered by `score` descending, then `idea_slug`, then `fact_slug`, and truncated
/// to `limit` after the excluded idea's rows have been removed in SQL. A blank or token-less
/// query yields an empty list. Each matching fact row is one hit; if two facts of one idea
/// share a slug, the hit carries the smaller of their titles.
///
/// `bm25` is computed over a connection-local copy of the `search_fts` rows of kind `title`,
/// `tags`, `idea_body` and `memory`: its IDF and average row length never include conversation
/// or artifact rows, so transcripts cannot move a fact's score. Every call rebuilds that
/// eligible-only copy, O(corpus) work per call, acceptable for an offline instrument.
///
/// This is an instrument for offline retrieval experiments. It is never registered as a model
/// tool: context reaches the model by push, not pull.
pub fn vault_search(
    conn: &Connection,
    query: &str,
    exclude_slug: Option<&str>,
    limit: usize,
) -> Result<Vec<FactHit>, IndexError> {
    let Some(match_expr) = fts_query(query) else {
        return Ok(Vec::new());
    };

    refresh_lexical_fts(conn)?;
    let mut stmt = conn.prepare(
        "SELECT i.slug, s.ref,
                (SELECT MIN(mf.title) FROM memory_facts mf
                 WHERE mf.idea_id = s.idea_id AND mf.slug = s.ref),
                bm25(lexical_fts),
                (SELECT COUNT(*) FROM backlinks bl WHERE bl.target_idea_id = i.id)
         FROM temp.lexical_fts s
         JOIN ideas i ON i.id = s.idea_id
         WHERE lexical_fts MATCH ?1
           AND s.kind = 'memory'
           AND (?2 IS NULL OR i.slug <> ?2)
           AND EXISTS (SELECT 1 FROM memory_facts mf
                       WHERE mf.idea_id = s.idea_id AND mf.slug = s.ref)",
    )?;
    let rows = stmt.query_map(rusqlite::params![&match_expr, exclude_slug], |row| {
        let bm25: f64 = row.get(3)?;
        let inbound: i64 = row.get(4)?;
        Ok(FactHit {
            idea_slug: row.get(0)?,
            fact_slug: row.get(1)?,
            fact_title: row.get(2)?,
            bm25,
            score: -bm25 + BACKLINK_BOOST * (1.0 + inbound.min(BACKLINK_CAP) as f64).ln(),
        })
    })?;
    let mut hits = rows.collect::<Result<Vec<_>, _>>()?;
    hits.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.idea_slug.cmp(&b.idea_slug))
            .then_with(|| a.fact_slug.cmp(&b.fact_slug))
    });
    hits.truncate(limit);
    Ok(hits)
}

/// One idea returned by [`lexical_baseline`].
#[derive(Debug, Clone, PartialEq)]
pub struct LexicalHit {
    pub idea_slug: String,
    /// `-bm25` of the idea's best-matching row; higher is better.
    pub score: f64,
}

const LEXICAL_KINDS: &str = "('title', 'tags', 'idea_body', 'memory')";
const LEXICAL_MAX_TOKENS: usize = 20;
const LEXICAL_MIN_TERM_CHARS: usize = 3;
const LEXICAL_FTS_DDL: &str = "DROP TABLE IF EXISTS temp.lexical_vocab;
     DROP TABLE IF EXISTS temp.lexical_fts;
     CREATE VIRTUAL TABLE temp.lexical_fts USING fts5(
         idea_id UNINDEXED, kind UNINDEXED, content, ref UNINDEXED);
     CREATE VIRTUAL TABLE temp.lexical_vocab USING fts5vocab(temp, lexical_fts, instance);";

// Rebuilds the connection-local `temp.lexical_fts` from the eligible rows of the current
// `search_fts` contents, same rowids, so every bm25 and vocab read that follows sees them.
fn refresh_lexical_fts(conn: &Connection) -> Result<(), IndexError> {
    conn.execute_batch(LEXICAL_FTS_DDL)?;
    conn.execute(
        &format!(
            "INSERT INTO temp.lexical_fts (rowid, idea_id, kind, content, ref)
             SELECT rowid, idea_id, kind, content, ref FROM main.search_fts
             WHERE kind IN {LEXICAL_KINDS}"
        ),
        [],
    )?;
    Ok(())
}

fn lexical_idea_id(conn: &Connection, slug: &str) -> Result<Option<i64>, IndexError> {
    match conn.query_row("SELECT id FROM ideas WHERE slug = ?1", [slug], |row| {
        row.get(0)
    }) {
        Ok(id) => Ok(Some(id)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn push_token(tokens: &mut Vec<String>, term: String) {
    if tokens.len() < LEXICAL_MAX_TOKENS && !tokens.contains(&term) {
        tokens.push(term);
    }
}

/// The query tokens [`lexical_baseline`] runs for the idea `slug`, at most 20, in this order:
///
/// 1. the idea's title words, then its tag words — the tokens FTS5 `unicode61` produced for its
///    `title` and `tags` rows (case-folded, split on non-alphanumerics), in first-occurrence
///    order, deduplicated;
/// 2. the idea's top TF-IDF terms by `tf * idf` descending, ties by term ascending, skipping
///    terms already taken.
///
/// Only `search_fts` rows of kind `title`, `tags`, `idea_body` and `memory` count, both as the
/// TF-IDF source and as the corpus; conversation and artifact rows are excluded. `tf` is the number of occurrences of a term in this idea's
/// eligible rows, `df` the number of distinct ideas whose eligible rows contain it, and
/// `idf = ln(N / df)` with `N` the number of ideas that have eligible rows. A term with
/// `idf = 0` (present in every idea), a pure-numeric term, or a term shorter than 3 characters is
/// never chosen by TF-IDF. Title and tag tokens count toward the 20 and are truncated too.
///
/// Term statistics come from an `fts5vocab` instance table over a copy of the eligible rows in
/// the connection's `temp` schema; the derived schema is untouched. Every call rebuilds that
/// eligible-only copy, O(corpus) work per call, acceptable for an offline instrument. An unknown
/// slug yields an empty list.
pub fn lexical_query_terms(conn: &Connection, slug: &str) -> Result<Vec<String>, IndexError> {
    let Some(idea_id) = lexical_idea_id(conn, slug)? else {
        return Ok(Vec::new());
    };
    Ok(LexicalCorpus::load(conn)?.query_terms(idea_id))
}

/// Term statistics of the `search_fts` rows of kind `title`, `tags`, `idea_body` and `memory`,
/// loaded once so a caller that needs [`lexical_query_terms`] for many ideas pays one scan.
/// Loading rebuilds the connection's `temp.lexical_fts` copy of those rows, which
/// [`LexicalCorpus::hits`] then scores against.
pub(crate) struct LexicalCorpus<'c> {
    conn: &'c Connection,
    ideas: i64,
    heads: BTreeMap<i64, Vec<(u8, i64, i64, String)>>,
    tf: BTreeMap<i64, BTreeMap<String, i64>>,
    df: BTreeMap<String, i64>,
}

impl<'c> LexicalCorpus<'c> {
    /// Rebuilds `temp.lexical_fts`, then one read of its `(rowid, idea, kind)` and one scan of
    /// its `fts5vocab` instance table, matched by rowid here rather than joined in SQL.
    pub(crate) fn load(conn: &'c Connection) -> Result<Self, IndexError> {
        refresh_lexical_fts(conn)?;
        let mut docs: HashMap<i64, (i64, Option<u8>)> = HashMap::new();
        let mut stmt = conn.prepare("SELECT rowid, idea_id, kind FROM temp.lexical_fts")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let head = match row.get::<_, String>(2)?.as_str() {
                "title" => Some(0),
                "tags" => Some(1),
                _ => None,
            };
            docs.insert(row.get(0)?, (row.get(1)?, head));
        }
        drop(rows);
        drop(stmt);
        let ideas = docs
            .values()
            .map(|(idea, _)| *idea)
            .collect::<HashSet<_>>()
            .len() as i64;

        let mut heads: BTreeMap<i64, Vec<(u8, i64, i64, String)>> = BTreeMap::new();
        let mut tf: BTreeMap<i64, BTreeMap<String, i64>> = BTreeMap::new();
        let mut stmt = conn.prepare("SELECT term, doc, offset FROM temp.lexical_vocab")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let doc: i64 = row.get(1)?;
            let Some(&(idea, head)) = docs.get(&doc) else {
                continue;
            };
            let term: String = row.get(0)?;
            if let Some(rank) = head {
                heads
                    .entry(idea)
                    .or_default()
                    .push((rank, doc, row.get(2)?, term.clone()));
            }
            *tf.entry(idea).or_default().entry(term).or_default() += 1;
        }
        for tokens in heads.values_mut() {
            tokens.sort();
        }
        let mut df: BTreeMap<String, i64> = BTreeMap::new();
        for terms in tf.values() {
            for term in terms.keys() {
                *df.entry(term.clone()).or_default() += 1;
            }
        }
        Ok(Self {
            conn,
            ideas,
            heads,
            tf,
            df,
        })
    }

    /// [`lexical_query_terms`] of the idea with id `idea_id`.
    pub(crate) fn query_terms(&self, idea_id: i64) -> Vec<String> {
        let mut tokens = Vec::new();
        for (_, _, _, term) in self.heads.get(&idea_id).into_iter().flatten() {
            push_token(&mut tokens, term.clone());
        }
        if tokens.len() == LEXICAL_MAX_TOKENS {
            return tokens;
        }

        let mut ranked: Vec<(f64, &str)> = Vec::new();
        for (term, &tf) in self.tf.get(&idea_id).into_iter().flatten() {
            let df = self.df[term];
            if df >= self.ideas
                || term.chars().count() < LEXICAL_MIN_TERM_CHARS
                || term.chars().all(char::is_numeric)
            {
                continue;
            }
            ranked.push((tf as f64 * (self.ideas as f64 / df as f64).ln(), term));
        }
        ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(b.1)));
        for (_, term) in ranked {
            if tokens.len() == LEXICAL_MAX_TOKENS {
                break;
            }
            push_token(&mut tokens, term.to_string());
        }
        tokens
    }

    /// [`lexical_baseline`] for the idea `slug` with its query `terms` already computed by
    /// [`LexicalCorpus::query_terms`], so a caller that also needs the terms runs one scan.
    pub(crate) fn hits(
        &self,
        slug: &str,
        terms: &[String],
        limit: usize,
    ) -> Result<Vec<LexicalHit>, IndexError> {
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let match_expr = terms
            .iter()
            .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" OR ");

        let mut stmt = self.conn.prepare(
            "SELECT i.slug, bm25(lexical_fts)
             FROM temp.lexical_fts s
             JOIN ideas i ON i.id = s.idea_id
             WHERE lexical_fts MATCH ?1
               AND i.slug <> ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![&match_expr, slug], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
        })?;
        let mut best: HashMap<String, f64> = HashMap::new();
        for row in rows {
            let (idea_slug, bm25) = row?;
            best.entry(idea_slug)
                .and_modify(|b| *b = b.min(bm25))
                .or_insert(bm25);
        }
        let mut hits: Vec<LexicalHit> = best
            .into_iter()
            .map(|(idea_slug, bm25)| LexicalHit {
                idea_slug,
                score: -bm25,
            })
            .collect();
        hits.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.idea_slug.cmp(&b.idea_slug))
        });
        hits.truncate(limit);
        Ok(hits)
    }

    /// Whether `term` occurs in the eligible rows of every idea that has any, so its IDF is 0.
    pub(crate) fn in_every_idea(&self, term: &str) -> bool {
        self.df.get(term).is_some_and(|&df| df >= self.ideas)
    }

    /// Whether `term` occurs in any eligible row of the idea with id `idea_id`.
    pub(crate) fn contains(&self, idea_id: i64, term: &str) -> bool {
        self.tf
            .get(&idea_id)
            .is_some_and(|terms| terms.contains_key(term))
    }
}

/// Fair lexical baseline retriever: the ideas most lexically similar to the idea `slug`.
///
/// The query is [`lexical_query_terms`] (title words, tag words, top TF-IDF terms; at most 20
/// tokens), each token quoted and OR-ed, run with FTS5 `bm25` against the `search_fts` rows of
/// kind `title`, `tags`, `idea_body` and `memory` of every idea except `slug`. The `bm25`
/// corpus (IDF, average row length) is exactly those eligible rows of every idea, `slug`'s own
/// included, copied into a connection-local table; conversation and artifact rows neither match
/// nor shape the statistics. Every call rebuilds that eligible-only copy, O(corpus) work per
/// call, acceptable for an offline instrument. The idea itself is excluded in SQL, so
/// `limit` applies to the other ideas only. An idea's `score` is `-bm25` of its best-matching
/// row. Results are ordered by `score` descending, then slug. An unknown slug, or an idea with no
/// query tokens, yields an empty list.
///
/// This is an instrument for offline retrieval experiments, not a full-body OR-query. It is
/// never registered as a model tool: context reaches the model by push, not pull.
pub fn lexical_baseline(
    conn: &Connection,
    slug: &str,
    limit: usize,
) -> Result<Vec<LexicalHit>, IndexError> {
    let Some(idea_id) = lexical_idea_id(conn, slug)? else {
        return Ok(Vec::new());
    };
    let corpus = LexicalCorpus::load(conn)?;
    let terms = corpus.query_terms(idea_id);
    corpus.hits(slug, &terms, limit)
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

/// One outbound fact-level link from an idea (D23): a `[[idea#fact]]` reference, or a bare
/// `[[fact]]` / frontmatter `links:` entry inside a memory fact that named a sibling fact.
#[derive(Debug, Clone, PartialEq)]
pub struct FactLink {
    /// The linking memory fact's slug; `None` when the link sits in the idea.md body.
    pub src_fact: Option<String>,
    pub dst_idea: String,
    pub dst_fact: String,
    /// Whether the last reindex found `dst_fact` inside `dst_idea` (`false` = dangling/forward
    /// `[[idea#fact]]`; bare links are only kept when they resolve).
    pub resolved: bool,
}

/// Outbound fact links of the idea `slug` — from its body and its memory facts — in reindex
/// insertion order, with resolution status.
pub fn fact_links_from(conn: &Connection, slug: &str) -> Result<Vec<FactLink>, IndexError> {
    let mut stmt = conn.prepare(
        "SELECT sf.slug, fl.dst_idea_slug, fl.dst_fact_slug, fl.dst_fact_id IS NOT NULL
         FROM fact_links fl
         JOIN ideas s ON s.id = fl.src_idea_id
         LEFT JOIN memory_facts sf ON sf.id = fl.src_fact_id
         WHERE s.slug = ?1
         ORDER BY fl.id",
    )?;
    let rows = stmt.query_map([slug], |row| {
        Ok(FactLink {
            src_fact: row.get(0)?,
            dst_idea: row.get(1)?,
            dst_fact: row.get(2)?,
            resolved: row.get(3)?,
        })
    })?;
    rows.collect::<Result<_, _>>().map_err(Into::into)
}

/// One idea related to a queried idea through the derived `edges` graph (D6).
#[derive(Debug, Clone, PartialEq)]
pub struct RelatedIdea {
    pub slug: String,
    pub title: String,
    /// A pair of ideas weighs the sum of its edges' weights across all edge types. Direct
    /// neighbours score their pair weight with the queried idea; two-hop candidates score 0.5
    /// times their best path, where a path weighs as much as its weakest pair.
    pub score: f64,
    /// Shortest edge distance from the queried idea: 1 or 2.
    pub hops: u32,
    /// Sorted, distinct explanations of the pairs that reached this idea at `hops`: one
    /// `type: detail` entry per edge type, `; `-joined, and two-hop reasons read
    /// `via <slug> (<pair reason>)`.
    pub reasons: Vec<String>,
}

const REASON_SEPARATOR: char = '\u{1f}';

/// Ideas related to `slug` through `edges`, best first, at most `limit`.
///
/// A recursive CTE walks the undirected edges up to two hops out from `slug`. Each candidate is
/// scored at its minimal hop count only (see [`RelatedIdea::score`]): a direct neighbour's
/// two-hop paths never add to its score. The queried idea is never returned, including through a
/// cycle back to itself. Ordered by score descending, hops ascending, slug ascending. An unknown
/// `slug` yields an empty list.
pub fn related_ideas(
    conn: &Connection,
    slug: &str,
    limit: usize,
) -> Result<Vec<RelatedIdea>, IndexError> {
    let mut stmt = conn.prepare(
        "WITH RECURSIVE
         start(id) AS (SELECT id FROM ideas WHERE slug = ?1),
         undirected(a, b, type, weight, detail) AS (
             SELECT src_idea_id, dst_idea_id, type, weight, detail FROM edges
             UNION ALL
             SELECT dst_idea_id, src_idea_id, type, weight, detail FROM edges
         ),
         pair(a, b, weight, reason) AS (
             SELECT a, b, SUM(weight), GROUP_CONCAT(type || ': ' || detail, '; ' ORDER BY type)
             FROM undirected GROUP BY a, b
         ),
         walk(node, hops, path_weight, reason) AS (
             SELECT p.b, 1, p.weight, p.reason
             FROM pair p JOIN start ON p.a = start.id
             WHERE p.b <> start.id
             UNION ALL
             SELECT p.b, w.hops + 1, MIN(w.path_weight, p.weight),
                    'via ' || (SELECT slug FROM ideas WHERE id = w.node) || ' (' || p.reason || ')'
             FROM walk w JOIN pair p ON p.a = w.node
             WHERE w.hops < 2 AND p.b <> (SELECT id FROM start)
         ),
         nearest(node, hops) AS (SELECT node, MIN(hops) FROM walk GROUP BY node),
         reasons(node, reason) AS (
             SELECT DISTINCT w.node, w.reason
             FROM walk w JOIN nearest n ON n.node = w.node AND n.hops = w.hops
         )
         SELECT i.slug, i.title, n.hops,
                (SELECT CASE n.hops WHEN 1 THEN MAX(w.path_weight)
                                    ELSE 0.5 * MAX(w.path_weight) END
                 FROM walk w WHERE w.node = n.node AND w.hops = n.hops) AS score,
                (SELECT GROUP_CONCAT(r.reason, char(31)) FROM reasons r WHERE r.node = n.node)
         FROM nearest n JOIN ideas i ON i.id = n.node
         ORDER BY score DESC, n.hops ASC, i.slug ASC
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(rusqlite::params![slug, limit as i64], |row| {
        let joined: String = row.get(4)?;
        let mut reasons: Vec<String> = joined.split(REASON_SEPARATOR).map(str::to_string).collect();
        reasons.sort();
        Ok(RelatedIdea {
            slug: row.get(0)?,
            title: row.get(1)?,
            hops: row.get(2)?,
            score: row.get(3)?,
            reasons,
        })
    })?;
    rows.collect::<Result<_, _>>().map_err(Into::into)
}

/// Two distinct tag names that [`crate::domain::tag::near_duplicate`] judges to be drift of one
/// another, with the slugs of the ideas carrying each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagNearDuplicate {
    pub a: String,
    pub b: String,
    pub a_ideas: Vec<String>,
    pub b_ideas: Vec<String>,
}

/// Every tag name with the slugs of the ideas carrying it (sorted), ordered by name. A tag no idea
/// carries appears with an empty list. One flat read, so a caller can drop the connection before
/// comparing names.
pub fn tag_carriers(conn: &Connection) -> Result<Vec<(String, Vec<String>)>, IndexError> {
    let mut stmt = conn.prepare(
        "SELECT t.name, i.slug FROM tags t
         LEFT JOIN idea_tags it ON it.tag_id = t.id
         LEFT JOIN ideas i ON i.id = it.idea_id
         ORDER BY t.name, i.slug",
    )?;
    let mut carriers: Vec<(String, Vec<String>)> = Vec::new();
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(0)?;
        let slug: Option<String> = row.get(1)?;
        if carriers.last().map(|(n, _)| n != &name).unwrap_or(true) {
            carriers.push((name, Vec::new()));
        }
        if let (Some(slug), Some((_, ideas))) = (slug, carriers.last_mut()) {
            ideas.push(slug);
        }
    }
    Ok(carriers)
}

/// Every pair of tag names in `tags` that look like drift of one another, ordered by `(a, b)` with
/// `a < b`. A read-side report only: the edges derivation matches tag names exactly and never
/// merges these.
pub fn tag_near_duplicates(conn: &Connection) -> Result<Vec<TagNearDuplicate>, IndexError> {
    let carriers = tag_carriers(conn)?;
    let mut report = Vec::new();
    for (i, (a, a_ideas)) in carriers.iter().enumerate() {
        for (b, b_ideas) in &carriers[i + 1..] {
            if crate::domain::tag::near_duplicate(a, b) {
                report.push(TagNearDuplicate {
                    a: a.clone(),
                    b: b.clone(),
                    a_ideas: a_ideas.clone(),
                    b_ideas: b_ideas.clone(),
                });
            }
        }
    }
    Ok(report)
}

/// The pairs [`tag_near_duplicates`] would report that involve at least one of `own_tags`, in the
/// same `(a, b)` order, computed from `carriers` (as returned by [`tag_carriers`], sorted by name)
/// by comparing only the own tags against every tag: O(own * tags) rather than O(tags^2).
pub fn own_tag_near_duplicates(
    own_tags: &[String],
    carriers: &[(String, Vec<String>)],
) -> Vec<TagNearDuplicate> {
    let mut pairs: std::collections::BTreeMap<(usize, usize), TagNearDuplicate> =
        std::collections::BTreeMap::new();
    for (own_idx, (own, _)) in carriers
        .iter()
        .enumerate()
        .filter(|(_, (name, _))| own_tags.contains(name))
    {
        for (other_idx, (other, _)) in carriers.iter().enumerate() {
            if other_idx == own_idx || !crate::domain::tag::near_duplicate(own, other) {
                continue;
            }
            let (lo, hi) = (own_idx.min(other_idx), own_idx.max(other_idx));
            pairs.entry((lo, hi)).or_insert_with(|| TagNearDuplicate {
                a: carriers[lo].0.clone(),
                b: carriers[hi].0.clone(),
                a_ideas: carriers[lo].1.clone(),
                b_ideas: carriers[hi].1.clone(),
            });
        }
    }
    pairs.into_values().collect()
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
                    sources: vec![],
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

    fn insert_fact(conn: &Connection, idea_id: i64, slug: &str, title: &str, body: &str) {
        conn.execute(
            "INSERT INTO memory_facts (idea_id, slug, title, created_at) \
             VALUES (?1, ?2, ?3, '2026-07-07T10:00:00Z')",
            rusqlite::params![idea_id, slug, title],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO search_fts (idea_id, kind, ref, content) \
             VALUES (?1, 'memory', ?2, ?3)",
            rusqlite::params![idea_id, slug, format!("{title}\n\n{body}")],
        )
        .unwrap();
    }

    fn fact_fixture_idea(conn: &Connection, slug: &str) -> i64 {
        insert_idea(conn, slug, slug, "draft", "2026-07-07T10:00:00Z");
        conn.last_insert_rowid()
    }

    #[test]
    fn vault_search_returns_fact_level_hits_with_slugs() {
        let tmp = tempfile::tempdir().unwrap();
        write_fixture_idea(tmp.path(), "alpha", "Alpha", &[], "Alpha statement.\n", 10);
        write_fixture_idea(tmp.path(), "beta", "Beta", &[], "Beta statement.\n", 11);
        for (idea, slug, title, body) in [
            (
                "beta",
                "churn-cliff",
                "Churn cliff",
                "Zorbicon retention drops.\n",
            ),
            ("beta", "unrelated", "Unrelated", "Nothing to see.\n"),
        ] {
            store::write_memory_fact(
                tmp.path(),
                idea,
                &MemoryFact {
                    frontmatter: MemoryFactFrontmatter {
                        slug: slug.into(),
                        title: title.into(),
                        tags: vec![],
                        created: Utc.with_ymd_and_hms(2026, 7, 7, 12, 0, 0).unwrap(),
                        links: vec![],
                    },
                    body: body.into(),
                },
            )
            .unwrap();
        }
        let mut conn = Connection::open_in_memory().unwrap();
        apply_schema(&conn).unwrap();
        reindex(&mut conn, tmp.path()).unwrap();

        let hits = vault_search(&conn, "zorbicon", Some("alpha"), 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].idea_slug, "beta");
        assert_eq!(hits[0].fact_slug, "churn-cliff");
        assert_eq!(hits[0].fact_title, "Churn cliff");
        assert!(hits[0].bm25 < 0.0);
        assert_eq!(hits[0].score, -hits[0].bm25);
    }

    #[test]
    fn vault_search_excludes_the_querying_idea() {
        let conn = Connection::open_in_memory().unwrap();
        apply_schema(&conn).unwrap();
        let a = fact_fixture_idea(&conn, "idea-a");
        let b = fact_fixture_idea(&conn, "idea-b");
        insert_fact(&conn, a, "fact-a", "Fact A", "quorblex appears here");
        insert_fact(&conn, b, "fact-b", "Fact B", "quorblex appears here too");

        let hits = vault_search(&conn, "quorblex", Some("idea-a"), 10).unwrap();
        let got: Vec<_> = hits
            .iter()
            .map(|h| (h.idea_slug.as_str(), h.fact_slug.as_str()))
            .collect();
        assert_eq!(got, [("idea-b", "fact-b")]);

        let all = vault_search(&conn, "quorblex", None, 10).unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn vault_search_limit_applies_after_exclusion() {
        let conn = Connection::open_in_memory().unwrap();
        apply_schema(&conn).unwrap();
        let a = fact_fixture_idea(&conn, "idea-a");
        let b = fact_fixture_idea(&conn, "idea-b");
        for n in 0..3 {
            insert_fact(
                &conn,
                a,
                &format!("a-{n}"),
                "Dense",
                "plinthar plinthar plinthar",
            );
        }
        insert_fact(
            &conn,
            b,
            "b-0",
            "Sparse",
            "plinthar among many other filler words here",
        );
        insert_fact(
            &conn,
            b,
            "b-1",
            "Sparse two",
            "plinthar among many other filler words there",
        );

        let hits = vault_search(&conn, "plinthar", Some("idea-a"), 2).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.idea_slug == "idea-b"));
    }

    #[test]
    fn vault_search_backlink_prior_breaks_ties() {
        let conn = Connection::open_in_memory().unwrap();
        apply_schema(&conn).unwrap();
        let quiet = fact_fixture_idea(&conn, "aaa-quiet");
        let popular = fact_fixture_idea(&conn, "zzz-popular");
        insert_fact(&conn, quiet, "f", "Same", "wombatron listing");
        insert_fact(&conn, popular, "f", "Same", "wombatron listing");
        for src in ["src-a", "src-b"] {
            let src_id = fact_fixture_idea(&conn, src);
            conn.execute(
                "INSERT INTO backlinks (source_idea_id, target_slug, target_idea_id) \
                 VALUES (?1, 'zzz-popular', ?2)",
                rusqlite::params![src_id, popular],
            )
            .unwrap();
        }

        let hits = vault_search(&conn, "wombatron", None, 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].idea_slug, "zzz-popular");
        assert_eq!(hits[0].bm25, hits[1].bm25);
        let expected = BACKLINK_BOOST * 3.0_f64.ln();
        assert!((hits[0].score - hits[1].score - expected).abs() < 1e-12);
    }

    #[test]
    fn vault_search_returns_one_hit_per_matching_fact_row_when_slugs_collide() {
        let conn = Connection::open_in_memory().unwrap();
        apply_schema(&conn).unwrap();
        let a = fact_fixture_idea(&conn, "idea-a");
        insert_fact(&conn, a, "dup", "First", "grallomir appears here");
        insert_fact(&conn, a, "dup", "Second", "nothing relevant");

        let hits = vault_search(&conn, "grallomir", None, 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].fact_slug, "dup");
    }

    #[test]
    fn vault_search_blank_query_is_empty() {
        let conn = Connection::open_in_memory().unwrap();
        apply_schema(&conn).unwrap();
        let a = fact_fixture_idea(&conn, "idea-a");
        insert_fact(&conn, a, "f", "Fact", "anything at all");

        assert!(vault_search(&conn, "", None, 5).unwrap().is_empty());
        assert!(vault_search(&conn, "   \t\n", None, 5).unwrap().is_empty());
        assert!(vault_search(&conn, "\0", None, 5).unwrap().is_empty());
    }

    fn reindexed(vault: &Path) -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_schema(&conn).unwrap();
        reindex(&mut conn, vault).unwrap();
        conn
    }

    fn idea_id(conn: &Connection, slug: &str) -> i64 {
        conn.query_row("SELECT id FROM ideas WHERE slug = ?1", [slug], |row| {
            row.get(0)
        })
        .unwrap()
    }

    fn hit_slugs(hits: &[LexicalHit]) -> Vec<&str> {
        hits.iter().map(|h| h.idea_slug.as_str()).collect()
    }

    #[test]
    fn lexical_baseline_terms_are_capped_title_and_tags_first_and_deterministic() {
        let tmp = tempfile::tempdir().unwrap();
        let words: Vec<String> = (0..30u8)
            .map(|i| format!("qz{}{}", (b'a' + i / 5) as char, (b'a' + i % 5) as char))
            .collect();
        let mut body: Vec<String> = words.iter().rev().cloned().collect();
        body.extend(std::iter::repeat_n("brimstoke".to_string(), 5));
        write_fixture_idea(
            tmp.path(),
            "harbor",
            "Glass Harbor",
            &["energy-grid", "tidal"],
            &format!("{}\n", body.join(" ")),
            10,
        );
        write_fixture_idea(
            tmp.path(),
            "other",
            "Other",
            &[],
            "Unrelated filler text.\n",
            11,
        );
        let conn = reindexed(tmp.path());

        let terms = lexical_query_terms(&conn, "harbor").unwrap();
        assert_eq!(terms.len(), 20, "capped at 20 tokens: {terms:?}");
        assert_eq!(terms[..5], ["glass", "harbor", "energy", "grid", "tidal"]);
        assert_eq!(
            terms[5], "brimstoke",
            "highest tf·idf term follows title and tags"
        );
        let mut tied = words.clone();
        tied.sort();
        assert_eq!(
            terms[6..],
            tied[..14],
            "equal tf·idf ties break by term ascending"
        );
        assert_eq!(terms, lexical_query_terms(&conn, "harbor").unwrap());
    }

    #[test]
    fn lexical_baseline_skips_zero_idf_terms_and_picks_rare_repeated_word() {
        let tmp = tempfile::tempdir().unwrap();
        write_fixture_idea(
            tmp.path(),
            "alpha",
            "Alpha",
            &[],
            "ubiquitous ubiquitous ubiquitous ubiquitous flonkery flonkery\n",
            10,
        );
        write_fixture_idea(tmp.path(), "beta", "Beta", &[], "ubiquitous here\n", 11);
        write_fixture_idea(tmp.path(), "gamma", "Gamma", &[], "ubiquitous there\n", 12);
        let conn = reindexed(tmp.path());

        let terms = lexical_query_terms(&conn, "alpha").unwrap();
        assert!(terms.contains(&"flonkery".to_string()), "{terms:?}");
        assert!(!terms.contains(&"ubiquitous".to_string()), "{terms:?}");
    }

    #[test]
    fn lexical_baseline_skips_short_and_numeric_terms() {
        let tmp = tempfile::tempdir().unwrap();
        write_fixture_idea(
            tmp.path(),
            "alpha",
            "Alpha",
            &[],
            "zq zq zq 2026 2026 2026 marrowind\n",
            10,
        );
        write_fixture_idea(tmp.path(), "beta", "Beta", &[], "Beta statement.\n", 11);
        let conn = reindexed(tmp.path());

        let terms = lexical_query_terms(&conn, "alpha").unwrap();
        assert_eq!(terms, ["alpha", "marrowind"]);
    }

    #[test]
    fn lexical_baseline_excludes_own_slug_and_limits_after_exclusion() {
        let tmp = tempfile::tempdir().unwrap();
        write_fixture_idea(
            tmp.path(),
            "idea-a",
            "Anchor",
            &[],
            "quorvex quorvex quorvex\n",
            10,
        );
        for (hour, slug) in [(11, "idea-b"), (12, "idea-c"), (13, "idea-d")] {
            write_fixture_idea(
                tmp.path(),
                slug,
                slug,
                &[],
                "quorvex among many other filler words\n",
                hour,
            );
        }
        write_fixture_idea(tmp.path(), "idea-e", "idea-e", &[], "unrelated\n", 14);
        let conn = reindexed(tmp.path());

        let two = lexical_baseline(&conn, "idea-a", 2).unwrap();
        assert_eq!(two.len(), 2, "{two:?}");
        assert!(two.iter().all(|h| h.idea_slug != "idea-a"), "{two:?}");

        let three = lexical_baseline(&conn, "idea-a", 3).unwrap();
        assert_eq!(hit_slugs(&three), ["idea-b", "idea-c", "idea-d"]);
        assert!(three.iter().all(|h| h.score > 0.0));
    }

    #[test]
    fn lexical_baseline_ignores_conversation_and_artifact_matches() {
        let tmp = tempfile::tempdir().unwrap();
        write_fixture_idea(
            tmp.path(),
            "alpha",
            "Alpha",
            &[],
            "snorkelpin snorkelpin\n",
            10,
        );
        store::append_conversation(tmp.path(), "alpha", "## user\nblorptang blorptang\n").unwrap();
        write_fixture_idea(tmp.path(), "conv", "Conv", &[], "Nothing here.\n", 11);
        store::append_conversation(tmp.path(), "conv", "## user\nsnorkelpin talk\n").unwrap();
        write_fixture_idea(tmp.path(), "arti", "Arti", &[], "Nothing there.\n", 12);
        write_fixture_idea(tmp.path(), "body", "Body", &[], "snorkelpin body\n", 13);
        let conn = reindexed(tmp.path());
        for (slug, content) in [
            ("arti", "snorkelpin artifact"),
            ("alpha", "glimmerax glimmerax glimmerax"),
        ] {
            conn.execute(
                "INSERT INTO search_fts (idea_id, kind, ref, content) \
                 VALUES (?1, 'artifact', 'run', ?2)",
                rusqlite::params![idea_id(&conn, slug), content],
            )
            .unwrap();
        }

        let terms = lexical_query_terms(&conn, "alpha").unwrap();
        assert!(terms.contains(&"snorkelpin".to_string()), "{terms:?}");
        assert!(!terms.contains(&"blorptang".to_string()), "{terms:?}");
        assert!(!terms.contains(&"glimmerax".to_string()), "{terms:?}");

        let hits = lexical_baseline(&conn, "alpha", 10).unwrap();
        assert_eq!(hit_slugs(&hits), ["body"]);
    }

    #[test]
    fn lexical_baseline_ranks_rare_term_sharer_above_common_term_sharer() {
        let tmp = tempfile::tempdir().unwrap();
        write_fixture_idea(
            tmp.path(),
            "anchor",
            "Anchor",
            &[],
            "zintharo zintharo widget\n",
            10,
        );
        write_fixture_idea(tmp.path(), "rare", "Rare", &[], "zintharo notes here\n", 11);
        write_fixture_idea(
            tmp.path(),
            "common",
            "Common",
            &[],
            "widget notes here\n",
            12,
        );
        for (hour, slug) in [(13, "x-one"), (14, "x-two"), (15, "x-three")] {
            write_fixture_idea(tmp.path(), slug, slug, &[], "widget filler\n", hour);
        }
        write_fixture_idea(tmp.path(), "bystander", "Bystander", &[], "unrelated\n", 16);
        let conn = reindexed(tmp.path());

        let terms = lexical_query_terms(&conn, "anchor").unwrap();
        assert!(terms.contains(&"widget".to_string()), "{terms:?}");
        let hits = lexical_baseline(&conn, "anchor", 10).unwrap();
        let slugs = hit_slugs(&hits);
        let rare = slugs.iter().position(|s| *s == "rare").unwrap();
        let common = slugs.iter().position(|s| *s == "common").unwrap();
        assert!(rare < common, "{hits:?}");
        assert!(!slugs.contains(&"bystander"), "{hits:?}");
    }

    fn write_fact(vault: &Path, idea: &str, slug: &str, body: &str) {
        store::write_memory_fact(
            vault,
            idea,
            &MemoryFact {
                frontmatter: MemoryFactFrontmatter {
                    slug: slug.into(),
                    title: slug.into(),
                    tags: vec![],
                    created: Utc.with_ymd_and_hms(2026, 7, 7, 12, 0, 0).unwrap(),
                    links: vec![],
                },
                body: body.into(),
            },
        )
        .unwrap();
    }

    #[test]
    fn lexical_baseline_df_counts_ideas_not_rows() {
        let tmp = tempfile::tempdir().unwrap();
        write_fixture_idea(tmp.path(), "alpha", "Alpha", &[], "quillane plans\n", 10);
        write_fixture_idea(tmp.path(), "beta", "Beta", &[], "quillane notes\n", 11);
        write_fixture_idea(tmp.path(), "gamma", "Gamma", &[], "unrelated\n", 12);
        write_fact(tmp.path(), "alpha", "fact-one", "quillane again\n");
        let conn = reindexed(tmp.path());

        let terms = lexical_query_terms(&conn, "alpha").unwrap();
        assert!(terms.contains(&"quillane".to_string()), "{terms:?}");
    }

    #[test]
    fn lexical_baseline_reads_memory_facts_as_source_and_pool() {
        let tmp = tempfile::tempdir().unwrap();
        write_fixture_idea(tmp.path(), "alpha", "Alpha", &[], "plain body\n", 10);
        write_fixture_idea(tmp.path(), "beta", "Beta", &[], "other body\n", 11);
        write_fixture_idea(tmp.path(), "gamma", "Gamma", &[], "third body\n", 12);
        write_fact(tmp.path(), "alpha", "fact-a", "sprockelt mechanism\n");
        write_fact(tmp.path(), "beta", "fact-b", "sprockelt elsewhere\n");
        let conn = reindexed(tmp.path());

        let terms = lexical_query_terms(&conn, "alpha").unwrap();
        assert!(terms.contains(&"sprockelt".to_string()), "{terms:?}");
        let hits = lexical_baseline(&conn, "alpha", 5).unwrap();
        assert_eq!(hit_slugs(&hits).first(), Some(&"beta"), "{hits:?}");
    }

    #[test]
    fn lexical_baseline_truncates_long_titles_and_dedupes_title_words() {
        let tmp = tempfile::tempdir().unwrap();
        let title_words: Vec<String> = (0..22u8)
            .map(|i| format!("tw{}{}", (b'a' + i / 5) as char, (b'a' + i % 5) as char))
            .collect();
        write_fixture_idea(
            tmp.path(),
            "long",
            &title_words.join(" "),
            &[],
            "twaa twaa twaa rarevox rarevox\n",
            10,
        );
        write_fixture_idea(tmp.path(), "other", "Other", &[], "filler\n", 11);
        write_fixture_idea(
            tmp.path(),
            "short",
            "Tidewrack",
            &[],
            "tidewrack tidewrack mirelune\n",
            12,
        );
        let conn = reindexed(tmp.path());

        let terms = lexical_query_terms(&conn, "long").unwrap();
        assert_eq!(terms, title_words[..20]);
        let terms = lexical_query_terms(&conn, "short").unwrap();
        assert_eq!(terms, ["tidewrack", "mirelune"]);
    }

    #[test]
    fn lexical_baseline_unknown_slug_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        write_fixture_idea(tmp.path(), "alpha", "Alpha", &[], "Alpha statement.\n", 10);
        let conn = reindexed(tmp.path());

        assert!(lexical_query_terms(&conn, "nobody").unwrap().is_empty());
        assert!(lexical_baseline(&conn, "nobody", 5).unwrap().is_empty());
    }

    fn score_bits(hits: &[LexicalHit]) -> Vec<(String, u64)> {
        hits.iter()
            .map(|h| (h.idea_slug.clone(), h.score.to_bits()))
            .collect()
    }

    fn fact_bits(hits: &[FactHit]) -> Vec<(String, String, u64, u64)> {
        hits.iter()
            .map(|h| {
                (
                    h.idea_slug.clone(),
                    h.fact_slug.clone(),
                    h.bm25.to_bits(),
                    h.score.to_bits(),
                )
            })
            .collect()
    }

    fn long_conversation(words: &str) -> String {
        let mut transcript = String::new();
        for n in 0..40 {
            transcript.push_str(&format!(
                "## user\nturn {n}: {words} and a lot of ordinary chatter besides\n\n"
            ));
        }
        transcript
    }

    #[test]
    fn lexical_baseline_bm25_ignores_conversation_rows() {
        let tmp = tempfile::tempdir().unwrap();
        write_fixture_idea(
            tmp.path(),
            "anchor",
            "Anchor",
            &[],
            "velmarch quintessor velmarch plans\n",
            10,
        );
        write_fixture_idea(tmp.path(), "near", "Near", &[], "velmarch notes here\n", 11);
        write_fixture_idea(
            tmp.path(),
            "far",
            "Far",
            &[],
            "quintessor among many other filler words\n",
            12,
        );
        write_fixture_idea(tmp.path(), "talker", "Talker", &[], "unrelated\n", 13);
        let mut conn = reindexed(tmp.path());
        let before = lexical_baseline(&conn, "anchor", 10).unwrap();
        assert_eq!(hit_slugs(&before), ["near", "far"], "{before:?}");

        store::append_conversation(
            tmp.path(),
            "talker",
            &long_conversation("velmarch quintessor velmarch"),
        )
        .unwrap();
        reindex(&mut conn, tmp.path()).unwrap();
        let after = lexical_baseline(&conn, "anchor", 10).unwrap();

        assert_eq!(
            score_bits(&after),
            score_bits(&before),
            "conversation rows moved eligible bm25: before {before:?} after {after:?}"
        );
    }

    #[test]
    fn vault_search_bm25_ignores_conversation_rows() {
        let tmp = tempfile::tempdir().unwrap();
        write_fixture_idea(tmp.path(), "asker", "Asker", &[], "Asker statement.\n", 10);
        write_fixture_idea(tmp.path(), "beta", "Beta", &[], "Beta statement.\n", 11);
        write_fixture_idea(tmp.path(), "gamma", "Gamma", &[], "Gamma statement.\n", 12);
        write_fixture_idea(tmp.path(), "talker", "Talker", &[], "unrelated\n", 13);
        write_fact(tmp.path(), "beta", "churn", "zorbicon retention drops\n");
        write_fact(
            tmp.path(),
            "gamma",
            "pricing",
            "zorbicon pricing among many other filler words\n",
        );
        let mut conn = reindexed(tmp.path());
        let before = vault_search(&conn, "zorbicon", Some("asker"), 10).unwrap();
        assert_eq!(before.len(), 2, "{before:?}");

        store::append_conversation(
            tmp.path(),
            "talker",
            &long_conversation("zorbicon retention zorbicon"),
        )
        .unwrap();
        reindex(&mut conn, tmp.path()).unwrap();
        let after = vault_search(&conn, "zorbicon", Some("asker"), 10).unwrap();

        assert_eq!(
            fact_bits(&after),
            fact_bits(&before),
            "conversation rows moved fact bm25: before {before:?} after {after:?}"
        );
    }

    #[test]
    fn lexical_fts_refreshes_after_reindex() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = tmp.path().join("vault");
        std::fs::create_dir_all(&vault).unwrap();
        let db = tmp.path().join("index.db");
        write_fixture_idea(
            &vault,
            "anchor",
            "Anchor",
            &[],
            "brallowick tessermont brallowick\n",
            10,
        );
        write_fixture_idea(&vault, "bystander", "Bystander", &[], "unrelated\n", 11);
        write_fixture_idea(&vault, "other", "Other", &[], "filler text\n", 12);
        let mut writer = crate::index::schema::open_or_create(&db).unwrap();
        reindex(&mut writer, &vault).unwrap();
        let reader = crate::index::schema::open_or_create(&db).unwrap();

        let facts = vault_search(&reader, "quillomar", Some("anchor"), 10).unwrap();
        assert!(facts.is_empty(), "{facts:?}");
        let hits = lexical_baseline(&reader, "anchor", 10).unwrap();
        assert!(hits.is_empty(), "{hits:?}");

        write_fact(&vault, "bystander", "echo", "quillomar echo\n");
        reindex(&mut writer, &vault).unwrap();
        let facts = vault_search(&reader, "quillomar", Some("anchor"), 10).unwrap();
        let got: Vec<_> = facts
            .iter()
            .map(|f| (f.idea_slug.as_str(), f.fact_slug.as_str()))
            .collect();
        assert_eq!(
            got,
            [("bystander", "echo")],
            "vault_search must see a fact another connection indexed: {facts:?}"
        );

        write_fixture_idea(
            &vault,
            "newcomer",
            "Newcomer",
            &[],
            "brallowick tessermont notes\n",
            13,
        );
        reindex(&mut writer, &vault).unwrap();
        let hits = lexical_baseline(&reader, "anchor", 10).unwrap();
        assert_eq!(
            hit_slugs(&hits),
            ["newcomer"],
            "lexical_baseline must see an idea another connection indexed: {hits:?}"
        );
    }

    #[test]
    fn lexical_baseline_bm25_ignores_artifact_rows() {
        let tmp = tempfile::tempdir().unwrap();
        write_fixture_idea(
            tmp.path(),
            "anchor",
            "Anchor",
            &[],
            "velmarch quintessor velmarch plans\n",
            10,
        );
        write_fixture_idea(tmp.path(), "near", "Near", &[], "velmarch notes here\n", 11);
        write_fixture_idea(
            tmp.path(),
            "far",
            "Far",
            &[],
            "quintessor among many other filler words\n",
            12,
        );
        write_fixture_idea(tmp.path(), "arti", "Arti", &[], "Nothing there.\n", 13);
        let conn = reindexed(tmp.path());
        let before = lexical_baseline(&conn, "anchor", 10).unwrap();
        assert_eq!(hit_slugs(&before), ["near", "far"], "{before:?}");

        for n in 0..40 {
            conn.execute(
                "INSERT INTO search_fts (idea_id, kind, ref, content) \
                 VALUES (?1, 'artifact', ?2, ?3)",
                rusqlite::params![
                    idea_id(&conn, "arti"),
                    format!("run-{n}"),
                    "velmarch quintessor velmarch and a lot of ordinary chatter besides",
                ],
            )
            .unwrap();
        }
        let after = lexical_baseline(&conn, "anchor", 10).unwrap();

        assert_eq!(
            score_bits(&after),
            score_bits(&before),
            "artifact rows moved eligible bm25: before {before:?} after {after:?}"
        );
    }
}
