//! D19/D32 workflow tests against the mock Ollama: deterministic control flow (fixed stage
//! order), failed step nulled + judge skips, chained output carried forward, the audit stage
//! gated by its toggle, only the final output persisted. No live model.

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
                sources: vec![],
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
        false,
        &|_| String::new(),
        &|_: &str| {},
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
        false,
        &|_| String::new(),
        &|_: &str| {},
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
        false,
        &|_| String::new(),
        &|_: &str| {},
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
        false,
        &|_| String::new(),
        &|_: &str| {},
    )
    .await
    .unwrap_err();
    assert!(matches!(err, ConceptError::UnknownWorkflow(name) if name == "nope"));
    assert!(mock.chat_bodies().is_empty());
}

async fn run(
    mock: &support::MockOllama,
    vault: &Path,
    name: &str,
    audit: bool,
) -> idea_vault::concepts::workflows::WorkflowOutcome {
    run_at(mock, vault, name, audit, 8192).await
}

async fn run_at(
    mock: &support::MockOllama,
    vault: &Path,
    name: &str,
    audit: bool,
    max_bytes: usize,
) -> idea_vault::concepts::workflows::WorkflowOutcome {
    let client = LlmBackend::ollama_only(OllamaClient::new(mock.url.clone(), "llama3.2").unwrap());
    let semaphore = Arc::new(Semaphore::new(1));
    let registry = SkillRegistry::builtin();
    run_workflow(
        &client,
        &semaphore,
        &registry,
        vault,
        "i",
        name,
        ContextBudget::new(max_bytes),
        audit,
        &|_| String::new(),
        &|_: &str| {},
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn interrogate_with_the_audit_on_adds_one_auditor_call_before_synthesis() {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path(), "i");
    let mock = spawn_sequence(
        &["llama3.2"],
        vec![
            tokens("1. Nobody pays"),
            tokens("- Test with a fake door"),
            tokens("- Needs a licence"),
            tokens("- Incumbents copy it"),
            tokens("F1: REFUTED — pilots paid\nF2: CONFIRMED — cheap\nF3: UNCERTAIN — region\nF4: CONFIRMED — likely"),
            tokens("audited position"),
        ],
    )
    .await;
    let outcome = run(&mock, tmp.path(), "interrogate", true).await;
    let bodies = mock.chat_bodies();
    assert_eq!(bodies.len(), 6, "4 steps + auditor + synthesizer");
    assert!(bodies[4].contains("You are the Auditor"));
    assert!(bodies[5].contains("[REFUTED — pilots paid]"));
    assert_eq!(outcome.audit.unwrap().answered, 4);
    let convo = store::read_conversation(tmp.path(), "i").unwrap();
    assert!(convo.contains("## assistant (workflow: interrogate)\naudited position"));
    assert!(convo.contains("~~Nobody pays~~"));
}

#[tokio::test]
async fn steelman_then_attack_carries_the_steelman_into_every_critic() {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path(), "i");
    let mock = spawn_sequence(
        &["llama3.2"],
        vec![
            tokens("STEELMAN-MARKER: the best version"),
            tokens("1. fails because a"),
            tokens("- disproof b"),
            tokens("argument c"),
            tokens("converged on the steelman"),
        ],
    )
    .await;
    let outcome = run(&mock, tmp.path(), "steelman-then-attack", false).await;
    let bodies = mock.chat_bodies();
    assert_eq!(bodies.len(), 5, "steelman + 3 critics + synthesizer");
    assert!(
        bodies[0].contains("You are the Advocate") && bodies[0].contains("strongest honest case")
    );
    for critic in &bodies[1..4] {
        assert!(critic.contains("You are the Critic"));
        assert!(critic.contains("## Prior stage: steelman") && critic.contains("STEELMAN-MARKER"));
    }
    assert_eq!(outcome.synthesis, "converged on the steelman");
    let convo = store::read_conversation(tmp.path(), "i").unwrap();
    assert_eq!(convo.matches("## assistant").count(), 1);
    assert!(
        !convo.contains("STEELMAN-MARKER"),
        "intermediate stage output stays out of truth"
    );
}

