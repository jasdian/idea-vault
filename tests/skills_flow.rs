//! D18 skill-invocation tests against the mock Ollama: context hydration reaches the model,
//! output lands as an assistant turn only after completion, failures append nothing, and the
//! shared semaphore gates the call. No live model.

mod support;

use std::path::Path;
use std::sync::Arc;

use chrono::{TimeZone, Utc};
use idea_vault::ai::budget::ContextBudget;
use idea_vault::ai::{LlmBackend, OllamaClient};
use idea_vault::concepts::skills::{self, SkillRegistry};
use idea_vault::domain::{Idea, IdeaFrontmatter, IdeaState};
use idea_vault::vault::store;
use support::{refused_url, spawn, spawn_sequence, ChatScript};
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
                sources: vec![],
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
        ChatScript::Tokens(vec!["1. Failure cause one.".into()]),
    )
    .await;
    let client = LlmBackend::ollama_only(OllamaClient::new(mock.url.clone(), "llama3.2").unwrap());
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
        &|_: &str| {},
    )
    .await
    .unwrap();
    assert_eq!(output, "1. Failure cause one.");

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
        "## user\nkick the tires\n## assistant (skill: premortem)\n1. Failure cause one.\n"
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

    let client =
        LlmBackend::ollama_only(OllamaClient::new(refused_url().await, "llama3.2").unwrap());
    let semaphore = Arc::new(Semaphore::new(1));
    let registry = SkillRegistry::builtin();

    let result = skills::invoke(
        &client,
        &semaphore,
        tmp.path(),
        "i",
        registry.get("devils-advocate").unwrap(),
        ContextBudget::new(4096),
        &|_: &str| {},
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
    let client = LlmBackend::ollama_only(OllamaClient::new(mock.url.clone(), "llama3.2").unwrap());

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
        &|_: &str| {},
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

/// Run `skill` once against a mock answering `answers` in order; returns the output, the request
/// bodies the mock saw, and the persisted transcript.
async fn invoke_with(skill: &str, answers: &[&str]) -> (String, Vec<String>, String) {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path(), "i");
    let mock = spawn_sequence(
        &["llama3.2"],
        answers
            .iter()
            .map(|a| ChatScript::Tokens(vec![a.to_string()]))
            .collect(),
    )
    .await;
    let client = LlmBackend::ollama_only(OllamaClient::new(mock.url.clone(), "llama3.2").unwrap());
    let semaphore = Arc::new(Semaphore::new(1));
    let registry = SkillRegistry::builtin();
    let output = skills::invoke(
        &client,
        &semaphore,
        tmp.path(),
        "i",
        registry.get(skill).unwrap(),
        ContextBudget::new(4096),
        &|_: &str| {},
    )
    .await
    .unwrap();
    let convo = store::read_conversation(tmp.path(), "i").unwrap();
    (output, mock.chat_bodies(), convo)
}

#[tokio::test]
async fn an_off_contract_answer_is_retried_once_with_the_violation_read_back() {
    let (output, bodies, convo) = invoke_with(
        "premortem",
        &[
            "It could fail for many reasons.",
            "Sure:\n1. Nobody pays.\n2. Churn.",
        ],
    )
    .await;
    assert_eq!(bodies.len(), 2, "exactly one retry");
    assert!(!bodies[0].contains("was rejected because"));
    assert!(bodies[1].contains("was rejected because") && bodies[1].contains("numbered list"));
    assert!(
        !bodies[1].contains("many reasons"),
        "the failed answer is not resent"
    );
    assert_eq!(output, "1. Nobody pays.\n2. Churn.", "preamble stripped");
    assert!(convo.ends_with("## assistant (skill: premortem)\n1. Nobody pays.\n2. Churn.\n"));
}

#[tokio::test]
async fn a_second_violation_stops_at_two_calls_and_keeps_the_answer() {
    let (output, bodies, convo) =
        invoke_with("premortem", &["no list", "still no list", "never asked"]).await;
    assert_eq!(bodies.len(), 2, "the retry cap is one");
    assert_eq!(output, "still no list");
    assert!(convo.ends_with("still no list\n"));
}

#[tokio::test]
async fn an_on_contract_answer_is_never_retried() {
    let (_, bodies, _) = invoke_with("devils-advocate", &["Plain prose is fine here."]).await;
    assert_eq!(bodies.len(), 1);
}

#[tokio::test]
async fn build_prompt_persists_only_the_fenced_block() {
    let (output, bodies, convo) = invoke_with(
        "build-prompt",
        &["Here is your prompt:\n```markdown\n# Build X\n```bash\ncargo test\n```\n```\nGood luck!"],
    )
    .await;
    assert_eq!(bodies.len(), 1);
    assert_eq!(
        output,
        "```markdown\n# Build X\n```bash\ncargo test\n```\n```"
    );
    assert!(!convo.contains("Good luck") && !convo.contains("Here is your prompt"));
}

#[tokio::test]
async fn a_skill_invocation_samples_at_its_role_profile() {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path(), "i");
    let mock = spawn(
        &["llama3.2"],
        ChatScript::Tokens(vec!["```\nbuild it\n```".into()]),
    )
    .await;
    let client = LlmBackend::ollama_only(OllamaClient::new(mock.url.clone(), "llama3.2").unwrap());
    let mut s = client.settings();
    s.role_tuning = true;
    s.role_profiles = idea_vault::concepts::agents::default_role_profiles();
    client.set_settings(s);
    let registry = SkillRegistry::builtin();
    let skill = registry.get("build-prompt").unwrap();

    skills::invoke(
        &client,
        &Semaphore::new(1),
        tmp.path(),
        "i",
        skill,
        ContextBudget::new(4096),
        &|_: &str| {},
    )
    .await
    .unwrap();

    let body: serde_json::Value = serde_json::from_str(&mock.chat_bodies()[0]).unwrap();
    let t = body["options"]["temperature"].as_f64().unwrap();
    assert!((t - 0.5).abs() < 1e-6, "synthesizer skill sampled at {t}");
}
