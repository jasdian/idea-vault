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
                sources: vec![],
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
    run_swarm_audited(mock, vault, k, angles, false).await
}

async fn run_swarm_audited(
    mock: &MockOllama,
    vault: &Path,
    k: usize,
    angles: &[&str],
    audit: bool,
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
        audit,
        &|_| String::new(),
        &|_: &str| {},
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
    let outcome = run_swarm_audited(&mock, tmp.path(), K, &angles, true)
        .await
        .unwrap();

    // All N complete (queued, not dropped) …
    assert_eq!(outcome.agent_results.len(), 6);
    assert!(outcome.agent_results.iter().all(Option::is_some));
    assert_eq!(
        mock.chat_bodies().len(),
        9,
        "6 agents + 1 auditor + its one re-ask (\"finding\" is no verdict) + 1 synthesizer"
    );
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
    assert!(convo.contains("## assistant (swarm: premortem, cheapest-disproof)\nconverged view\n"));
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

#[tokio::test]
async fn the_audit_judges_findings_before_synthesis_and_keeps_refuted_ones_visible() {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path(), "i");
    // K=1 serializes: premortem agent, constraints agent, auditor, synthesizer.
    let mock = spawn_sequence(
        &["llama3.2"],
        vec![
            ChatScript::Tokens(vec!["1. Nobody pays for it\n2. Churn kills it".into()]),
            ChatScript::Tokens(vec!["- Needs a licence".into()]),
            ChatScript::Tokens(vec![
                "F1: REFUTED — three pilots already paid\nF2: UNCERTAIN — depends on region\nF3: CONFIRMED — churn is unaddressed".into(),
            ]),
            ChatScript::Tokens(vec!["converged view".into()]),
        ],
    )
    .await;

    let outcome = run_swarm_audited(&mock, tmp.path(), 1, &["premortem", "constraints"], true)
        .await
        .unwrap();

    let bodies = mock.chat_bodies();
    assert_eq!(bodies.len(), 4);
    assert!(
        bodies[1].contains("You are the Researcher"),
        "constraints runs under its own role"
    );
    // Factored: the auditor sees numbered findings, not the critics' personas or framing.
    // Items interleave across lenses, so each lens's top finding comes first.
    assert!(bodies[2].contains("You are the Auditor"));
    assert!(bodies[2].contains("F1: Nobody pays for it"));
    assert!(bodies[2].contains("F2: Needs a licence"));
    assert!(bodies[2].contains("F3: Churn kills it"));
    assert!(!bodies[2].contains("You are the Critic"));
    // The synthesizer sees the idea, each finding's lens, and its verdict.
    assert!(bodies[3].contains("Idea under swarm attack."));
    assert!(bodies[3].contains("(premortem · critic) [REFUTED — three pilots already paid]"));
    assert!(bodies[3].contains("(constraints · researcher) [UNCERTAIN"));

    let report = outcome.audit.expect("audit ran");
    assert_eq!(report.answered, 3);
    let convo = store::read_conversation(tmp.path(), "i").unwrap();
    assert!(convo.contains("converged view"));
    assert!(convo.contains("_Audit: 1 confirmed · 1 uncertain · 1 refuted_"));
    assert!(convo.contains("### Disproven objections"));
    assert!(
        convo.contains("~~Nobody pays for it~~ (premortem · critic) — three pilots already paid")
    );
}

#[tokio::test]
async fn a_garbled_audit_degrades_to_unverified_and_still_synthesizes() {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path(), "i");
    let mock = spawn_sequence(
        &["llama3.2"],
        vec![
            ChatScript::Tokens(vec!["1. Nobody pays".into()]),
            ChatScript::Tokens(vec!["These all look like great points!".into()]),
            // The one targeted re-ask (ADR-0023 amendment) is garbled too.
            ChatScript::Tokens(vec!["Still great points!".into()]),
            ChatScript::Tokens(vec!["converged anyway".into()]),
        ],
    )
    .await;
    let outcome = run_swarm_audited(&mock, tmp.path(), 1, &["premortem"], true)
        .await
        .unwrap();
    assert!(outcome.audit.unwrap().failed);
    let convo = store::read_conversation(tmp.path(), "i").unwrap();
    assert!(convo.contains("converged anyway"));
    assert!(convo.contains("findings above are unverified"));
    assert!(
        !mock.chat_bodies()[3].contains("auditor's verdict"),
        "no verdict guidance when the audit failed"
    );
}

#[tokio::test]
async fn with_the_audit_off_there_is_no_auditor_call() {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path(), "i");
    let mock = spawn(&["llama3.2"], ChatScript::Tokens(vec!["1. x".into()])).await;
    let outcome = run_swarm(&mock, tmp.path(), 1, &["premortem", "devils-advocate"])
        .await
        .unwrap();
    assert!(outcome.audit.is_none());
    assert_eq!(mock.chat_bodies().len(), 3, "2 agents + synthesizer");
    assert!(!mock
        .chat_bodies()
        .iter()
        .any(|b| b.contains("You are the Auditor")));
}

