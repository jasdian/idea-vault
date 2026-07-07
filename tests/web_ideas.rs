//! Web handler tests for R3 create (D10): create→Draft with truth on disk before index, slug
//! collision handling, and input validation. Runs the real router over temp dirs.

mod support;

use axum::http::StatusCode;
use idea_vault::domain::IdeaState;
use idea_vault::vault::store;
use support::web::{get, post_form, test_state};

#[tokio::test]
async fn create_idea_lands_as_draft_on_disk_and_in_index() {
    let (state, vault_dir) = test_state();

    let (status, body) =
        post_form(state.clone(), "/ideas", "title=Distributed%20Idea%20Market").await;
    assert_eq!(status, StatusCode::OK);
    // The row partial links to the new idea and shows its state.
    assert!(body.contains("/idea/distributed-idea-market"));
    assert!(body.contains("draft"));

    // Truth on disk: idea.md in Draft, conversation.md created empty (D10 post-conditions).
    let idea = store::read_idea(&vault_dir, "distributed-idea-market").unwrap();
    assert_eq!(idea.frontmatter.state, IdeaState::Draft);
    assert_eq!(idea.frontmatter.title, "Distributed Idea Market");
    assert!(vault_dir
        .join("distributed-idea-market/conversation.md")
        .is_file());
    assert_eq!(
        store::read_conversation(&vault_dir, "distributed-idea-market").unwrap(),
        ""
    );

    // And in the index: the list page renders it. The empty-state placeholder is always in the
    // DOM (hidden by CSS once a row exists) and must come AFTER the rows for that CSS to work.
    let (status, list) = get(state, "/").await;
    assert_eq!(status, StatusCode::OK);
    let row_pos = list.find("Distributed Idea Market").expect("row rendered");
    let placeholder_pos = list.find("No ideas yet").expect("placeholder in DOM");
    assert!(row_pos < placeholder_pos, "rows precede the empty-state");
}

#[tokio::test]
async fn duplicate_title_gets_a_disambiguated_slug() {
    let (state, vault_dir) = test_state();

    let (s1, _) = post_form(state.clone(), "/ideas", "title=Same%20Title").await;
    let (s2, body2) = post_form(state.clone(), "/ideas", "title=Same%20Title").await;
    assert_eq!(s1, StatusCode::OK);
    assert_eq!(s2, StatusCode::OK);

    assert!(body2.contains("/idea/same-title-2"), "second gets -2 slug");
    assert!(vault_dir.join("same-title/idea.md").is_file());
    assert!(vault_dir.join("same-title-2/idea.md").is_file());
    // Both retain the same human title; the slug is the identity (D22).
    assert_eq!(
        store::read_idea(&vault_dir, "same-title-2")
            .unwrap()
            .frontmatter
            .title,
        "Same Title"
    );
}

#[tokio::test]
async fn empty_or_whitespace_title_is_rejected_with_400() {
    let (state, vault_dir) = test_state();

    let (s1, _) = post_form(state.clone(), "/ideas", "title=").await;
    assert_eq!(s1, StatusCode::BAD_REQUEST);
    let (s2, _) = post_form(state.clone(), "/ideas", "title=%20%20%20").await;
    assert_eq!(s2, StatusCode::BAD_REQUEST);

    // Nothing was created for either attempt.
    let entries: Vec<_> = std::fs::read_dir(&vault_dir).unwrap().collect();
    assert!(entries.is_empty(), "vault must stay empty on rejects");
}

#[tokio::test]
async fn seed_body_is_written_when_provided() {
    let (state, vault_dir) = test_state();

    let (status, _) = post_form(
        state,
        "/ideas",
        "title=Seeded&body=The%20raw%20seed%20statement.",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let idea = store::read_idea(&vault_dir, "seeded").unwrap();
    assert_eq!(idea.body, "The raw seed statement.\n");
}
