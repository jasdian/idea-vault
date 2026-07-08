//! Reindex — rebuild the derived SQLite index from `vault/**` (docs/03-data-model.md §D15).
//!
//! This is the operation that enforces the *reindex invariant* (ADR-0002): the whole index is
//! reconstructable from markdown alone. It runs inside a single transaction and returns counts so
//! callers (and the property test from docs/10-testing-strategy.md, below) can verify the rebuild.

use std::path::Path;

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{params, Connection};

use super::{IndexError, SNIPPET_MATCH_CLOSE, SNIPPET_MATCH_OPEN};
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
/// the boot-relevant drift. It deliberately does not diff conversations or fact bodies
/// (post-write upserts keep those fresh; `POST /admin/reindex` is the manual override), and it
/// skips unparsable idea dirs the same way `reindex` does, so a malformed file never wedges boot.
pub fn check_drift(conn: &Connection, vault_dir: &Path) -> Result<bool, IndexError> {
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
/// frontmatter `links:` list — deduplicated per source idea, first-occurrence order. The
/// conversation transcript is indexed for search but deliberately not mined for backlinks
/// (chat text mentioning an idea is not a curated cross-reference).
pub fn reindex(conn: &mut Connection, vault_dir: &Path) -> Result<ReindexCounts, IndexError> {
    let tx = conn.transaction()?;
    let mut counts = ReindexCounts::default();

    // 2. Clear every derived table — full rebuild semantics.
    tx.execute_batch(
        "DELETE FROM idea_tags;
         DELETE FROM memory_facts;
         DELETE FROM backlinks;
         DELETE FROM search_fts;
         DELETE FROM tags;
         DELETE FROM ideas;",
    )?;

    // 3–9. Walk the vault and repopulate.
    for entry in walk::walk_ideas(vault_dir)? {
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
            "INSERT INTO search_fts (idea_id, kind, content) VALUES (?1, 'title', ?2)",
            params![idea_id, sanitized(&fm.title)],
        )?;
        if !fm.tags.is_empty() {
            // Space-joined so multi-word tags stay separable tokens; omitted entirely when there
            // are no tags rather than indexing an empty row.
            tx.execute(
                "INSERT INTO search_fts (idea_id, kind, content) VALUES (?1, 'tags', ?2)",
                params![idea_id, sanitized(&fm.tags.join(" "))],
            )?;
        }
        tx.execute(
            "INSERT INTO search_fts (idea_id, kind, content) VALUES (?1, 'idea_body', ?2)",
            params![idea_id, sanitized(&idea.body)],
        )?;
        let conversation = store::read_conversation(vault_dir, &entry.slug)?;
        if !conversation.is_empty() {
            tx.execute(
                "INSERT INTO search_fts (idea_id, kind, content) VALUES (?1, 'conversation', ?2)",
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
                "INSERT INTO search_fts (idea_id, kind, content) VALUES (?1, 'artifact', ?2)",
                params![
                    idea_id,
                    sanitized(&format!(
                        "{}\n\n{}",
                        artifact.frontmatter.title, artifact.body
                    ))
                ],
            )?;
        }

        // 7 + 9. Memory facts and `[[slug]]` link targets.
        let mut targets: Vec<String> = links::extract_links(&idea.body);
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
            counts.facts += 1;

            // Fact bodies are owner-authored durable truth (the `memory_facts` table and
            // MEMORY.md are index-only pointers, no body column) — one 'memory' search_fts row
            // per fact, title+body, so extracted facts are finally searchable like idea_body.
            tx.execute(
                "INSERT INTO search_fts (idea_id, kind, content) VALUES (?1, 'memory', ?2)",
                params![
                    idea_id,
                    sanitized(&format!("{}\n\n{}", fact.frontmatter.title, fact.body))
                ],
            )?;

            for target in links::extract_links(&fact.body) {
                targets.push(target);
            }
            for target in &fact.frontmatter.links {
                // Frontmatter `links:` entries are author-provided strings — hold them to the
                // same canonical-slug bar as `[[slug]]` tokens.
                if domain_slug::is_valid(target) {
                    targets.push(target.clone());
                }
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

    tx.commit()?;
    Ok(counts)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::domain::{Idea, IdeaFrontmatter, IdeaState, MemoryFact, MemoryFactFrontmatter};
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
                created: dt(10),
                updated: dt(11),
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
            },
            body: body.into(),
        }
    }

    /// Fixture per docs/10-testing-strategy.md: mixed states, tags, facts, `[[slug]]` links
    /// including dangling and forward references, plus a conversation transcript and a
    /// knowledge-extraction artifact (docs/adr/0015).
    fn build_fixture_vault(vault: &Path) {
        store::write_idea(
            vault,
            &idea(
                "alpha",
                "Alpha",
                IdeaState::InDiscussion,
                &["markets", "risk"],
                "Alpha builds on [[beta]] but also on [[ghost-idea]] (not created yet).\n",
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
            "SELECT 'fts', i.slug, s.kind, s.content FROM search_fts s
             JOIN ideas i ON i.id = s.idea_id ORDER BY i.slug, s.kind",
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
    fn counts_and_backlink_resolution_match_the_fixture() {
        let tmp = tempfile::tempdir().unwrap();
        build_fixture_vault(tmp.path());
        let mut conn = mem_conn();

        let counts = reindex(&mut conn, tmp.path()).unwrap();
        // alpha: [[beta]], [[ghost-idea]] — beta: [[alpha]] (body + fact, deduped) + [[gamma]]
        // (fact body); the fact's frontmatter "Not A Slug" entry is rejected.
        assert_eq!(
            counts,
            ReindexCounts {
                ideas: 2,
                facts: 1,
                links: 4
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
                ("beta".into(), "gamma".into(), None),
            ]
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

        // The artifact's [[beta]] link is NOT a backlink (alpha's only targets come from its
        // own body: beta + ghost-idea).
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
        assert_eq!(counts.ideas, 2); // broken is skipped, the rest indexed

        // And the skip is stable: drift check ignores it the same way.
        assert!(!check_drift(&conn, tmp.path()).unwrap());
    }
}
