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

const PLAN: &str = "## Goal\nBuild the agency tool.\n\n## Settled\n- none\n\n## Verify first\n- none\n\n## Open questions\n- none\n\n## Plan\n- [ ] T1: Build it\n  accept: `cargo test` → exit 0\n\n## Kill criteria\n- none";

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
async fn ready_to_build_folds_audited_findings_into_a_gated_build_plan() {
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
            tokens(&format!("Here it is:\n{PLAN}\n\nGood luck!")),
        ],
    )
    .await;
    let outcome = run(&mock, tmp.path(), "ready-to-build", true).await;
    let bodies = mock.chat_bodies();
    assert_eq!(bodies.len(), 7, "5 harvesters + auditor + build plan");
    assert!(bodies[0].contains("You are the Harvester"));
    let chain = &bodies[6];
    assert!(chain.contains("BUILD PLAN"));
    assert!(chain.contains("## Prior stage: findings"));
    assert!(chain.contains("[CONFIRMED] Ship solo first"));
    assert!(chain.contains("[REFUTED] Call three agencies"));
    assert!(
        outcome.synthesis.starts_with("**Build plan** → [")
            && outcome.synthesis.contains("· audited"),
        "{}",
        outcome.synthesis
    );
    let convo = store::read_conversation(tmp.path(), "i").unwrap();
    assert!(convo.contains(&format!(
        "## assistant (workflow: ready-to-build)\n{}",
        outcome.synthesis
    )));
    assert!(!convo.contains("Good luck") && !convo.contains("Build the agency tool"));
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
    scripts.push(tokens(PLAN));
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
    chained_step_body_with(
        [
            "- Ship solo first",
            "- Agencies pay monthly",
            "",
            "- risk: churn",
            "- Call three agencies",
        ],
        auditor_reply,
        max_bytes,
    )
    .await
}

async fn chained_step_body_with(
    harvest: [&str; 5],
    auditor_reply: &str,
    max_bytes: usize,
) -> String {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path(), "i");
    let mut scripts: Vec<ChatScript> = harvest.iter().map(|h| tokens(h)).collect();
    scripts.push(tokens(auditor_reply));
    scripts.push(tokens(PLAN));
    let mock = spawn_sequence(&["llama3.2"], scripts).await;
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
        .find(|l| l.contains("[CONFIRMED] Ship solo first"))
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

const NOTHING_HARVESTED: &str = "harvest produced nothing; use the quick build prompt";

async fn empty_harvest_run(audit: bool) -> (Vec<String>, ConceptError, String, usize) {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path(), "i");
    let mut scripts: Vec<ChatScript> = vec![tokens(""); 5];
    scripts.push(tokens(PLAN));
    let mock = spawn_sequence(&["llama3.2"], scripts).await;
    let client = LlmBackend::ollama_only(OllamaClient::new(mock.url.clone(), "llama3.2").unwrap());
    let err = run_workflow(
        &client,
        &Arc::new(Semaphore::new(1)),
        &SkillRegistry::builtin(),
        tmp.path(),
        "i",
        "ready-to-build",
        ContextBudget::new(8192),
        audit,
        &|_| String::new(),
        &|_: &str| {},
    )
    .await
    .unwrap_err();
    let convo = store::read_conversation(tmp.path(), "i").unwrap();
    let artifacts = store::list_artifact_files(tmp.path(), "i").unwrap().len();
    (mock.chat_bodies(), err, convo, artifacts)
}

#[tokio::test]
async fn an_empty_harvest_skips_the_audit_call_and_the_planner() {
    let (bodies, _, _, _) = empty_harvest_run(true).await;
    assert!(
        !bodies.iter().any(|b| b.contains("You are the Auditor")),
        "no auditor request expected"
    );
    assert_eq!(bodies.len(), 5, "the 5 harvesters only, no planner call");
}

#[tokio::test]
async fn an_empty_harvest_fails_the_same_way_with_the_audit_on_or_off() {
    let (_, on_err, on_convo, on_files) = empty_harvest_run(true).await;
    let (_, off_err, off_convo, off_files) = empty_harvest_run(false).await;
    assert_eq!(on_err.to_string(), off_err.to_string());
    assert_eq!(on_convo, off_convo);
    assert_eq!((on_files, off_files), (0, 0));
}