#[tokio::test]
async fn related_block_reaches_every_angle_once_computed() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path(), "i");
    let mock = spawn(&["llama3.2"], ChatScript::Tokens(vec!["1. x".into()])).await;
    let client = LlmBackend::ollama_only(OllamaClient::new(mock.url.clone(), "llama3.2").unwrap());
    let registry = SkillRegistry::builtin();
    let calls = AtomicUsize::new(0);
    let provider = |_: usize| {
        calls.fetch_add(1, Ordering::SeqCst);
        "## Related ideas elsewhere in the vault\n- RELATED-MARKER (`other`): link\n\n".to_string()
    };
    let angles = ["premortem", "devils-advocate", "constraints"];

    swarm(
        &client,
        &Semaphore::new(2),
        &registry,
        tmp.path(),
        "i",
        angles.iter().map(|a| a.to_string()).collect(),
        ContextBudget::new(4096),
        true,
        &provider,
        &|_: &str| {},
    )
    .await
    .unwrap();

    assert_eq!(calls.load(Ordering::SeqCst), 1, "computed once per fan-out");
    let bodies = mock.chat_bodies();
    assert_eq!(
        bodies.len(),
        angles.len() + 3,
        "angles + auditor + its re-ask (\"1. x\" is no verdict) + synthesizer"
    );
    let (auditor, rest): (Vec<&String>, Vec<&String>) = bodies
        .iter()
        .partition(|b| b.contains("You are the Auditor"));
    assert_eq!(auditor.len(), 2);
    assert!(
        auditor.iter().all(|a| !a.contains("RELATED-MARKER")),
        "the audit prompt carries no related block; agent answers here never quote it"
    );
    let agents: Vec<&&String> = rest
        .iter()
        .filter(|b| !b.contains("You are the Synthesizer"))
        .collect();
    assert_eq!(agents.len(), angles.len());
    for body in agents {
        let block = body.find("RELATED-MARKER").expect("block in every angle");
        let own = body.find("Idea under swarm attack.").unwrap();
        assert!(block < own, "block precedes the own context");
    }
}

#[tokio::test]
async fn audit_cap_swarm_turn_counts_the_findings_left_out() {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path(), "i");
    let items = |prefix: &str| {
        (0..12)
            .map(|i| format!("- {prefix}{i}a {prefix}{i}b {prefix}{i}c"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let mock = spawn_sequence(
        &["llama3.2"],
        vec![
            ChatScript::Tokens(vec![items("p")]),
            ChatScript::Tokens(vec![items("q")]),
            ChatScript::Tokens(vec!["F1: CONFIRMED — ok".into()]),
            // The partial audit's one re-ask (ADR-0023 amendment) adds nothing.
            ChatScript::Tokens(vec!["F1: CONFIRMED — ok".into()]),
            ChatScript::Tokens(vec!["converged view".into()]),
        ],
    )
    .await;
    run_swarm_audited(&mock, tmp.path(), 1, &["premortem", "constraints"], true)
        .await
        .unwrap();
    let convo = store::read_conversation(tmp.path(), "i").unwrap();
    assert!(
        convo.contains("4 further findings not audited (cap 20)"),
        "{convo}"
    );
}

#[tokio::test]
async fn audit_cap_swarm_off_turn_reports_the_findings_left_out() {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path(), "i");
    let items = |prefix: &str| {
        (0..12)
            .map(|i| format!("- {prefix}{i}a {prefix}{i}b {prefix}{i}c"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let mock = spawn_sequence(
        &["llama3.2"],
        vec![
            ChatScript::Tokens(vec![items("p")]),
            ChatScript::Tokens(vec![items("q")]),
            ChatScript::Tokens(vec!["converged view".into()]),
        ],
    )
    .await;
    run_swarm_audited(&mock, tmp.path(), 1, &["premortem", "constraints"], false)
        .await
        .unwrap();
    let convo = store::read_conversation(tmp.path(), "i").unwrap();
    assert!(
        convo.contains("4 further findings left out (cap 20)"),
        "{convo}"
    );
    assert!(!convo.contains("not audited"), "{convo}");
}

#[tokio::test]
async fn angles_answered_names_the_angle_that_failed() {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path(), "i");
    let mock = spawn_sequence(
        &["llama3.2"],
        vec![
            ChatScript::Tokens(vec!["finding A".into()]),
            ChatScript::EofAfter(vec!["half a".into()]),
            ChatScript::Tokens(vec!["converged view".into()]),
        ],
    )
    .await;
    run_swarm(&mock, tmp.path(), 1, &["premortem", "cheapest-disproof"])
        .await
        .unwrap();
    let convo = store::read_conversation(tmp.path(), "i").unwrap();
    assert!(
        convo.contains("_1 of 2 angles answered; missing: cheapest-disproof_"),
        "{convo}"
    );
}

#[tokio::test]
async fn angles_answered_has_no_line_when_every_angle_answered() {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path(), "i");
    let mock = spawn_sequence(
        &["llama3.2"],
        vec![
            ChatScript::Tokens(vec!["finding A".into()]),
            ChatScript::Tokens(vec!["finding B".into()]),
            ChatScript::Tokens(vec!["converged view".into()]),
        ],
    )
    .await;
    run_swarm(&mock, tmp.path(), 1, &["premortem", "cheapest-disproof"])
        .await
        .unwrap();
    let convo = store::read_conversation(tmp.path(), "i").unwrap();
    assert!(!convo.contains("angles answered"), "{convo}");
}
