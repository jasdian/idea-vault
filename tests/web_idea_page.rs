//! Web handler tests for R2, the idea page: sanitized markdown rendering, transcript, memory
//! panel, and the D20 degraded/available compose-box states.

mod support;

use axum::http::StatusCode;
use chrono::{TimeZone, Utc};
use idea_vault::domain::{Idea, IdeaFrontmatter, IdeaState, MemoryFact, MemoryFactFrontmatter};
use idea_vault::vault::store;
use support::web::{get, post_form, test_state, test_state_with_ollama};
use support::{spawn, ChatScript};

fn seed(vault: &std::path::Path, state: IdeaState, body: &str) {
    store::write_idea(
        vault,
        &Idea {
            frontmatter: IdeaFrontmatter {
                title: "Sharp Idea".into(),
                slug: "sharp-idea".into(),
                state,
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
async fn idea_page_renders_sanitized_body_transcript_and_memory() {
    let (state, vault_dir) = test_state();
    seed(
        &vault_dir,
        IdeaState::InDiscussion,
        "Some **bold** claim.\n\n<script>alert('xss')</script>\n",
    );
    store::append_conversation(
        &vault_dir,
        "sharp-idea",
        "## user\nfirst *probing* question\n",
    )
    .unwrap();
    store::append_conversation(&vault_dir, "sharp-idea", "## assistant\na counterpoint\n").unwrap();
    store::write_memory_fact(
        &vault_dir,
        "sharp-idea",
        &MemoryFact {
            frontmatter: MemoryFactFrontmatter {
                slug: "core-tension".into(),
                title: "Core tension".into(),
                tags: vec![],
                created: Utc.with_ymd_and_hms(2026, 7, 7, 11, 0, 0).unwrap(),
                links: vec![],
            },
            body: "The one durable conclusion.\n".into(),
        },
    )
    .unwrap();
    store::rebuild_memory_index(&vault_dir, "sharp-idea").unwrap();

    let (status, body) = get(state, "/idea/sharp-idea").await;
    assert_eq!(status, StatusCode::OK);

    // Body: markdown rendered, the injected script stripped (sanitized server-side). The page's
    // own trusted <script> (copy-button JS in base.html) is fine — assert the XSS payload is gone.
    assert!(body.contains("<strong>bold</strong>"));
    assert!(
        !body.contains("alert('xss')"),
        "injected scripts must never reach the browser"
    );
    // Transcript: both turns rendered with roles and markdown.
    assert!(body.contains("<em>probing</em>"));
    assert!(body.contains("a counterpoint"));
    assert!(body.contains("turn--you") && body.contains("turn--foil"));
    // Memory panel: index entry visible.
    assert!(body.contains("[[core-tension]]") && body.contains("The one durable conclusion."));
    // Degraded AI (harness refuses): banner with the Unreachable remedy + disabled compose (D20).
    assert!(body.contains("The foil is offline"));
    assert!(body.contains("ollama serve"), "Unreachable remedy copy");
}

#[tokio::test]
async fn compose_box_is_live_when_ollama_is_available() {
    let mock = spawn(&["llama3.2"], ChatScript::Tokens(vec![])).await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    seed(&vault_dir, IdeaState::InDiscussion, "body\n");

    let (status, body) = get(state, "/idea/sharp-idea").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("hx-post=\"/idea/sharp-idea/chat\""));
    assert!(!body.contains("The foil is offline"));
}

#[tokio::test]
async fn draft_page_has_oob_targets_but_no_oob_fragments() {
    // The full page must render the badge and the (empty) actions container — the anchors the
    // out-of-band swaps replace later — but never the `hx-swap-oob` fragments themselves, which
    // belong only to transcript responses (duplicate-id guard for the
    // transcript_inner vs respond_with_transcript split).
    let mock = spawn(&["llama3.2"], ChatScript::Tokens(vec![])).await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    seed(&vault_dir, IdeaState::Draft, "body\n");

    let (status, body) = get(state, "/idea/sharp-idea").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("id=\"idea-state\""));
    assert!(body.contains("id=\"idea-actions\""), "OOB target exists");
    assert!(!body.contains("hx-swap-oob"), "full pages carry no OOB");
    assert!(
        !body.contains("/idea/sharp-idea/store"),
        "a Draft offers no store control"
    );
}

#[tokio::test]
async fn stored_idea_shows_reopen_panel_not_compose() {
    let (state, vault_dir) = test_state();
    seed(&vault_dir, IdeaState::Stored, "Consolidated statement.\n");

    let (status, body) = get(state, "/idea/sharp-idea").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("hx-post=\"/idea/sharp-idea/reopen\""));
    assert!(
        !body.contains("/idea/sharp-idea/chat"),
        "no compose when Stored"
    );
    assert!(body.contains("state--stored"));
}

#[tokio::test]
async fn missing_idea_is_404() {
    let (state, _vault_dir) = test_state();
    let (status, _) = get(state, "/idea/ghost").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn malformed_slug_is_404_not_500() {
    let (state, _vault_dir) = test_state();
    // Invalid slug charset (space, uppercase) must be answered like a missing idea.
    let (status, _) = get(state.clone(), "/idea/Bad%20Slug").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = get(state, "/idea/%2e%2e").await;
    assert_ne!(status, StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn model_missing_disables_compose_with_pull_hint() {
    // Ollama server up, but the configured model (llama3.2) is not in the tags list.
    let mock = spawn(&["mistral"], ChatScript::Tokens(vec![])).await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    seed(&vault_dir, IdeaState::InDiscussion, "body\n");

    let (status, body) = get(state, "/idea/sharp-idea").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("The foil is offline"));
    assert!(
        body.contains("ollama pull llama3.2"),
        "D20 per-state remedy"
    );
    // No composer is rendered while the model is unavailable — just the note.
    assert!(
        !body.contains("/idea/sharp-idea/chat"),
        "compose box absent when offline"
    );
}

#[tokio::test]
async fn deleting_a_memory_fact_removes_it_and_shrinks_reopen_context() {
    let (state, vault_dir) = test_state();
    seed(&vault_dir, IdeaState::Stored, "A stored idea.\n");
    for (slug, title) in [("keep-me", "Keep me"), ("drop-me", "Drop me")] {
        store::write_memory_fact(
            &vault_dir,
            "sharp-idea",
            &MemoryFact {
                frontmatter: MemoryFactFrontmatter {
                    slug: slug.into(),
                    title: title.into(),
                    tags: vec![],
                    created: Utc.with_ymd_and_hms(2026, 7, 7, 11, 0, 0).unwrap(),
                    links: vec![],
                },
                body: format!("Body of {title}.\n"),
            },
        )
        .unwrap();
    }
    store::rebuild_memory_index(&vault_dir, "sharp-idea").unwrap();

    let (status, body) = post_form(state, "/idea/sharp-idea/memory/drop-me/delete", "").await;
    assert_eq!(status, StatusCode::OK);
    // The re-rendered panel keeps the other fact and drops the deleted one.
    assert!(body.contains("keep-me") && !body.contains("drop-me"));
    // On disk: the fact file is gone and MEMORY.md no longer references it (reopen loads less).
    assert!(!vault_dir.join("sharp-idea/memory/drop-me.md").is_file());
    assert!(vault_dir.join("sharp-idea/memory/keep-me.md").is_file());
    let idx = store::read_memory_index(&vault_dir, "sharp-idea").unwrap();
    assert_eq!(idx.entries.len(), 1);
}