#[tokio::test]
async fn ready_to_build_mode_errors_when_every_harvester_failed() {
    for audit in [true, false] {
        let (_, err, convo, artifacts) = empty_harvest_run(audit).await;
        assert!(err.to_string().contains(NOTHING_HARVESTED), "{err}");
        assert!(!convo.contains("Build plan"), "nothing persisted: {convo}");
        assert_eq!(artifacts, 0);
    }
}

#[tokio::test]
async fn an_empty_interrogate_fan_out_fails_in_synthesize_without_an_audit_call() {
    for audit in [true, false] {
        let tmp = tempfile::tempdir().unwrap();
        seed_idea(tmp.path(), "i");
        let mock = spawn_sequence(&["llama3.2"], vec![tokens(""); 4]).await;
        let client =
            LlmBackend::ollama_only(OllamaClient::new(mock.url.clone(), "llama3.2").unwrap());
        let semaphore = Arc::new(Semaphore::new(2));
        let err = run_workflow(
            &client,
            &semaphore,
            &SkillRegistry::builtin(),
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
        let bodies = mock.chat_bodies();
        assert_eq!(bodies.len(), 4, "fan-out only, audit={audit}");
        assert!(
            !bodies.iter().any(|b| b.contains("You are the Auditor")),
            "no auditor request, audit={audit}"
        );
    }
}

const PREAMBLE_HEADING: &str = "## How to use the findings";

#[tokio::test]
async fn ready_to_build_preamble_maps_verdicts_to_sections() {
    let chain = chained_step_body(
        "F1: CONFIRMED — ok\nF2: UNCERTAIN — maybe\nF3: REFUTED — no\nF4: CONFIRMED — ok",
    )
    .await;
    let pre = chain.find(PREAMBLE_HEADING).expect("preamble present");
    let findings = chain.find("## Prior stage: findings").expect("findings");
    assert!(pre < findings, "the preamble precedes the findings");
    let preamble = &chain[pre..findings];
    for (lead, needle) in [
        ("- A CONFIRMED decision", "verbatim owner quote"),
        (
            "- An open question that is not REFUTED",
            "Open questions (Q#)",
        ),
        (
            "- A risk that is not REFUTED",
            "Verify first (P# with a read-only check)",
        ),
        ("- A risk that is not REFUTED", "Kill criteria (K#)"),
        ("- A CONFIRMED next action", "keeping any paths or commands"),
        ("- A REFUTED finding", "never Settled and never a task"),
    ] {
        let line = preamble.lines().find(|l| l.starts_with(lead)).unwrap();
        assert!(line.contains(needle), "{lead} line lacks {needle}: {line}");
    }
}

#[tokio::test]
async fn ready_to_build_preamble_leaves_refuted_findings_out_entirely() {
    let chain = chained_step_body("F1: REFUTED — no").await;
    let refuted = chain
        .lines()
        .find(|l| l.contains("REFUTED finding"))
        .unwrap();
    assert!(
        refuted.contains("leave it out of the plan entirely"),
        "{refuted}"
    );
    assert!(!chain.contains("do-not-build-on"));
}

#[tokio::test]
async fn ready_to_build_preamble_labels_each_finding_with_its_kind() {
    let chain = chained_step_body(
        "F1: CONFIRMED — ok\nF2: CONFIRMED — ok\nF3: UNCERTAIN — maybe\nF4: CONFIRMED — ok",
    )
    .await;
    let block = findings_block_of(&chain);
    for (text, lead) in [
        ("Ship solo first", "- decision · [CONFIRMED]"),
        ("Agencies pay monthly", "- fact · "),
        ("risk: churn", "- risk · "),
        ("Call three agencies", "- next action · "),
    ] {
        let line = block.lines().find(|l| l.contains(text)).unwrap();
        assert!(line.starts_with(lead), "{text}: {line}");
    }
    let pre = &chain[chain.find(PREAMBLE_HEADING).unwrap()..chain.find(block).unwrap()];
    for label in ["decision", "open question", "risk", "next action", "fact"] {
        assert!(pre.contains(label), "preamble names {label}");
    }
}

async fn planner_body_with_conversation(turns: usize, budget: usize) -> String {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path(), "i");
    for n in 0..turns {
        let filler = format!("turn {n} {}", "talk ".repeat(40));
        store::append_conversation(tmp.path(), "i", &format!("## user\n{filler}\n")).unwrap();
    }
    let mut scripts: Vec<ChatScript> = (0..5).map(|i| tokens(&format!("- finding{i}"))).collect();
    scripts.push(tokens(PLAN));
    let mock = spawn_sequence(&["llama3.2"], scripts).await;
    run_at(&mock, tmp.path(), "ready-to-build", false, budget).await;
    mock.chat_bodies().pop().unwrap().replace("\\n", "\n")
}

