//! Golden-vault snapshot test (docs/10-testing-strategy.md "Golden vaults"): a checked-in
//! fixture vault covering every state and the documented edge cases (dangling backlink,
//! reopened-with-merged-memory, unicode-title→ASCII-slug) is reindexed and its derived tables
//! are snapshot-compared against a committed expectation. Also re-asserts the ADR-0002 keystone
//! (idempotent + rebuildable) against real on-disk fixtures rather than generated ones.

use std::path::Path;

use idea_vault::index::{reindex, schema};
use rusqlite::Connection;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/golden-vault");
const EXPECTED_SNAPSHOT: &str = include_str!("fixtures/golden-vault.snap");

/// A normalized, id-free, deterministic dump of every derived table — the same shape reindex
/// determinism guarantees, so it is stable to commit as a golden file.
fn snapshot(conn: &Connection) -> String {
    let mut out = String::new();
    let mut run = |header: &str, sql: &str| {
        out.push_str(header);
        out.push('\n');
        let mut stmt = conn.prepare(sql).unwrap();
        let cols = stmt.column_count();
        let mut rows = stmt.query([]).unwrap();
        while let Some(row) = rows.next().unwrap() {
            let cells: Vec<String> = (0..cols)
                .map(|i| {
                    row.get::<_, Option<String>>(i)
                        .unwrap()
                        .unwrap_or_else(|| "∅".into())
                })
                .collect();
            out.push_str("  ");
            out.push_str(&cells.join(" | "));
            out.push('\n');
        }
    };
    run(
        "[ideas]",
        "SELECT slug, title, state, created_at, updated_at FROM ideas ORDER BY slug",
    );
    run(
        "[tags]",
        "SELECT i.slug, t.name FROM idea_tags it JOIN ideas i ON i.id=it.idea_id \
         JOIN tags t ON t.id=it.tag_id ORDER BY i.slug, t.name",
    );
    run(
        "[memory_facts]",
        "SELECT i.slug, f.slug, f.title, f.created_at FROM memory_facts f \
         JOIN ideas i ON i.id=f.idea_id ORDER BY i.slug, f.slug",
    );
    run(
        "[backlinks]",
        "SELECT s.slug, b.target_slug, t.slug FROM backlinks b \
         JOIN ideas s ON s.id=b.source_idea_id LEFT JOIN ideas t ON t.id=b.target_idea_id \
         ORDER BY s.slug, b.target_slug",
    );
    run(
        "[search_fts]",
        "SELECT i.slug, s.kind FROM search_fts s JOIN ideas i ON i.id=s.idea_id \
         ORDER BY i.slug, s.kind",
    );
    out
}

fn reindex_fixture(vault: &Path) -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    schema::apply_schema(&conn).unwrap();
    reindex::reindex(&mut conn, vault).unwrap();
    conn
}

#[test]
fn golden_vault_reindex_matches_committed_snapshot() {
    let conn = reindex_fixture(Path::new(FIXTURE));
    let actual = snapshot(&conn);
    // If this fails after an intended reindex change, review the diff and update the .snap file.
    assert_eq!(
        actual.trim(),
        EXPECTED_SNAPSHOT.trim(),
        "golden-vault snapshot drift:\n---actual---\n{actual}"
    );
}

#[test]
fn golden_vault_edge_cases_resolve_as_documented() {
    let conn = reindex_fixture(Path::new(FIXTURE));

    // Every idea state is represented and indexed verbatim (D9/ADR-0007).
    let states: Vec<(String, String)> = {
        let mut stmt = conn
            .prepare("SELECT slug, state FROM ideas ORDER BY slug")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    assert!(states.contains(&("draft-idea".into(), "draft".into())));
    assert!(states.contains(&("in-discussion".into(), "in_discussion".into())));
    assert!(states.contains(&("stored-idea".into(), "stored".into())));
    assert!(states.contains(&("reopened-merged".into(), "reopened".into())));
    // The unicode-title idea is indexed under its ASCII slug (D22).
    assert!(states.iter().any(|(s, _)| s == "caf-ber-ide"));

    // Dangling backlink: draft-idea → never-created stays unresolved (NULL); stored target resolves.
    let dangling: Option<String> = conn
        .query_row(
            "SELECT t.slug FROM backlinks b JOIN ideas s ON s.id=b.source_idea_id \
             LEFT JOIN ideas t ON t.id=b.target_idea_id \
             WHERE s.slug='draft-idea' AND b.target_slug='never-created'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(dangling, None, "dangling ref stays NULL (D23)");
    let resolved: Option<String> = conn
        .query_row(
            "SELECT t.slug FROM backlinks b JOIN ideas s ON s.id=b.source_idea_id \
             LEFT JOIN ideas t ON t.id=b.target_idea_id \
             WHERE s.slug='draft-idea' AND b.target_slug='stored-idea'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(resolved, Some("stored-idea".into()));

    // Reopened-with-merged-memory carries both facts.
    let fact_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory_facts f JOIN ideas i ON i.id=f.idea_id \
             WHERE i.slug='reopened-merged'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(fact_count, 2);

    // Distractors (README.txt, not-an-idea-dir without idea.md) are skipped by the walk.
    let idea_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM ideas", [], |r| r.get(0))
        .unwrap();
    assert_eq!(idea_count, 5, "only the 5 real idea dirs are indexed");
}

#[test]
fn golden_vault_reindex_is_idempotent_and_rebuildable() {
    // ADR-0002 keystone against real on-disk fixtures (complements the generated-vault test).
    let mut conn = reindex_fixture(Path::new(FIXTURE));
    let first = snapshot(&conn);
    reindex::reindex(&mut conn, Path::new(FIXTURE)).unwrap();
    assert_eq!(first, snapshot(&conn), "reindex is idempotent");

    let fresh = reindex_fixture(Path::new(FIXTURE));
    assert_eq!(first, snapshot(&fresh), "rebuildable from the vault alone");
}
