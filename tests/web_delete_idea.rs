//! Deleting an entire idea: removes the vault folder and drops it from the index.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers outside #[test] fns; HTC-6 binds shipping code"
)]

mod support;

use axum::http::StatusCode;
use chrono::{TimeZone, Utc};
use idea_vault::domain::{Idea, IdeaFrontmatter, IdeaState};
use idea_vault::vault::store;
use support::web::{get, post_form, test_state};

fn seed(vault: &std::path::Path, slug: &str) {
    store::write_idea(
        vault,
        &Idea {
            frontmatter: IdeaFrontmatter {
                title: format!("Idea {slug}"),
                slug: slug.into(),
                state: IdeaState::InDiscussion,
                tags: vec![],
                sources: vec![],
                created: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
                updated: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
                extra: Default::default(),
            },
            body: "body\n".into(),
        },
    )
    .unwrap();
    store::append_conversation(vault, slug, "## user\nhi\n").unwrap();
}

#[tokio::test]
async fn deleting_an_idea_removes_the_folder_and_deindexes_it() {
    let (state, vault) = test_state();
    seed(&vault, "keep-me");
    seed(&vault, "kill-me");
    // Index them so the list shows both.
    store::append_conversation(&vault, "keep-me", "## assistant\nx\n").unwrap();

    let (status, _) = post_form(state.clone(), "/idea/kill-me/delete", "").await;
    assert_eq!(status, StatusCode::OK);

    // Folder gone; the other idea untouched.
    assert!(!vault.join("kill-me").exists());
    assert!(vault.join("keep-me/idea.md").is_file());

    // Gone from the list; the 404 for the deleted idea page.
    let (_, list) = get(state.clone(), "/").await;
    assert!(list.contains("Idea keep-me") && !list.contains("Idea kill-me"));
    let (status, _) = get(state, "/idea/kill-me").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Deleting the LAST idea is the one legitimate way to reach "vault empty, index populated" —
/// precisely the state the ADR-0019 empty-vault guard refuses to rebuild from. The delete route
/// therefore forces the rebuild; without that, the guard would strand the just-deleted idea in
/// the list forever, and clicking it would 404. Regression test for that interaction.
#[tokio::test]
async fn deleting_the_last_idea_empties_the_list() {
    let (state, vault) = test_state();
    seed(&vault, "only-one");
    // Index it, so at delete time the vault empties while the index still holds a row — the exact
    // state the guard refuses to rebuild from.
    let (status, _) = post_form(state.clone(), "/admin/reindex", "").await;
    assert_eq!(status, StatusCode::OK);

    let (_, list) = get(state.clone(), "/").await;
    assert!(list.contains("Idea only-one"), "precondition: it is listed");

    let (status, _) = post_form(state.clone(), "/idea/only-one/delete", "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!vault.join("only-one").exists());

    let (_, list) = get(state, "/").await;
    assert!(
        !list.contains("Idea only-one"),
        "the deleted idea must not survive in the index as a phantom row"
    );
    assert!(list.contains("Nothing here yet"));
}

#[tokio::test]
async fn deleting_a_missing_idea_is_404() {
    let (state, _vault) = test_state();
    let (status, _) = post_form(state, "/idea/ghost/delete", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
