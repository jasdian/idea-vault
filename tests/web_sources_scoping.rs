//! Per-turn source scoping through the web routes (ADR-0021): a chat turn on an idea with an
//! attached, registered source runs the Ollama tool loop — the deterministic note rides the
//! first message, the `source_*` leaves are offered, and the reply still lands end-to-end; a
//! stale attach list degrades to a plain unscoped turn instead of failing; and the per-idea
//! meter counts the source-schema bytes the next turn will actually carry.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers outside #[test] fns; HTC-6 binds shipping code"
)]

mod support;

use axum::http::StatusCode;
use chrono::{TimeZone, Utc};
use idea_vault::app::AppState;
use idea_vault::domain::{Idea, IdeaFrontmatter, IdeaState};
use idea_vault::sources::SourceConfig;
use idea_vault::vault::store;
use support::web::{get, post_form, test_state, test_state_with_ollama};
use support::{spawn, ChatScript};

fn seed(vault: &std::path::Path, slug: &str, sources: Vec<String>) {
    store::write_idea(
        vault,
        &Idea {
            frontmatter: IdeaFrontmatter {
                title: "Sourced".into(),
                slug: slug.into(),
                state: IdeaState::InDiscussion,
                tags: vec![],
                sources,
                created: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
                updated: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
                extra: Default::default(),
            },
            body: "The idea body.\n".into(),
        },
    )
    .unwrap();
}

/// Register `name` in the state's source registry, rooted at a real tempdir with one file in it.
/// The tempdir is leaked for the process lifetime (like the harness's own) so
/// `resolve_attached` can canonicalize the root for the whole test.
fn register_source(state: &AppState, name: &str) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("note.md"), "reference material\n").unwrap();
    state
        .sources
        .add(SourceConfig {
            name: idea_vault::domain::Name::try_from(name).unwrap(),
            host_path: dir.path().to_path_buf(),
        })
        .unwrap();
    std::mem::forget(dir);
}

#[tokio::test]
async fn chat_turn_with_attached_source_runs_the_tool_loop_end_to_end() {
    let mock = spawn(
        &["llama3.2"],
        ChatScript::Tokens(vec!["Grounded".into(), " reply".into()]),
    )
    .await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    register_source(&state, "refs");
    seed(&vault_dir, "sourced", vec!["refs".into()]);

    let (status, body) = post_form(
        state.clone(),
        "/idea/sourced/chat",
        "message=use%20my%20notes",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("use my notes"), "user turn shown immediately");

    // The reply lands via the background job, exactly like an unscoped turn.
    let final_body =
        support::web::poll_until(state, "/idea/sourced/pending", "Grounded reply").await;
    assert!(final_body.contains("turn--foil"));
    // The poll partial's meter is scoped too: the source schemas show as the tools term.
    assert!(
        final_body.contains("KB tools"),
        "scoped meter counts source-schema bytes: {final_body}"
    );

    // Persisted like any other turn.
    let convo = store::read_conversation(&vault_dir, "sourced").unwrap();
    assert!(convo.contains("## assistant\nGrounded reply"));

    // The request that reached the model is well-formed: a non-streaming tool round whose first
    // message carries the deterministic note (prepended, not replacing the prompt) and whose
    // tools array offers the three source leaves.
    let bodies = mock.chat_bodies();
    let raw = bodies
        .iter()
        .find(|b| b.contains("source_grep"))
        .expect("a tool-loop request reached the mock");
    let parsed: serde_json::Value = serde_json::from_str(raw).expect("well-formed JSON body");
    assert_eq!(parsed["stream"], false);
    let first = parsed["messages"][0]["content"].as_str().unwrap();
    assert!(
        first.contains("Attached reference sources for this idea: refs"),
        "note prepended: {first}"
    );
    assert!(first.contains("use my notes"), "prompt intact: {first}");
    let names: Vec<&str> = parsed["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t.pointer("/function/name")?.as_str())
        .collect();
    for tool in ["source_list", "source_grep", "source_read"] {
        assert!(names.contains(&tool), "{tool} offered; got {names:?}");
    }
}

#[tokio::test]
async fn stale_attached_source_degrades_to_a_plain_unscoped_turn() {
    // "ghost" is not in the registry: resolve_attached warn-drops it, so the turn must run
    // exactly like an unscoped one — streaming path, no note, no source tools — never fail.
    let mock = spawn(
        &["llama3.2"],
        ChatScript::Tokens(vec!["Plain reply".into()]),
    )
    .await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    seed(&vault_dir, "sourced", vec!["ghost".into()]);

    let (status, _) = post_form(state.clone(), "/idea/sourced/chat", "message=hello").await;
    assert_eq!(status, StatusCode::OK);
    support::web::poll_until(state, "/idea/sourced/pending", "Plain reply").await;

    let bodies = mock.chat_bodies();
    assert!(!bodies.is_empty());
    for body in &bodies {
        assert!(
            !body.contains("Attached reference sources"),
            "no note on a degraded turn: {body}"
        );
        assert!(
            !body.contains("source_grep"),
            "no source tools on a degraded turn: {body}"
        );
    }
}

#[tokio::test]
async fn per_idea_meter_counts_source_schema_bytes_only_where_attached() {
    // No model needed — the meter is rendered from the scoped backend, not a call.
    let (state, vault_dir) = test_state();
    register_source(&state, "refs");
    seed(&vault_dir, "sourced", vec!["refs".into()]);
    seed(&vault_dir, "plain", vec![]);

    // The idea page and the history view both carry the scoped tools term…
    let (status, body) = get(state.clone(), "/idea/sourced").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("KB tools"), "idea page meter scoped: {body}");
    let (status, body) = get(state.clone(), "/idea/sourced/history").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("KB tools"), "history meter scoped: {body}");

    // …and an idea with no attach list keeps the source-free meter.
    let (status, body) = get(state, "/idea/plain/history").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains("KB tools"), "unscoped idea stays clean");
}
