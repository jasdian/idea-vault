//! Deleting an entire idea: removes the vault folder and drops it from the index.

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
                created: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
                updated: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
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

#[tokio::test]
async fn deleting_a_missing_idea_is_404() {
    let (state, _vault) = test_state();
    let (status, _) = post_form(state, "/idea/ghost/delete", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
