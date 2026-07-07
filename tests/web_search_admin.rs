//! Web handler tests for R8 search and R10 admin reindex: FTS fragment rendering (escaped
//! snippets), hostile input, and the manual-reconcile path for edits the boot drift check
//! cannot see (hand-edited conversations — the ADR-0002 recovery story through the browser).

mod support;

use axum::http::StatusCode;
use chrono::{TimeZone, Utc};
use idea_vault::domain::{Idea, IdeaFrontmatter, IdeaState};
use idea_vault::vault::store;
use support::web::{get, post_form, test_state};

fn seed(vault: &std::path::Path, slug: &str, body: &str) {
    store::write_idea(
        vault,
        &Idea {
            frontmatter: IdeaFrontmatter {
                title: format!("Idea {slug}"),
                slug: slug.into(),
                state: IdeaState::InDiscussion,
                tags: vec![],
                created: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
                updated: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
            },
            body: body.into(),
        },
    )
    .unwrap();
}

#[tokio::test]
async fn search_renders_hits_fragment_after_reindex() {
    let (state, vault_dir) = test_state();
    seed(
        &vault_dir,
        "alpha",
        "This body mentions wombats twice: wombats.\n",
    );
    seed(&vault_dir, "beta", "Nothing relevant here.\n");
    let (status, _) = post_form(state.clone(), "/admin/reindex", "").await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = get(state.clone(), "/search?q=wombats").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("/idea/alpha"), "hit links to the idea");
    assert!(body.contains("Idea alpha"));
    assert!(!body.contains("/idea/beta"), "non-matching idea absent");

    // Prefix search-as-you-type (R8) and the empty state both render.
    let (_, body) = get(state.clone(), "/search?q=womb").await;
    assert!(body.contains("/idea/alpha"));
    let (_, body) = get(state, "/search?q=zzzznothing").await;
    assert!(body.contains("No matches."));
}

#[tokio::test]
async fn search_snippets_are_escaped_and_hostile_queries_never_500() {
    let (state, vault_dir) = test_state();
    seed(
        &vault_dir,
        "spiky",
        "A body with <script>alert('xss')</script> markup and a keyword: nightjar.\n",
    );
    let (status, _) = post_form(state.clone(), "/admin/reindex", "").await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = get(state.clone(), "/search?q=nightjar").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !body.contains("<script>"),
        "snippet content must be escaped, never raw HTML"
    );

    for hostile in [
        "%22unbalanced",
        "AND%20OR%20NOT%20(",
        "a%00b",
        "content%3Ax",
    ] {
        let (status, _) = get(state.clone(), &format!("/search?q={hostile}")).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "hostile query {hostile} must not 500"
        );
    }
    let (status, body) = get(state, "/search?q=").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("No matches."));
}

#[tokio::test]
async fn admin_reindex_returns_counts_and_reconciles_hand_edits() {
    let (state, vault_dir) = test_state();
    seed(&vault_dir, "alpha", "Original body with [[beta]] link.\n");
    seed(&vault_dir, "beta", "Second idea.\n");
    store::write_memory_fact(
        &vault_dir,
        "alpha",
        &idea_vault::domain::MemoryFact {
            frontmatter: idea_vault::domain::MemoryFactFrontmatter {
                slug: "fact-one".into(),
                title: "Fact one".into(),
                tags: vec![],
                created: Utc.with_ymd_and_hms(2026, 7, 7, 11, 0, 0).unwrap(),
                links: vec![],
            },
            body: "durable\n".into(),
        },
    )
    .unwrap();

    let (status, body) = post_form(state.clone(), "/admin/reindex", "").await;
    assert_eq!(status, StatusCode::OK);
    // D15: counts returned for verification.
    assert!(body.contains("\"ideas\":2"));
    assert!(body.contains("\"facts\":1"));
    assert!(body.contains("\"links\":1"));

    // The T3-documented gap: a hand-edited conversation is invisible to the boot drift check.
    // The admin route is the manual reconcile — content becomes searchable only after it.
    store::append_turn(&vault_dir, "alpha", "user", "a very rare word: axolotl").unwrap();
    let (_, before) = get(state.clone(), "/search?q=axolotl").await;
    assert!(
        before.contains("No matches."),
        "hand edit not yet indexed (expected)"
    );
    let (status, _) = post_form(state.clone(), "/admin/reindex", "").await;
    assert_eq!(status, StatusCode::OK);
    let (_, after) = get(state, "/search?q=axolotl").await;
    assert!(
        after.contains("/idea/alpha"),
        "reconciled after manual reindex"
    );
}