#[tokio::test]
async fn ready_to_build_preamble_notes_a_clipped_discussion() {
    let clipped = planner_body_with_conversation(40, 4000).await;
    let note = clipped
        .lines()
        .find(|l| l.starts_with("(discussion clipped"))
        .expect("note present when turns were dropped");
    assert!(note.contains("of 41 turns"), "{note}");
    let whole = planner_body_with_conversation(0, 8192).await;
    assert!(
        !whole.contains("discussion clipped"),
        "absent when nothing is dropped"
    );
}

#[tokio::test]
async fn ready_to_build_preamble_counts_itself_in_the_cap() {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path(), "i");
    let mut scripts: Vec<ChatScript> = (0..5)
        .map(|i| tokens(&format!("- finding{i} {}", "long text ".repeat(60))))
        .collect();
    scripts.push(tokens(PLAN));
    let mock = spawn_sequence(&["llama3.2"], scripts).await;
    let budget = 4000;
    run_at(&mock, tmp.path(), "ready-to-build", false, budget).await;
    let chain = mock.chat_bodies().pop().unwrap().replace("\\n", "\n");
    let block = findings_block_of(&chain);
    let start = chain.find(PREAMBLE_HEADING).unwrap();
    let end = chain.find(block).unwrap() + block.len();
    assert!(
        end - start <= budget / 3,
        "preamble + block is {} bytes",
        end - start
    );
    assert!(block.ends_with('…'), "{block}");
}

#[tokio::test]
async fn ready_to_build_preamble_states_refuted_first_and_conditions_the_kind_rules() {
    let chain = chained_step_body(
        "F1: CONFIRMED — ok\nF2: UNCERTAIN — maybe\nF3: REFUTED — no\nF4: CONFIRMED — ok",
    )
    .await;
    let pre = chain.find(PREAMBLE_HEADING).unwrap();
    let findings = chain.find("## Prior stage: findings").unwrap();
    let bullets: Vec<&str> = chain[pre..findings]
        .lines()
        .filter(|l| l.starts_with("- "))
        .collect();
    assert!(
        bullets[0].starts_with("- A REFUTED finding, whatever its kind"),
        "{}",
        bullets[0]
    );
    let line = |lead: &str| bullets.iter().find(|l| l.starts_with(lead)).copied();
    for lead in [
        "- An open question that is not REFUTED",
        "- A risk that is not REFUTED",
    ] {
        assert!(line(lead).is_some(), "missing rule: {lead}");
    }
    let unquoted = line("- A CONFIRMED decision").unwrap();
    assert!(
        unquoted.contains("otherwise") && unquoted.contains("Open questions"),
        "{unquoted}"
    );
}

#[tokio::test]
async fn ready_to_build_preamble_labels_an_open_question_finding() {
    let chain = chained_step_body_with(
        [
            "- Ship solo first",
            "- Agencies pay monthly",
            "- Who pays first?",
            "- risk: churn",
            "- Call three agencies",
        ],
        "F1: CONFIRMED — ok\nF2: CONFIRMED — ok\nF3: UNCERTAIN — maybe\nF4: CONFIRMED — ok\nF5: CONFIRMED — ok",
        8192,
    )
    .await;
    let line = findings_block_of(&chain)
        .lines()
        .find(|l| l.contains("Who pays first?"))
        .unwrap();
    assert!(line.starts_with("- open question · [UNCERTAIN]"), "{line}");
}

