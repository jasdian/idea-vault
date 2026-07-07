//! D19 workflow tests against the mock Ollama: deterministic control flow (fixed step order),
//! failed step nulled + judge skips, only the final synthesis persisted. No live model.

mod support;

use std::path::Path;
use std::sync::Arc;

use chrono::{TimeZone, Utc};
use idea_vault::ai::budget::ContextBudget;
use idea_vault::ai::{LlmBackend, OllamaClient};
use idea_vault::concepts::skills::SkillRegistry;
use idea_vault::concepts::workflows::run_workflow;
use idea_vault::concepts::ConceptError;
use idea_vault::domain::{Idea, IdeaFrontmatter, IdeaState};
use idea_vault::vault::store;
use support::{spawn, spawn_sequence, ChatScript};
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
            body: "Idea under workflow.\n".into(),
        },
    )
    .unwrap();
    store::append_conversation(vault, slug, "## user\nrun the pipeline\n").unwrap();
}

fn tokens(text: &str) -> ChatScript {
    ChatScript::Tokens(vec![text.to_string()])
}

#[tokio::test]
async fn interrogate_runs_the_fixed_dag_in_order_and_persists_only_the_synthesis() {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path(), "i");
    // K=1 serializes the fan-out (FIFO semaphore), so call order == step order: the run is
    // deterministic and the captured bodies prove the fixed DAG (D19).
    let mock = spawn_sequence(
        &["llama3.2"],
        vec![
            tokens("premortem finding"),
            tokens("disproof finding"),
            tokens("advocate finding"),
            tokens("research notes"),
            tokens("one converged position"),
        ],
    )
    .await;
    let client = LlmBackend::ollama_only(OllamaClient::new(mock.url.clone(), "llama3.2").unwrap());
    let semaphore = Arc::new(Semaphore::new(1));
    let registry = SkillRegistry::builtin();

    let outcome = run_workflow(
        &client,
        &semaphore,
        &registry,
        tmp.path(),
        "i",
        "interrogate",
        ContextBudget::new(4096),
    )
    .await
    .unwrap();

    assert_eq!(outcome.workflow, "interrogate");
    assert_eq!(outcome.synthesis, "one converged position");
    assert_eq!(outcome.step_results.len(), 4);
    assert!(outcome.step_results.iter().all(Option::is_some));

    // Deterministic control flow: the captured request order matches the fixed step list.
    let bodies = mock.chat_bodies();
    assert_eq!(bodies.len(), 5, "4 steps + 1 synthesizer");
    assert!(bodies[0].contains("You are the Critic") && bodies[0].contains("failed badly"));
    assert!(bodies[1].contains("You are the Critic") && bodies[1].contains("cheapest, fastest"));
    assert!(bodies[2].contains("You are the Researcher") && bodies[2].contains("constraints"));
    assert!(bodies[3].contains("You are the Critic") && bodies[3].contains("second-order"));
    assert!(bodies[4].contains("You are the Synthesizer"));
    assert!(bodies[4].contains("premortem finding") && bodies[4].contains("research notes"));

    // Persist boundary: exactly one new turn, the synthesis; intermediates never land.
    let convo = store::read_conversation(tmp.path(), "i").unwrap();
    assert!(convo.contains("## assistant (workflow: interrogate)\none converged position\n"));
    assert!(!convo.contains("premortem finding"));
    assert_eq!(convo.matches("## assistant").count(), 1);
}

#[tokio::test]
async fn failed_step_is_skipped_and_workflow_degrades() {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path(), "i");
    // Step 2 dies mid-stream; the rest proceed; synthesizer never sees the dead step's text.
    let mock = spawn_sequence(
        &["llama3.2"],
        vec![
            tokens("finding one"),
            ChatScript::EofAfter(vec!["partial".into()]),
            tokens("finding three"),
            tokens("notes"),
            tokens("converged anyway"),
        ],
    )
    .await;
    let client = LlmBackend::ollama_only(OllamaClient::new(mock.url.clone(), "llama3.2").unwrap());
    let semaphore = Arc::new(Semaphore::new(1));
    let registry = SkillRegistry::builtin();

    let outcome = run_workflow(
        &client,
        &semaphore,
        &registry,
        tmp.path(),
        "i",
        "interrogate",
        ContextBudget::new(4096),
    )
    .await
    .unwrap();

    assert!(outcome.step_results[0].is_some());
    assert!(outcome.step_results[1].is_none(), "failed step nulled");
    assert_eq!(outcome.synthesis, "converged anyway");
    let bodies = mock.chat_bodies();
    assert!(
        !bodies[4].contains("partial"),
        "dead step kept from judge/synth"
    );

    // Persist boundary holds through a failure: exactly one new turn, no intermediates.
    let convo = store::read_conversation(tmp.path(), "i").unwrap();
    assert_eq!(convo.matches("## assistant").count(), 1);
    assert!(convo.contains("## assistant (workflow: interrogate)\nconverged anyway\n"));
    assert!(!convo.contains("finding one") && !convo.contains("partial"));
}

#[tokio::test]
async fn all_steps_failed_errors_and_persists_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path(), "i");
    let convo_before = store::read_conversation(tmp.path(), "i").unwrap();
    let mock = spawn(&["llama3.2"], ChatScript::EofAfter(vec![])).await;
    let client = LlmBackend::ollama_only(OllamaClient::new(mock.url.clone(), "llama3.2").unwrap());
    let semaphore = Arc::new(Semaphore::new(2));
    let registry = SkillRegistry::builtin();

    let err = run_workflow(
        &client,
        &semaphore,
        &registry,
        tmp.path(),
        "i",
        "interrogate",
        ContextBudget::new(4096),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, ConceptError::NothingToSynthesize));
    assert_eq!(
        store::read_conversation(tmp.path(), "i").unwrap(),
        convo_before
    );
}

#[tokio::test]
async fn unknown_workflow_fails_fast_with_no_ai_calls() {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path(), "i");
    let mock = spawn(&["llama3.2"], tokens("x")).await;
    let client = LlmBackend::ollama_only(OllamaClient::new(mock.url.clone(), "llama3.2").unwrap());
    let semaphore = Arc::new(Semaphore::new(1));
    let registry = SkillRegistry::builtin();

    let err = run_workflow(
        &client,
        &semaphore,
        &registry,
        tmp.path(),
        "i",
        "nope",
        ContextBudget::new(4096),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, ConceptError::UnknownWorkflow(name) if name == "nope"));
    assert!(mock.chat_bodies().is_empty());
}