#[tokio::test]
async fn ready_to_build_folds_audited_findings_into_a_fenced_build_prompt() {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path(), "i");
    let mock = spawn_sequence(
        &["llama3.2"],
        vec![
            tokens("- Ship solo first"),
            tokens("- Agencies pay monthly"),
            tokens(""),
            tokens("- risk: churn"),
            tokens("- Call three agencies"),
            tokens("F1: CONFIRMED — settled\nF2: CONFIRMED — said\nF3: UNCERTAIN — maybe\nF4: REFUTED — not discussed"),
            tokens("Here it is:\n```markdown\n# Build the agency tool\n```\nGood luck!"),
        ],
    )
    .await;
    let outcome = run(&mock, tmp.path(), "ready-to-build", true).await;
    let bodies = mock.chat_bodies();
    assert_eq!(bodies.len(), 7, "5 harvesters + auditor + build prompt");
    assert!(bodies[0].contains("You are the Harvester"));
    let chain = &bodies[6];
    assert!(chain.contains("BUILD PROMPT"));
    assert!(chain.contains("## Prior stage: findings"));
    assert!(chain.contains("[CONFIRMED] Ship solo first"));
    assert!(chain.contains("[REFUTED] Call three agencies"));
    assert_eq!(
        outcome.synthesis,
        "```markdown\n# Build the agency tool\n```"
    );
    let convo = store::read_conversation(tmp.path(), "i").unwrap();
    assert!(convo.contains(
        "## assistant (workflow: ready-to-build)\n```markdown\n# Build the agency tool\n```"
    ));
    assert!(!convo.contains("Good luck"));
}

fn sampled_temperatures(mock: &support::MockOllama) -> Vec<f64> {
    mock.chat_bodies()
        .iter()
        .map(|b| {
            let v: serde_json::Value = serde_json::from_str(b).unwrap();
            v["options"]["temperature"].as_f64().unwrap()
        })
        .collect()
}

fn ready_to_build_mock_script() -> Vec<ChatScript> {
    let mut scripts: Vec<ChatScript> = (0..5).map(|i| tokens(&format!("finding {i}"))).collect();
    scripts.push(tokens("```\nbuild it\n```"));
    scripts
}

async fn run_ready_to_build(role_tuning: bool) -> Vec<f64> {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path(), "i");
    let mock = spawn_sequence(&["llama3.2"], ready_to_build_mock_script()).await;
    let client = LlmBackend::ollama_only(OllamaClient::new(mock.url.clone(), "llama3.2").unwrap());
    let mut s = client.settings();
    s.role_tuning = role_tuning;
    s.role_profiles = idea_vault::concepts::agents::default_role_profiles();
    client.set_settings(s);
    let semaphore = Arc::new(Semaphore::new(1));

    run_workflow(
        &client,
        &semaphore,
        &SkillRegistry::builtin(),
        tmp.path(),
        "i",
        "ready-to-build",
        ContextBudget::new(4096),
        false,
        &|_| String::new(),
        &|_: &str| {},
    )
    .await
    .unwrap();
    sampled_temperatures(&mock)
}

#[tokio::test]
async fn role_tuning_samples_harvesters_cold_and_the_synthesizer_warmer() {
    let temps = run_ready_to_build(true).await;
    assert_eq!(temps.len(), 6);
    for t in &temps[..5] {
        assert!((t - 0.2).abs() < 1e-6, "harvester sampled at {t}");
    }
    assert!(
        (temps[5] - 0.5).abs() < 1e-6,
        "synthesizer sampled at {}",
        temps[5]
    );
}

#[tokio::test]
async fn role_tuning_off_samples_every_step_at_the_global_temperature() {
    let temps = run_ready_to_build(false).await;
    assert_eq!(temps.len(), 6);
    assert!(temps.iter().all(|t| (t - 0.7).abs() < 1e-6), "{temps:?}");
}

