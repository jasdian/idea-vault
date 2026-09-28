//! Cross-idea context: a short, separately budgeted block naming the ideas related to the one
//! under discussion (derived `edges` graph, D6), pushed into the chat prompt beside, never inside,
//! the idea's own assembled context.

use rusqlite::Connection;

use crate::index::queries::{self, RelatedIdea};
use crate::index::IndexError;

/// Related ideas scoring below this are noise and never shown.
pub const MIN_RELATED_SCORE: f64 = 0.1;

/// At most this many related ideas are shown.
pub const MAX_RELATED: usize = 5;

/// Render the related-ideas block for `slug` in at most `max_bytes` bytes, trailing blank line
/// included, or `""` when nothing qualifies or not even the header plus one entry fits.
///
/// Entries follow [`crate::index::queries::related_ideas`] order and are added whole; the queried
/// idea is never an entry. Deterministic for a given index.
pub fn related_block(
    conn: &Connection,
    slug: &str,
    max_bytes: usize,
) -> Result<String, IndexError> {
    if max_bytes <= HEADER.len() + 1 {
        return Ok(String::new());
    }
    let mut block = String::from(HEADER);
    let mut entries = 0;
    for idea in queries::related_ideas(conn, slug, MAX_RELATED)? {
        if idea.score < MIN_RELATED_SCORE {
            continue;
        }
        let entry = render_entry(conn, slug, &idea)?;
        if block.len() + entry.len() + 1 > max_bytes {
            break;
        }
        block.push_str(&entry);
        entries += 1;
    }
    if entries == 0 {
        return Ok(String::new());
    }
    block.push('\n');
    Ok(block)
}

const HEADER: &str = "## Related ideas elsewhere in the vault\n\
Context only: other ideas the owner has explored. This discussion is about the idea below; \
do not switch to them unless the owner does.\n";

const MAX_REASON_CHARS: usize = 120;
const MAX_TITLE_CHARS: usize = 80;
const MAX_FACT_TITLES: usize = 2;
const OWN_IDEA: &str = "this idea";

fn render_entry(conn: &Connection, own: &str, idea: &RelatedIdea) -> Result<String, IndexError> {
    let reasons: Vec<String> = if idea.hops == 1 {
        idea.reasons.iter().map(|r| redact_own(r, own)).collect()
    } else {
        idea.reasons.iter().take(1).cloned().collect()
    };
    let reasons: Vec<String> = reasons
        .iter()
        .map(|r| truncate_chars(r, MAX_REASON_CHARS))
        .collect();
    let mut entry = format!(
        "- {} (`{}`): {}\n",
        truncate_chars(&idea.title, MAX_TITLE_CHARS),
        idea.slug,
        reasons.join("; ")
    );
    let facts = recent_fact_titles(conn, &idea.slug)?;
    if !facts.is_empty() {
        entry.push_str(&format!("  Facts: {}\n", facts.join("; ")));
    }
    Ok(entry)
}

