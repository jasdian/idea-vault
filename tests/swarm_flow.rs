//! D14/ADR-0006 swarm tests against the instrumented mock Ollama, including the docs/10
//! keystone: fan out N ≫ K agents, assert max concurrent Ollama calls == K and all N complete;
//! failed agents null out and the judge skips them; only the synthesis is persisted.

mod support;

use std::path::Path;
use std::sync::Arc;

use chrono::{TimeZone, Utc};
use idea_vault::ai::budget::ContextBudget;
use idea_vault::ai::{LlmBackend, OllamaClient};
use idea_vault::concepts::skills::SkillRegistry;
use idea_vault::concepts::{swarm::swarm, ConceptError};
use idea_vault::domain::{Idea, IdeaFrontmatter, IdeaState};
use idea_vault::vault::store;
use support::{spawn, spawn_sequence, ChatScript, MockOllama};
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
            body: "Idea under swarm attack.\n".into(),
        },
    )
    .unwrap();
    store::append_conversation(vault, slug, "## user\nswarm it\n").unwrap();
}

async fn run_swarm(
    mock: &MockOllama,
    vault: &Path,
    k: usize,
    angles: &[&str],
) -> Result<idea_vault::concepts::swarm::SwarmOutcome, ConceptError> {
    let client = LlmBackend::ollama_only(OllamaClient::new(mock.url.clone(), "llama3.2").unwrap());
    let semaphore = Arc::new(Semaphore::new(k));
    let registry = SkillRegistry::builtin();
    swarm(
        &client,
        &semaphore,
        &registry,
        vault,
        "i",
        angles.iter().map(|a| a.to_string()).collect(),
        ContextBudget::new(4096),
    )
    .await
}

#[tokio::test]
async fn keystone_max_in_flight_equals_k_and_all_n_complete() {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path(), "i");
    // Every call holds its response open for 80ms — with N=6 agents + 1 synthesizer racing
    // through K=2 permits, overlap at exactly K is guaranteed while >K is impossible.
    let mock = spawn(
        &["llama3.2"],
        ChatScript::TokensAfterDelay {
            tokens: vec!["finding".into()],
            delay_ms: 80,
        },
    )
    .await;

    const K: usize = 2;
    let angles = [
        "premortem",
        "cheapest-disproof",
        "devils-advocate",
        "premortem",
        "cheapest-disproof",
        "devils-advocate",
    ];
    let outcome = run_swarm(&mock, tmp.path(), K, &angles).await.unwrap();

    // All N complete (queued, not dropped) …
    assert_eq!(outcome.agent_results.len(), 6);
    assert!(outcome.agent_results.iter().all(Option::is_some));
    assert_eq!(mock.chat_bodies().len(), 7, "6 agents + 1 synthesizer");
    // … while in-flight calls never exceeded K, and genuinely reached K (real parallelism).
    assert!(
        mock.max_in_flight() <= K,
        "bound violated: {} > K={K}",
        mock.max_in_flight()
    );
    assert_eq!(mock.max_in_flight(), K, "fan-out should saturate the bound");
}

#[tokio::test]
async fn failed_agent_is_nulled_and_judge_skips_it() {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path(), "i");
    // K=1 serializes the calls (tokio's semaphore is FIFO), so the script sequence maps
    // deterministically: agent1 ok, agent2 dies mid-stream, synthesizer ok.
    let mock = spawn_sequence(
        &["llama3.2"],
        vec![
            ChatScript::Tokens(vec!["finding A".into()]),
            ChatScript::EofAfter(vec!["half a".into()]),
            ChatScript::Tokens(vec!["converged view".into()]),
        ],
    )
    .await;

    let outcome = run_swarm(&mock, tmp.path(), 1, &["premortem", "cheapest-disproof"])
        .await
        .unwrap();

    assert!(outcome.agent_results[0].is_some());
    assert!(outcome.agent_results[1].is_none(), "failed agent nulled");
    assert_eq!(outcome.synthesis, "converged view");

    // The synthesizer saw the surviving finding, not the dead agent's partial output.
    let bodies = mock.chat_bodies();
    assert!(bodies[2].contains("finding A"));
    assert!(!bodies[2].contains("half a"));

    // Only the synthesis is persisted, as one swarm turn; intermediate outputs are not.
    let convo = store::read_conversation(tmp.path(), "i").unwrap();
    assert!(convo.contains("## assistant (swarm)\nconverged view\n"));
    assert!(
        !convo.contains("finding A"),
        "intermediate output not persisted"
    );
}

#[tokio::test]
async fn all_agents_failed_errors_and_persists_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path(), "i");
    let convo_before = store::read_conversation(tmp.path(), "i").unwrap();
    let mock = spawn(&["llama3.2"], ChatScript::EofAfter(vec![])).await;

    let err = run_swarm(&mock, tmp.path(), 2, &["premortem", "devils-advocate"])
        .await
        .unwrap_err();
    assert!(matches!(err, ConceptError::NothingToSynthesize));
    assert_eq!(
        store::read_conversation(tmp.path(), "i").unwrap(),
        convo_before
    );
}

#[tokio::test]
async fn unknown_angle_fails_fast_before_any_model_call() {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path(), "i");
    let mock = spawn(&["llama3.2"], ChatScript::Tokens(vec!["x".into()])).await;

    let err = run_swarm(&mock, tmp.path(), 2, &["premortem", "not-a-skill"])
        .await
        .unwrap_err();
    assert!(matches!(err, ConceptError::UnknownSkill(name) if name == "not-a-skill"));
    assert!(mock.chat_bodies().is_empty(), "no AI call was made");
}
