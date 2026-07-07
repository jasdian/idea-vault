//! Read queries over the derived index (docs/03-data-model.md §D6).
//!
//! These are pure reads of derived tables; they never mutate truth. If the index is stale a
//! reindex reconciles it (ADR-0002).

use rusqlite::Connection;

use super::IndexError;

/// One row of the idea list, projected for the vault overview UI.
#[derive(Debug, Clone)]
pub struct IdeaSummary {
    pub slug: String,
    pub title: String,
    pub state: String,
    pub updated_at: String,
}

/// A single full-text search result.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub slug: String,
    pub title: String,
    pub snippet: String,
}

/// List every indexed idea, most-recently-updated first.
pub fn list_ideas(conn: &Connection) -> Result<Vec<IdeaSummary>, IndexError> {
    let mut stmt =
        conn.prepare("SELECT slug, title, state, updated_at FROM ideas ORDER BY updated_at DESC")?;
    let rows = stmt.query_map([], |row| {
        Ok(IdeaSummary {
            slug: row.get(0)?,
            title: row.get(1)?,
            state: row.get(2)?,
            updated_at: row.get(3)?,
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

/// Full-text search over `search_fts` (idea bodies + conversations), joined back to `ideas` for
/// slug/title (R8). Results are best-match-first (bm25) and deduplicated to one hit per idea —
/// an idea matching in both its body and conversation appears once, with its best snippet. The
/// snippet is plain text (no highlight markup): the web layer escapes everything it renders, so
/// nothing here may depend on surviving as HTML.
pub fn search(conn: &Connection, query: &str) -> Result<Vec<SearchHit>, IndexError> {
    let Some(match_expr) = fts_query(query) else {
        return Ok(Vec::new());
    };

    let mut stmt = conn.prepare(
        "SELECT i.slug, i.title, snippet(search_fts, 2, '', '', '…', 12)
         FROM search_fts
         JOIN ideas i ON i.id = search_fts.idea_id
         WHERE search_fts MATCH ?1
         ORDER BY bm25(search_fts)
         LIMIT 100",
    )?;
    // The SQL limit is pre-dedup (an idea can contribute an idea_body row AND a conversation
    // row); dedup below then caps distinct ideas at 50, so duplicates can't starve the page.
    let rows = stmt.query_map([&match_expr], |row| {
        Ok(SearchHit {
            slug: row.get(0)?,
            title: row.get(1)?,
            snippet: row.get(2)?,
        })
    })?;

    let mut out: Vec<SearchHit> = Vec::new();
    for row in rows {
        let hit = row?;
        if !out.iter().any(|h| h.slug == hit.slug) {
            out.push(hit);
            if out.len() == 50 {
                break;
            }
        }
    }
    Ok(out)
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
        "SELECT i.slug, i.title, i.state, i.updated_at FROM ideas i
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
        assert!(
            hits[0].snippet.contains("zebra zebra"),
            "kept the best-ranked (conversation) snippet, got: {}",
            hits[0].snippet
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
