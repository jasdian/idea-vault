//! ADR-0015 knowledge-extraction tests against the instrumented mock Ollama, including the
//! docs/10 keystone shape: fan out N lenses through K permits, assert max concurrent calls == K;
//! per-lens findings persist as artifacts (unlike the swarm, which discards intermediates);
//! failures degrade per-lens; an all-fail run writes nothing.

mod support;

use std::path::Path;
use std::sync::Arc;

use chrono::{TimeZone, Utc};
use idea_vault::ai::budget::ContextBudget;
use idea_vault::ai::{LlmBackend, OllamaClient};
use idea_vault::concepts::knowledge::{extract_knowledge, KnowledgeOutcome, LENSES};
use idea_vault::concepts::skills::SkillRegistry;
use idea_vault::concepts::ConceptError;
use idea_vault::domain::{ArtifactKind, Idea, IdeaFrontmatter, IdeaState};
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
            body: "Idea under extraction.\n".into(),
        },
    )
    .unwrap();
    store::append_conversation(vault, slug, "## user\nharvest it\n").unwrap();
}

async fn run_extraction(
    mock: &MockOllama,
    vault: &Path,
    k: usize,
    lenses: &[&str],
) -> Result<KnowledgeOutcome, ConceptError> {
    let client = LlmBackend::ollama_only(OllamaClient::new(mock.url.clone(), "llama3.2").unwrap());
    let semaphore = Arc::new(Semaphore::new(k));
    let registry = SkillRegistry::builtin();
    extract_knowledge(
        &client,
        &semaphore,
        &registry,
        vault,
        "i",
        lenses.iter().map(|l| l.to_string()).collect(),
        ContextBudget::new(4096),
        &|_: &str| {},
    )
    .await
}

#[tokio::test]
async fn keystone_max_in_flight_equals_k_and_every_lens_persists_an_artifact() {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path(), "i");
    // Every call holds its response open for 80ms — with N=5 lenses + 1 synthesizer racing
    // through K=2 permits, overlap at exactly K is guaranteed while >K is impossible.
    let mock = spawn(
        &["llama3.2"],
        ChatScript::TokensAfterDelay {
            tokens: vec!["harvested".into()],
            delay_ms: 80,
        },
    )
    .await;

    const K: usize = 2;
    let outcome = run_extraction(&mock, tmp.path(), K, &LENSES).await.unwrap();

    assert_eq!(mock.chat_bodies().len(), 6, "5 lenses + 1 synthesizer");
    assert!(
        mock.max_in_flight() <= K,
        "bound violated: {} > K={K}",
        mock.max_in_flight()
    );
    assert_eq!(mock.max_in_flight(), K, "fan-out should saturate the bound");

    // Every lens persisted one artifact, plus the synthesis — with the run stamp as the shared
    // stem prefix and per-lens frontmatter.
    assert_eq!(outcome.findings.len(), 5);
    let artifacts = store::read_artifacts(tmp.path(), "i").unwrap();
    assert_eq!(artifacts.len(), 6);
    for (lens, finding) in LENSES.iter().zip(&outcome.findings) {
        assert_eq!(&finding.lens, lens);
        assert!(finding.file_slug.starts_with(&outcome.run_stamp));
        let artifact = store::read_artifact(tmp.path(), "i", &finding.file_slug).unwrap();
        assert_eq!(artifact.frontmatter.kind, ArtifactKind::Finding);
        assert_eq!(artifact.frontmatter.lens.as_deref(), Some(*lens));
        assert_eq!(artifact.body, "harvested");
    }
    let synthesis_slug = outcome.synthesis_slug.expect("synthesis persisted");
    let synthesis = store::read_artifact(tmp.path(), "i", &synthesis_slug).unwrap();
    assert_eq!(synthesis.frontmatter.kind, ArtifactKind::Synthesis);
    assert_eq!(synthesis.frontmatter.lens, None);

    // The synthesis is one conversation turn; the per-lens findings are not in the transcript.
    let convo = store::read_conversation(tmp.path(), "i").unwrap();
    assert!(convo.contains("## assistant (knowledge)\nharvested\n"));
    assert_eq!(convo.matches("## assistant").count(), 1);
}