#[tokio::test]
async fn related_block_reaches_workflow_stages_but_not_audit() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path(), "i");
    let mock = spawn_sequence(
        &["llama3.2"],
        vec![
            tokens("STEELMAN-MARKER: the best version"),
            tokens("1. fails because a"),
            tokens("- disproof b"),
            tokens("argument c"),
            tokens("F1: CONFIRMED — a\nF2: UNCERTAIN — b\nF3: REFUTED — c"),
            tokens("converged"),
        ],
    )
    .await;
    let client = LlmBackend::ollama_only(OllamaClient::new(mock.url.clone(), "llama3.2").unwrap());
    let calls = AtomicUsize::new(0);
    let provider = |_: usize| {
        calls.fetch_add(1, Ordering::SeqCst);
        "## Related ideas elsewhere in the vault\n- RELATED-MARKER (`other`): link\n\n".to_string()
    };

    run_workflow(
        &client,
        &Semaphore::new(1),
        &SkillRegistry::builtin(),
        tmp.path(),
        "i",
        "steelman-then-attack",
        ContextBudget::new(8192),
        true,
        &provider,
        &|_: &str| {},
    )
    .await
    .unwrap();

    assert_eq!(calls.load(Ordering::SeqCst), 2, "once per hydrated stage");
    let bodies = mock.chat_bodies();
    assert_eq!(
        bodies.len(),
        6,
        "steelman + 3 critics + auditor + synthesizer"
    );
    for stage in &bodies[0..4] {
        let block = stage
            .find("RELATED-MARKER")
            .expect("block in every stage prompt");
        let own = stage.find("Idea under workflow.").unwrap();
        assert!(block < own, "block precedes the own context");
    }
    for critic in &bodies[1..4] {
        assert!(
            critic.contains("STEELMAN-MARKER"),
            "carried stage output kept"
        );
    }
    let audit = &bodies[4];
    assert!(
        audit.contains("Judge each finding ONLY"),
        "the audit prompt"
    );
    assert!(
        !audit.contains("RELATED-MARKER"),
        "the audit prompt carries no related block; agent answers here never quote it"
    );
}

async fn chained_step_body(auditor_reply: &str) -> String {
    chained_step_body_at(auditor_reply, 8192).await
}

async fn chained_step_body_at(auditor_reply: &str, max_bytes: usize) -> String {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path(), "i");
    let mock = spawn_sequence(
        &["llama3.2"],
        vec![
            tokens("- Ship solo first"),
            tokens("- Agencies pay monthly"),
            tokens(""),
            tokens("- risk: churn"),
            tokens("- Call three agencies"),
            tokens(auditor_reply),
            tokens("```markdown\n# Build\n```"),
        ],
    )
    .await;
    run_at(&mock, tmp.path(), "ready-to-build", true, max_bytes).await;
    mock.chat_bodies().pop().unwrap().replace("\\n", "\n")
}

#[tokio::test]
async fn chained_findings_carry_the_auditor_reason() {
    let chain = chained_step_body(
        "F1: CONFIRMED — settled in the discussion\nF2: CONFIRMED — said\nF3: UNCERTAIN — maybe\nF4: REFUTED — not discussed",
    )
    .await;
    assert!(
        chain.contains("[CONFIRMED] Ship solo first (")
            && chain.contains("auditor: settled in the discussion")
    );
    assert!(chain.contains("auditor: not discussed"));
}

#[tokio::test]
async fn chained_findings_are_unlabelled_when_the_audit_failed() {
    let chain = chained_step_body("I cannot judge these.").await;
    assert!(chain.contains("Ship solo first"));
    assert!(!chain.contains("[UNCERTAIN]") && !chain.contains("[CONFIRMED]"));
    assert!(chain.contains("audit was unavailable"));
}

#[tokio::test]
async fn chained_findings_open_with_the_verdict_guidance() {
    let chain = chained_step_body(
        "F1: CONFIRMED — ok\nF2: UNCERTAIN — maybe\nF3: REFUTED — no\nF4: CONFIRMED — ok",
    )
    .await;
    let heading = "## Prior stage: findings\n";
    let at = chain.find(heading).unwrap() + heading.len();
    assert!(chain[at..].starts_with(idea_vault::concepts::audit::VERDICT_GUIDANCE));
    assert_eq!(
        chain
            .matches(idea_vault::concepts::audit::VERDICT_GUIDANCE)
            .count(),
        1
    );
    assert!(!chain.contains("listed separately"));
}

