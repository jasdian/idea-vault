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

/// Full-text search over `search_fts`, joined back to `ideas` for slug/title.
pub fn search(_conn: &Connection, _query: &str) -> Result<Vec<SearchHit>, IndexError> {
    // TODO(search): see docs/03-data-model.md §D6 (search_fts) — FTS5 MATCH + snippet(), join
    // search_fts.idea_id -> ideas.id for slug/title.
    Err(IndexError::NotImplemented("index::queries::search"))
}

/// Slugs of ideas that link *to* `slug` via `[[slug]]` (resolved during reindex).
pub fn backlinks_for(_conn: &Connection, _slug: &str) -> Result<Vec<String>, IndexError> {
    // TODO(reindex): see docs/03-data-model.md §D6 (backlinks) — resolve backlinks.target_slug ==
    // slug, return distinct source idea slugs; target_idea_id may be NULL if unresolved.
    Err(IndexError::NotImplemented("index::queries::backlinks_for"))
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
}
