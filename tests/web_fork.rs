//! The "btw" history view and fork-to-new-idea: a fork carries the full context (body +
//! conversation + memory) into a new idea, leaving the original untouched.

mod support;

use axum::http::StatusCode;
use chrono::{TimeZone, Utc};
use idea_vault::domain::{Idea, IdeaFrontmatter, IdeaState, MemoryFact, MemoryFactFrontmatter};
use idea_vault::vault::store;
use support::web::{get, post_form, test_state};

fn seed(vault: &std::path::Path) {
    store::write_idea(
        vault,
        &Idea {
            frontmatter: IdeaFrontmatter {
                title: "Forky".into(),
                slug: "forky".into(),
                state: IdeaState::InDiscussion,
                tags: vec!["risk".into()],
                created: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
                updated: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
            },
            body: "The best statement so far.\n".into(),
        },
    )
    .unwrap();
    store::append_conversation(vault, "forky", "## user\nmain line question\n").unwrap();
    store::append_conversation(vault, "forky", "## assistant\na reply\n").unwrap();
    store::write_memory_fact(
        vault,
        "forky",
        &MemoryFact {
            frontmatter: MemoryFactFrontmatter {
                slug: "durable".into(),
                title: "Durable".into(),
                tags: vec![],
                created: Utc.with_ymd_and_hms(2026, 7, 7, 11, 0, 0).unwrap(),
                links: vec![],
            },
            body: "The one conclusion.\n".into(),
        },
    )
    .unwrap();
    store::rebuild_memory_index(vault, "forky").unwrap();
}

#[tokio::test]
async fn history_view_shows_the_thread_and_a_fork_control() {
    let (state, vault) = test_state();
    seed(&vault);
    let (status, body) = get(state, "/idea/forky/history").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("btw — Forky"));
    assert!(body.contains("main line question") && body.contains("a reply"));
    assert!(
        body.contains("hx-post=\"/idea/forky/fork\""),
        "fork control present"
    );
}

#[tokio::test]
async fn forking_copies_full_context_into_a_new_idea_and_leaves_the_original() {
    let (state, vault) = test_state();
    seed(&vault);

    let (status, _) = post_form(state, "/idea/forky/fork", "").await;
    assert_eq!(status, StatusCode::OK);

    // The fork exists with the derived slug, carrying body + conversation + memory.
    let fork = store::read_idea(&vault, "forky-fork").expect("fork created");
    assert_eq!(fork.frontmatter.title, "Forky (fork)");
    assert_eq!(fork.frontmatter.state, IdeaState::InDiscussion);
    assert_eq!(fork.body, "The best statement so far.\n");
    let convo = store::read_conversation(&vault, "forky-fork").unwrap();
    assert!(convo.contains("main line question") && convo.contains("a reply"));
    assert_eq!(
        store::read_memory_index(&vault, "forky-fork")
            .unwrap()
            .entries
            .len(),
        1
    );

    // The original is untouched.
    assert!(store::read_idea(&vault, "forky").is_ok());
}