#[tokio::test]
async fn chained_findings_drop_the_auditor_suffix_when_the_reason_is_empty() {
    let chain = chained_step_body(
        "F1: CONFIRMED —\nF2: CONFIRMED — said\nF3: UNCERTAIN — maybe\nF4: REFUTED — no",
    )
    .await;
    let line = chain
        .lines()
        .find(|l| l.starts_with("- [CONFIRMED] Ship solo first"))
        .unwrap();
    assert!(!line.contains("auditor:"), "{line}");
}

fn findings_block_of(chain: &str) -> &str {
    let start = chain.find("## Prior stage: findings\n").unwrap();
    let block = &chain[start..];
    &block[..block.find("\n\n## ").unwrap_or(block.len())]
}

#[tokio::test]
async fn chained_findings_clip_each_reason() {
    let long = "r".repeat(3000);
    let reply =
        format!("F1: CONFIRMED — {long}\nF2: CONFIRMED — ok\nF3: UNCERTAIN — ok\nF4: REFUTED — ok");
    let chain = chained_step_body(&reply).await;
    let block = findings_block_of(&chain);
    assert!(block.contains('…'), "the long reason is clipped");
    assert!(
        block.len() < 8192 / 2,
        "outer cap not reached: {}",
        block.len()
    );
}

#[tokio::test]
async fn chained_findings_clip_the_block_to_half_the_budget() {
    let long = "r".repeat(1000);
    let reply = format!(
        "F1: CONFIRMED — {long}\nF2: CONFIRMED — {long}\nF3: UNCERTAIN — {long}\nF4: REFUTED — {long}"
    );
    let chain = chained_step_body_at(&reply, 1024).await;
    let block = findings_block_of(&chain);
    assert!(block.len() <= 1024 / 2, "block is {} bytes", block.len());
}

async fn empty_harvest_run(audit: bool) -> (Vec<String>, String) {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path(), "i");
    let mock = spawn_sequence(
        &["llama3.2"],
        vec![
            tokens(""),
            tokens(""),
            tokens(""),
            tokens(""),
            tokens(""),
            tokens("```markdown\n# Build\n```"),
        ],
    )
    .await;
    run(&mock, tmp.path(), "ready-to-build", audit).await;
    (
        mock.chat_bodies(),
        store::read_conversation(tmp.path(), "i").unwrap(),
    )
}

#[tokio::test]
async fn an_empty_harvest_skips_the_audit_call() {
    let (bodies, _) = empty_harvest_run(true).await;
    assert!(
        !bodies.iter().any(|b| b.contains("You are the Auditor")),
        "no auditor request expected"
    );
    assert_eq!(bodies.len(), 6, "5 harvesters + the build-prompt step");
}

#[tokio::test]
async fn an_empty_harvest_behaves_the_same_with_the_audit_on_or_off() {
    let (on_bodies, on_convo) = empty_harvest_run(true).await;
    let (off_bodies, off_convo) = empty_harvest_run(false).await;
    assert_eq!(on_bodies.len(), off_bodies.len());
    assert_eq!(on_convo, off_convo);

    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path(), "i");
    let mock = spawn(&["llama3.2"], ChatScript::EofAfter(vec![])).await;
    let client = LlmBackend::ollama_only(OllamaClient::new(mock.url.clone(), "llama3.2").unwrap());
    let semaphore = Arc::new(Semaphore::new(2));
    let registry = SkillRegistry::builtin();
    for audit in [true, false] {
        let err = run_workflow(
            &client,
            &semaphore,
            &registry,
            tmp.path(),
            "i",
            "interrogate",
            ContextBudget::new(4096),
            audit,
            &|_| String::new(),
            &|_: &str| {},
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ConceptError::NothingToSynthesize));
    }
}