#[tokio::test]
async fn ready_to_build_preamble_keeps_the_whole_preamble_at_a_tiny_budget() {
    let chain = chained_step_body_at(
        "F1: CONFIRMED — ok\nF2: CONFIRMED — ok\nF3: UNCERTAIN — maybe\nF4: REFUTED — no",
        1500,
    )
    .await;
    let pre = chain.find(PREAMBLE_HEADING).unwrap();
    let findings = chain.find("## Prior stage: findings").unwrap();
    assert!(
        chain[pre..findings].contains("treat it as UNCERTAIN."),
        "the preamble is never clipped"
    );
    let block = findings_block_of(&chain);
    assert!(
        block.len() <= 200,
        "the block keeps only its floor: {}",
        block.len()
    );
}

fn plan_artifact_header(vault: &Path) -> String {
    let file = store::list_artifact_files(vault, "i")
        .unwrap()
        .into_iter()
        .find(|f| f.slug.ends_with("-build-plan"))
        .expect("a build-plan artifact");
    let body = store::read_artifact(vault, "i", &file.slug).unwrap().body;
    body.lines().nth(1).unwrap().to_string()
}

async fn ready_to_build_mode_run(audit: bool, auditor_reply: &str) -> (String, String) {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path(), "i");
    let mut scripts: Vec<ChatScript> = [
        "- Ship solo first",
        "- Agencies pay monthly",
        "",
        "- risk: churn",
        "- Call three agencies",
    ]
    .iter()
    .map(|h| tokens(h))
    .collect();
    if audit {
        scripts.push(tokens(auditor_reply));
    }
    scripts.push(tokens(PLAN));
    let mock = spawn_sequence(&["llama3.2"], scripts).await;
    let outcome = run(&mock, tmp.path(), "ready-to-build", audit).await;
    (outcome.synthesis, plan_artifact_header(tmp.path()))
}

#[tokio::test]
async fn ready_to_build_mode_names_a_skipped_audit_and_its_reason() {
    let (pointer, header) = ready_to_build_mode_run(false, "").await;
    let label = "ready-to-build · audit skipped (audit off in Settings)";
    assert!(pointer.contains(label), "{pointer}");
    assert!(header.starts_with(&format!("_{label} · ")), "{header}");
    assert!(!pointer.contains("quick") && !header.contains("quick"));
}

#[tokio::test]
async fn ready_to_build_mode_names_a_failed_audit() {
    let (pointer, header) = ready_to_build_mode_run(true, "I cannot judge these.").await;
    assert!(
        pointer.contains("ready-to-build · audit failed"),
        "{pointer}"
    );
    assert!(
        header.starts_with("_ready-to-build · audit failed · "),
        "{header}"
    );
}

#[tokio::test]
async fn ready_to_build_mode_flags_a_uniform_pass_as_weak() {
    let (pointer, header) = ready_to_build_mode_run(
        true,
        "F1: CONFIRMED — ok\nF2: CONFIRMED — ok\nF3: CONFIRMED — ok\nF4: CONFIRMED — ok",
    )
    .await;
    assert!(
        pointer.contains("audited · uniform pass (weak)"),
        "{pointer}"
    );
    assert!(header.contains("audited · uniform pass (weak)"), "{header}");
}

#[tokio::test]
async fn ready_to_build_mode_leaves_a_mixed_audit_unflagged() {
    let (pointer, header) = ready_to_build_mode_run(
        true,
        "F1: CONFIRMED — ok\nF2: CONFIRMED — ok\nF3: UNCERTAIN — maybe\nF4: REFUTED — no",
    )
    .await;
    assert!(pointer.contains("· audited\n"), "{pointer}");
    assert!(!pointer.contains("weak") && !header.contains("weak"));
    assert!(!pointer.contains("skipped") && !pointer.contains("failed"));
}