fn recent_fact_titles(conn: &Connection, slug: &str) -> Result<Vec<String>, IndexError> {
    let mut stmt = conn.prepare(
        "SELECT f.title FROM memory_facts f JOIN ideas i ON i.id = f.idea_id
         WHERE i.slug = ?1
         ORDER BY f.created_at DESC, f.slug ASC
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(rusqlite::params![slug, MAX_FACT_TITLES as i64], |row| {
        row.get::<_, String>(0)
    })?;
    let titles: Vec<String> = rows.collect::<Result<_, _>>()?;
    Ok(titles
        .iter()
        .map(|t| truncate_chars(t, MAX_TITLE_CHARS))
        .collect())
}

/// Replace the queried idea's own slug inside `link:` reasons with a neutral phrase, so a block
/// or panel about an idea never names that idea. Tag details are tag names, not idea
/// references, and are left alone.
pub fn redact_own(reason: &str, own: &str) -> String {
    reason
        .split("; ")
        .map(|part| match part.strip_prefix("link: ") {
            Some(detail) => format!("link: {}", replace_slug_token(detail, own)),
            None => part.to_string(),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn replace_slug_token(text: &str, slug: &str) -> String {
    if slug.is_empty() {
        return text.to_string();
    }
    let is_slug_char = |c: char| c.is_alphanumeric() || c == '-' || c == '_';
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find(slug) {
        let before = rest[..pos].chars().next_back();
        let after = rest[pos + slug.len()..].chars().next();
        out.push_str(&rest[..pos]);
        if before.is_some_and(is_slug_char) || after.is_some_and(is_slug_char) {
            out.push_str(slug);
        } else {
            out.push_str(OWN_IDEA);
        }
        rest = &rest[pos + slug.len()..];
    }
    out.push_str(rest);
    out
}

/// Collapse whitespace and cut to `max_chars` characters, ending with an ellipsis when cut.
pub fn truncate_chars(text: &str, max_chars: usize) -> String {
    let text = one_line(text);
    match text.char_indices().nth(max_chars) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text,
    }
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use std::path::Path;

    use crate::domain::{Idea, IdeaFrontmatter, IdeaState, MemoryFact, MemoryFactFrontmatter};
    use crate::index::queries::related_ideas;
    use crate::index::reindex::reindex;
    use crate::index::schema::apply_schema;
    use crate::vault::store;

    fn write_idea(vault: &Path, slug: &str, title: &str, tags: &[&str], body: &str) {
        store::write_idea(
            vault,
            &Idea {
                frontmatter: IdeaFrontmatter {
                    title: title.into(),
                    slug: slug.into(),
                    state: IdeaState::InDiscussion,
                    tags: tags.iter().map(|t| t.to_string()).collect(),
                    sources: vec![],
                    created: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
                    updated: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
                },
                body: body.into(),
            },
        )
        .unwrap();
    }

    fn write_fact(vault: &Path, idea: &str, slug: &str, title: &str, hour: u32) {
        store::write_memory_fact(
            vault,
            idea,
            &MemoryFact {
                frontmatter: MemoryFactFrontmatter {
                    slug: slug.into(),
                    title: title.into(),
                    tags: vec![],
                    created: Utc.with_ymd_and_hms(2026, 7, 7, hour, 0, 0).unwrap(),
                    links: vec![],
                },
                body: "Fact body.\n".into(),
            },
        )
        .unwrap();
    }

    fn index(vault: &Path) -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_schema(&conn).unwrap();
        reindex(&mut conn, vault).unwrap();
        conn
    }

    #[test]
    fn related_block_caps_titles_so_one_long_title_cannot_starve_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let long = "Very long title ".repeat(200);
        write_idea(
            tmp.path(),
            "alpha",
            "Own",
            &[],
            "Links [[beta]] and [[gamma]].\n",
        );
        write_idea(tmp.path(), "beta", &long, &[], "Standalone.\n");
        write_idea(tmp.path(), "gamma", "Short neighbour", &[], "Standalone.\n");
        write_fact(tmp.path(), "beta", "a-fact", &long, 9);
        let conn = index(tmp.path());

        let block = related_block(&conn, "alpha", 1_024).unwrap();
        assert!(block.contains("(`beta`)"), "got {block:?}");
        assert!(
            block.contains("- Short neighbour (`gamma`): "),
            "got {block:?}"
        );
        assert!(block.len() <= 1_024);
    }

    #[test]
    fn related_block_excludes_the_own_slug() {
        let tmp = tempfile::tempdir().unwrap();
        write_idea(tmp.path(), "alpha", "Own", &[], "Links [[beta]].\n");
        write_idea(tmp.path(), "beta", "Neighbour", &[], "Standalone.\n");
        let conn = index(tmp.path());

        let block = related_block(&conn, "alpha", 4_096).unwrap();
        assert!(block.starts_with(HEADER), "labelled block, got {block:?}");
        assert!(block.contains("- Neighbour (`beta`): "));
        assert!(!block.contains("alpha"), "own slug leaked: {block:?}");
        assert!(block.ends_with("\n\n"), "ends with a blank line");
    }

    #[test]
    fn related_block_respects_max_bytes_with_whole_entries() {
        let tmp = tempfile::tempdir().unwrap();
        write_idea(
            tmp.path(),
            "alpha",
            "Own",
            &[],
            "Links [[beta]] and [[gamma]].\n",
        );
        write_idea(tmp.path(), "beta", "Beta", &[], "One.\n");
        write_idea(tmp.path(), "gamma", "Gamma", &[], "Two.\n");
        let conn = index(tmp.path());

        let full = related_block(&conn, "alpha", 100_000).unwrap();
        assert!(full.contains("(`beta`)") && full.contains("(`gamma`)"));
        assert_eq!(full, related_block(&conn, "alpha", 100_000).unwrap());
        assert_eq!(related_block(&conn, "alpha", full.len()).unwrap(), full);

        let one = related_block(&conn, "alpha", full.len() - 1).unwrap();
        assert!(one.contains("(`beta`)"), "first entry kept, got {one:?}");
        assert!(!one.contains("gamma"), "second entry dropped whole");

        let full_lines: Vec<&str> = full.lines().collect();
        for max in 0..=full.len() {
            let block = related_block(&conn, "alpha", max).unwrap();
            assert!(block.len() <= max, "{} > {max}", block.len());
            if block.is_empty() {
                continue;
            }
            assert!(block.contains("- "), "never a bare header at {max}");
            assert!(
                block.lines().all(|l| full_lines.contains(&l)),
                "no partial entry"
            );
        }
        assert_eq!(related_block(&conn, "alpha", HEADER.len()).unwrap(), "");
    }

    #[test]
    fn related_block_drops_ideas_below_min_related_score() {
        let tmp = tempfile::tempdir().unwrap();
        write_idea(tmp.path(), "alpha", "Own", &[], "Links [[beta]].\n");
        write_idea(tmp.path(), "beta", "Beta", &["shared"], "One.\n");
        write_idea(tmp.path(), "gamma", "Gamma", &["shared"], "Two.\n");
        let conn = index(tmp.path());

        let related = related_ideas(&conn, "alpha", 10).unwrap();
        let gamma = related
            .iter()
            .find(|r| r.slug == "gamma")
            .expect("gamma reached");
        assert!(
            gamma.score < MIN_RELATED_SCORE,
            "fixture premise: {}",
            gamma.score
        );

        let block = related_block(&conn, "alpha", 4_096).unwrap();
        assert!(block.contains("(`beta`)"), "got {block:?}");
        assert!(!block.contains("gamma") && !block.contains("Gamma"));
    }

    #[test]
    fn related_block_is_empty_with_no_edges() {
        let tmp = tempfile::tempdir().unwrap();
        write_idea(tmp.path(), "alpha", "Own", &["mine"], "Nothing linked.\n");
        write_idea(tmp.path(), "beta", "Beta", &["theirs"], "Nothing linked.\n");
        let conn = index(tmp.path());

        assert_eq!(related_block(&conn, "alpha", 4_096).unwrap(), "");
        assert_eq!(related_block(&conn, "unknown", 4_096).unwrap(), "");
    }

    #[test]
    fn related_block_shows_the_two_most_recent_fact_titles() {
        let tmp = tempfile::tempdir().unwrap();
        write_idea(tmp.path(), "alpha", "Own", &[], "Links [[beta]].\n");
        write_idea(tmp.path(), "beta", "Beta", &[], "One.\n");
        write_fact(tmp.path(), "beta", "oldest", "Oldest fact", 1);
        write_fact(tmp.path(), "beta", "newest", "Newest fact", 3);
        write_fact(tmp.path(), "beta", "middle", "Middle fact", 2);
        let conn = index(tmp.path());

        let block = related_block(&conn, "alpha", 4_096).unwrap();
        assert!(
            block.contains("\n  Facts: Newest fact; Middle fact\n"),
            "got {block:?}"
        );
        assert!(!block.contains("Oldest fact"));
    }

    #[test]
    fn related_block_shows_one_reason_per_two_hop_idea() {
        let tmp = tempfile::tempdir().unwrap();
        write_idea(
            tmp.path(),
            "alpha",
            "Own",
            &[],
            "Links [[beta]] and [[delta]].\n",
        );
        write_idea(tmp.path(), "beta", "Beta", &[], "Links [[gamma]].\n");
        write_idea(tmp.path(), "delta", "Delta", &[], "Links [[gamma]].\n");
        write_idea(tmp.path(), "gamma", "Gamma", &[], "Leaf.\n");
        let conn = index(tmp.path());

        let block = related_block(&conn, "alpha", 4_096).unwrap();
        let gamma = block
            .lines()
            .find(|l| l.contains("(`gamma`)"))
            .unwrap_or_else(|| panic!("gamma entry missing: {block:?}"));
        assert_eq!(gamma.matches("via ").count(), 1, "got {gamma:?}");
    }

    #[test]
    fn related_block_truncates_long_reasons() {
        let tmp = tempfile::tempdir().unwrap();
        let facts = [
            "first-extremely-long-fact-slug-about-orchard-frost",
            "second-extremely-long-fact-slug-about-orchard-frost",
            "third-extremely-long-fact-slug-about-orchard-frost",
        ];
        let body: String = facts
            .iter()
            .map(|f| format!("See [[beta#{f}]]. "))
            .collect();
        write_idea(tmp.path(), "alpha", "Own", &[], &body);
        write_idea(tmp.path(), "beta", "Beta", &[], "One.\n");
        for (hour, fact) in facts.iter().enumerate() {
            write_fact(tmp.path(), "beta", fact, "Fact", hour as u32 + 1);
        }
        let conn = index(tmp.path());

        let block = related_block(&conn, "alpha", 4_096).unwrap();
        let line = block
            .lines()
            .find(|l| l.contains("(`beta`)"))
            .unwrap_or_else(|| panic!("beta entry missing: {block:?}"));
        let reason = line.split_once("`): ").unwrap().1;
        assert!(reason.ends_with('…'), "got {reason:?}");
        assert_eq!(reason.chars().count(), MAX_REASON_CHARS + 1);
        assert!(!reason.contains("alpha"));
    }
}
