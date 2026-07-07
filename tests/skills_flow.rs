//! D18 skill-invocation tests against the mock Ollama: context hydration reaches the model,
//! output lands as an assistant turn only after completion, failures append nothing, and the
//! shared semaphore gates the call. No live model.

mod support;

use std::path::Path;
use std::sync::Arc;

use chrono::{TimeZone, Utc};
use idea_vault::ai::budget::ContextBudget;
use idea_vault::ai::OllamaClient;
use idea_vault::concepts::skills::{self, SkillRegistry};
use idea_vault::domain::{Idea, IdeaFrontmatter, IdeaState};
use idea_vault::vault::store;
use support::{refused_url, spawn, ChatScript};
use tokio::sync::Semaphore;

fn seed_idea(vault: &Path, slug: &str) {
    store::write_idea(
        vault,
        &Idea {
            frontmatter: IdeaFrontmatter {
                title: "Test idea".into(),
                slug: slug.into(),
                state: IdeaState::InDiscussion,
                tags: vec![],
                created: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
                updated: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
            },
            body: "A distinctive idea statement.\n".into(),
        },
    )
    .unwrap();
    store::append_conversation(vault, slug, "## user\nkick the tires\n").unwrap();
}

#[tokio::test]
async fn invoke_hydrates_context_and_appends_assistant_turn() {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path(), "i");
    let mock = spawn(
        &["llama3.2"],
        ChatScript::Tokens(vec!["Failure cause one.".into()]),
    )
    .await;
    let client = OllamaClient::new(mock.url.clone(), "llama3.2").unwrap();
    let semaphore = Arc::new(Semaphore::new(2));
    let registry = SkillRegistry::builtin();
    let skill = registry.get("premortem").unwrap();

    let output = skills::invoke(
        &client,
        &semaphore,
        tmp.path(),
        "i",
        skill,
        ContextBudget::new(4096),
    )
    .await
    .unwrap();
    assert_eq!(output, "Failure cause one.");

    // The hydrated {context} actually reached the model: the captured /api/chat request body
    // carries both the skill's template text and the idea body/conversation (D18 + D21).
    let bodies = mock.chat_bodies();
    assert_eq!(bodies.len(), 1);
    assert!(
        bodies[0].contains("failed badly 12 months"),
        "skill template present"
    );
    assert!(
        bodies[0].contains("A distinctive idea statement."),
        "idea body hydrated"
    );
    assert!(
        bodies[0].contains("kick the tires"),
        "recent conversation hydrated"
    );
    assert!(
        !bodies[0].contains("{context}"),
        "slot replaced, not left literal"
    );

    // Output appended as a labelled assistant turn, after the user turn (append-only).
    let convo = store::read_conversation(tmp.path(), "i").unwrap();
    assert_eq!(
        convo,
        "## user\nkick the tires\n## assistant (skill: premortem)\nFailure cause one.\n"
    );

    // Stateless: idea state untouched (D18).
    assert_eq!(
        store::read_idea(tmp.path(), "i").unwrap().frontmatter.state,
        IdeaState::InDiscussion
    );
}

#[tokio::test]
async fn failed_skill_call_appends_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path(), "i");
    let convo_before = store::read_conversation(tmp.path(), "i").unwrap();

    let client = OllamaClient::new(refused_url().await, "llama3.2").unwrap();
    let semaphore = Arc::new(Semaphore::new(1));
    let registry = SkillRegistry::builtin();

    let result = skills::invoke(
        &client,
        &semaphore,
        tmp.path(),
        "i",
        registry.get("devils-advocate").unwrap(),
        ContextBudget::new(4096),
    )
    .await;
    assert!(result.is_err());
    // Persist boundary: a failed call leaves the transcript untouched.
    assert_eq!(
        store::read_conversation(tmp.path(), "i").unwrap(),
        convo_before
    );
}

#[tokio::test]
async fn invoke_waits_on_the_shared_semaphore() {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path(), "i");
    let mock = spawn(&["llama3.2"], ChatScript::Tokens(vec!["ok".into()])).await;
    let client = OllamaClient::new(mock.url.clone(), "llama3.2").unwrap();

    // Hold the only permit: invoke must block until it is released (shared bound, ADR-0006).
    let semaphore = Arc::new(Semaphore::new(1));
    let held = semaphore.clone().acquire_owned().await.unwrap();

    let registry = SkillRegistry::builtin();
    let skill = registry.get("premortem").unwrap().clone();
    let fut = skills::invoke(
        &client,
        &semaphore,
        tmp.path(),
        "i",
        &skill,
        ContextBudget::new(4096),
    );
    tokio::pin!(fut);

    // While the permit is held, the invoke future must not complete.
    let raced = tokio::time::timeout(std::time::Duration::from_millis(100), fut.as_mut()).await;
    assert!(
        raced.is_err(),
        "invoke completed despite exhausted semaphore"
    );

    drop(held);
    let output = tokio::time::timeout(std::time::Duration::from_secs(5), fut)
        .await
        .expect("completes once a permit frees")
        .unwrap();
    assert_eq!(output, "ok");
}