#[tokio::test]
async fn failed_lens_is_skipped_and_the_rest_persist() {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path(), "i");
    // K=1 serializes the calls (tokio's semaphore is FIFO), so the script sequence maps
    // deterministically: lens1 ok, lens2 dies mid-stream, synthesizer ok.
    let mock = spawn_sequence(
        &["llama3.2"],
        vec![
            ChatScript::Tokens(vec!["- decided X".into()]),
            ChatScript::EofAfter(vec!["half a".into()]),
            ChatScript::Tokens(vec!["converged".into()]),
        ],
    )
    .await;

    let outcome = run_extraction(
        &mock,
        tmp.path(),
        1,
        &["extract-key-decisions", "extract-open-questions"],
    )
    .await
    .unwrap();

    // Only the surviving lens got an artifact; the dead lens's partial output is nowhere.
    assert_eq!(outcome.findings.len(), 1);
    assert_eq!(outcome.findings[0].lens, "extract-key-decisions");
    let artifacts = store::read_artifacts(tmp.path(), "i").unwrap();
    assert_eq!(artifacts.len(), 2, "one finding + one synthesis");
    assert!(artifacts.iter().all(|a| !a.body.contains("half a")));

    let convo = store::read_conversation(tmp.path(), "i").unwrap();
    assert!(convo.contains("## assistant (knowledge)\nconverged\n"));
    assert!(!convo.contains("- decided X"), "findings live in artifacts");
}

#[tokio::test]
async fn all_lenses_failed_errors_and_persists_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path(), "i");
    let convo_before = store::read_conversation(tmp.path(), "i").unwrap();
    let mock = spawn(&["llama3.2"], ChatScript::EofAfter(vec![])).await;

    let err = run_extraction(
        &mock,
        tmp.path(),
        2,
        &["extract-key-decisions", "extract-open-questions"],
    )
    .await
    .unwrap_err();
    assert!(matches!(err, ConceptError::NothingToSynthesize));
    assert!(store::read_artifacts(tmp.path(), "i").unwrap().is_empty());
    assert!(!tmp.path().join("i/artifacts").exists());
    assert_eq!(
        store::read_conversation(tmp.path(), "i").unwrap(),
        convo_before
    );
}

#[tokio::test]
async fn empty_synthesis_keeps_findings_and_skips_the_turn() {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path(), "i");
    let convo_before = store::read_conversation(tmp.path(), "i").unwrap();
    // Lens returns a finding; the synthesizer comes back Ok-but-empty.
    let mock = spawn_sequence(
        &["llama3.2"],
        vec![
            ChatScript::Tokens(vec!["- decided X".into()]),
            ChatScript::Tokens(vec![]),
        ],
    )
    .await;

    let outcome = run_extraction(&mock, tmp.path(), 1, &["extract-key-decisions"])
        .await
        .unwrap();

    // Findings are the primary deliverable: persisted even without a synthesis (ADR-0015).
    assert_eq!(outcome.findings.len(), 1);
    assert!(outcome.synthesis.is_empty());
    assert_eq!(outcome.synthesis_slug, None);
    let artifacts = store::read_artifacts(tmp.path(), "i").unwrap();
    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts[0].frontmatter.kind, ArtifactKind::Finding);
    // No empty turn is appended (D24: surface, not swallow).
    assert_eq!(
        store::read_conversation(tmp.path(), "i").unwrap(),
        convo_before
    );
}

#[tokio::test]
async fn unknown_lens_fails_fast_before_any_model_call() {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path(), "i");
    let mock = spawn(&["llama3.2"], ChatScript::Tokens(vec!["x".into()])).await;

    let err = run_extraction(
        &mock,
        tmp.path(),
        2,
        &["extract-key-decisions", "not-a-lens"],
    )
    .await
    .unwrap_err();
    assert!(matches!(err, ConceptError::UnknownSkill(name) if name == "not-a-lens"));
    assert!(mock.chat_bodies().is_empty(), "no AI call was made");
    assert!(store::read_artifacts(tmp.path(), "i").unwrap().is_empty());
}

#[tokio::test]
async fn repeated_runs_never_overwrite_earlier_artifacts() {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path(), "i");
    let mock = spawn(&["llama3.2"], ChatScript::Tokens(vec!["harvested".into()])).await;

    let first = run_extraction(&mock, tmp.path(), 1, &["extract-key-decisions"])
        .await
        .unwrap();
    let second = run_extraction(&mock, tmp.path(), 1, &["extract-key-decisions"])
        .await
        .unwrap();

    // Same-second runs disambiguate (`-2` suffix) instead of clobbering (D22 predicate over
    // both extensions).
    assert_ne!(first.findings[0].file_slug, second.findings[0].file_slug);
    assert_ne!(first.synthesis_slug, second.synthesis_slug);
    let artifacts = store::read_artifacts(tmp.path(), "i").unwrap();
    assert_eq!(artifacts.len(), 4, "2 runs × (finding + synthesis)");
    let mut slugs: Vec<_> = artifacts
        .iter()
        .map(|a| a.frontmatter.slug.clone())
        .collect();
    slugs.dedup();
    assert_eq!(slugs.len(), 4, "all stems unique: {slugs:?}");
}
