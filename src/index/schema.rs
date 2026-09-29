//! SQLite schema for the derived index (docs/03-data-model.md §D6).
//!
//! Every table here is **derived** and rebuilt by [`crate::index::reindex`]. There are no
//! migrations: because markdown is the source of truth (ADR-0002), a schema change bumps
//! [`SCHEMA_VERSION`] and the next reindex drops and rebuilds every derived table; recovery from
//! corruption is `delete index.db + reindex`. All DDL is written `IF NOT EXISTS` so
//! [`apply_schema`] is idempotent.

use rusqlite::Connection;
use std::path::Path;

use super::IndexError;

/// Version of [`SCHEMA_DDL`], stamped into `PRAGMA user_version` by a completed reindex. Bump it
/// whenever the DDL or the derivation of any table changes: an index stamped with another value
/// was built by a different binary, so `reindex::check_drift` reports drift and `reindex` drops
/// and recreates every derived table before rebuilding from the vault (ADR-0002).
///
/// 0 = unstamped (binaries before the stamp), 2 = `fact_links` + `edges`, 3 = tag edges,
/// 4 = `search_fts.ref` (fact slug of `memory` rows, artifact slug of `artifact` rows),
/// 5 = lexical word-overlap edges, 6 = lexical edge bm25 over the eligible rows only,
/// 7 = lexical edges keep the top 2 hits per idea and need 3 shared terms.
pub const SCHEMA_VERSION: i64 = 7;

const DERIVED_TABLES: [&str; 8] = [
    "edges",
    "idea_tags",
    "fact_links",
    "memory_facts",
    "backlinks",
    "search_fts",
    "tags",
    "ideas",
];

/// Full derived-index DDL (docs/03 §D6). Applied verbatim, idempotently.
const SCHEMA_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS ideas (
    id         INTEGER PRIMARY KEY,
    slug       TEXT UNIQUE NOT NULL,
    title      TEXT NOT NULL,
    state      TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS tags (
    id   INTEGER PRIMARY KEY,
    name TEXT UNIQUE NOT NULL
);

CREATE TABLE IF NOT EXISTS idea_tags (
    idea_id INTEGER NOT NULL REFERENCES ideas(id),
    tag_id  INTEGER NOT NULL REFERENCES tags(id),
    PRIMARY KEY (idea_id, tag_id)
);

CREATE TABLE IF NOT EXISTS memory_facts (
    id         INTEGER PRIMARY KEY,
    idea_id    INTEGER NOT NULL REFERENCES ideas(id),
    slug       TEXT NOT NULL,
    title      TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS backlinks (
    id             INTEGER PRIMARY KEY,
    source_idea_id INTEGER NOT NULL REFERENCES ideas(id),
    target_slug    TEXT NOT NULL,
    target_idea_id INTEGER REFERENCES ideas(id)
);

CREATE TABLE IF NOT EXISTS fact_links (
    id            INTEGER PRIMARY KEY,
    src_idea_id   INTEGER NOT NULL REFERENCES ideas(id),
    src_fact_id   INTEGER REFERENCES memory_facts(id),
    dst_idea_slug TEXT NOT NULL,
    dst_fact_slug TEXT NOT NULL,
    dst_fact_id   INTEGER REFERENCES memory_facts(id),
    explicit      INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS edges (
    src_idea_id INTEGER NOT NULL REFERENCES ideas(id),
    dst_idea_id INTEGER NOT NULL REFERENCES ideas(id),
    type        TEXT NOT NULL,
    weight      REAL NOT NULL,
    detail      TEXT NOT NULL,
    PRIMARY KEY (src_idea_id, dst_idea_id, type),
    CHECK (src_idea_id < dst_idea_id)
);

CREATE VIRTUAL TABLE IF NOT EXISTS search_fts USING fts5(
    idea_id UNINDEXED,
    kind    UNINDEXED,
    content,
    ref     UNINDEXED
);
"#;

/// Open (creating if absent) the index database at `path`, enable WAL, and apply the schema.
///
/// Creates the parent directory and the database file as needed. The database is a rebuildable
/// derived index — deleting the file and re-running this + reindex is a supported recovery path.
pub fn open_or_create(path: &Path) -> Result<Connection, IndexError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let conn = Connection::open(path)?;
    // WAL: better read/write concurrency for the server; safe for a derived index.
    conn.pragma_update(None, "journal_mode", "WAL")?;
    apply_schema(&conn)?;
    Ok(conn)
}

/// Drop every derived table and re-apply [`SCHEMA_DDL`], so a layout from another
/// [`SCHEMA_VERSION`] is replaced rather than kept by `IF NOT EXISTS`. Only `reindex` calls this,
/// inside its rebuild transaction.
pub(crate) fn recreate_schema(conn: &Connection) -> Result<(), IndexError> {
    for table in DERIVED_TABLES {
        conn.execute_batch(&format!("DROP TABLE IF EXISTS {table};"))?;
    }
    apply_schema(conn)
}

/// Apply the full derived-index DDL. Idempotent (`CREATE ... IF NOT EXISTS` throughout), so it is
/// safe to call on every startup regardless of whether the file already existed.
pub fn apply_schema(conn: &Connection) -> Result<(), IndexError> {
    conn.execute_batch(SCHEMA_DDL)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_or_create_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("index.db");

        // First call creates parent dirs, file, and schema.
        let conn1 = open_or_create(&path).unwrap();
        drop(conn1);
        // Second call on the same path must succeed (DDL is IF NOT EXISTS).
        let conn2 = open_or_create(&path).unwrap();
        // apply_schema again is also idempotent.
        apply_schema(&conn2).unwrap();
    }

    #[test]
    fn fts5_is_available_and_matches() {
        // Proves the bundled SQLite was built with FTS5 (the scaffold's main native-dep risk).
        let conn = Connection::open_in_memory().unwrap();
        apply_schema(&conn).unwrap();

        conn.execute(
            "INSERT INTO search_fts (idea_id, kind, content) VALUES (?1, ?2, ?3)",
            rusqlite::params![1_i64, "idea_body", "distributed idea market incentives"],
        )
        .unwrap();

        let hit: i64 = conn
            .query_row(
                "SELECT idea_id FROM search_fts WHERE search_fts MATCH ?1",
                rusqlite::params!["incentives"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(hit, 1);
    }
}
