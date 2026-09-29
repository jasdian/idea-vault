//! The build-plan persist boundary (docs/adr/0030): a gated plan lands as an artifact, the
//! transcript gets only a pointer turn, and pointer turns never ground a later plan. Both
//! capstone paths (the quick skill and the audited workflow) reach it against the mock Ollama.

mod support;

use std::path::Path;

use chrono::{TimeZone, Utc};
use idea_vault::ai::sources::SourceProbe;
use idea_vault::concepts::audit::Label;
use idea_vault::concepts::build_plan::finish::{finish, Finished, PlanInputs};
use idea_vault::concepts::build_plan::gates::{AuditView, AuditedFinding};
use idea_vault::concepts::ConceptError;
use idea_vault::domain::{ArtifactKind, Idea, IdeaFrontmatter, IdeaState};
use idea_vault::vault::store;

const SLUG: &str = "trader";

fn seed(vault: &Path) {
    store::write_idea(
        vault,
        &Idea {
            frontmatter: IdeaFrontmatter {
                title: "Trader".into(),
                slug: SLUG.into(),
                state: IdeaState::InDiscussion,
                tags: vec![],
                sources: vec![],
                created: Utc.with_ymd_and_hms(2026, 9, 1, 10, 0, 0).unwrap(),
                updated: Utc.with_ymd_and_hms(2026, 9, 1, 10, 0, 0).unwrap(),
            },
            body: "A small trading bot.\n".into(),
        },
    )
    .unwrap();
    store::append_turn(
        vault,
        SLUG,
        "user",
        "We run the cheapest disproof before any Rust exists.",
    )
    .unwrap();
    store::append_turn(
        vault,
        SLUG,
        "assistant",
        "Close must not be gated symmetrically with open.",
    )
    .unwrap();
}

const ANSWER: &str = "Here is the plan.

## Goal
Disprove the strategy cheaply before building.

## Settled
- S1: Disproof comes before any code.
  quote: \"the cheapest disproof before any Rust exists\"
- S2: Close is gated differently from open.
  quote: \"Close must not be gated symmetrically with open\"
- S3: The owner chose freeze at entry.
  quote: \"we will freeze the zone at entry\"

## Verify first
- none

## Open questions
- Q1: Freeze the zone snapshot at entry, or dwell on the live label?

## Plan
- [ ] T1: Write the spec with a dated kill criterion
  touches: `SPEC.md`
  accept: `test -s SPEC.md` → exit 0

## Kill criteria
- K1: The backtest prints KILL → stop and report
  checked by: T1
  gates: T1";

fn run(vault: &Path, answer: &str, minute: u32) -> Result<Finished, ConceptError> {
    let probe = SourceProbe::default();
    finish(PlanInputs {
        vault_dir: vault,
        idea_slug: SLUG,
        answer,
        turn_role: "assistant (skill: build-prompt)",
        lens: "build-prompt",
        model: "llama3.2".into(),
        audit: None,
        probe: &probe,
        now: Utc.with_ymd_and_hms(2026, 9, 29, 12, minute, 0).unwrap(),
    })
}

#[test]
fn finish_writes_an_artifact_plus_a_pointer_turn() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let done = run(dir.path(), ANSWER, 0).unwrap();
    assert_eq!(done.artifact_slug, "20260929-120000-build-plan");

    let artifact = store::read_artifact(dir.path(), SLUG, &done.artifact_slug).unwrap();
    assert_eq!(artifact.frontmatter.kind, ArtifactKind::BuildPlan);
    assert_eq!(artifact.frontmatter.lens.as_deref(), Some("build-prompt"));
    assert!(
        artifact
            .body
            .starts_with("# Build plan — Trader\n_quick · unaudited · llama3.2"),
        "{}",
        artifact.body
    );
    assert!(artifact
        .body
        .contains("- S1: Disproof comes before any code. — you"));
    assert!(artifact
        .body
        .contains("- S2: Close is gated differently from open. — foil"));
    assert!(
        artifact.body.contains(
            "## Quarantined — do not build on these\n- X1: The owner chose freeze at entry"
        ),
        "an ungrounded quote is quarantined in the artifact: {}",
        artifact.body
    );

    let conversation = store::read_conversation(dir.path(), SLUG).unwrap();
    let last = store::split_turns(&conversation).pop().unwrap();
    assert!(
        last.starts_with("## assistant (skill: build-prompt)\n"),
        "{last}"
    );
    assert!(
        last.contains("[20260929-120000-build-plan](/idea/trader/artifact/20260929-120000-build-plan.md) · quick · unaudited"),
        "{last}"
    );
    assert!(
        last.contains("settled 2 (1 you · 1 foil) · opened 0 · quarantined 1"),
        "{last}"
    );
    assert!(
        last.contains("**Open questions for you**")
            && last.contains("- Q1: Freeze the zone snapshot"),
        "{last}"
    );
}

#[test]
fn finish_never_appends_the_plan_body() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    run(dir.path(), ANSWER, 0).unwrap();
    let conversation = store::read_conversation(dir.path(), SLUG).unwrap();
    for body_line in [
        "## Settled",
        "## Plan",
        "quote:",
        "accept:",
        "Disproof comes before any code",
    ] {
        assert!(
            !conversation.contains(body_line),
            "the transcript carries only the pointer, found {body_line:?}"
        );
    }
}

#[test]
fn finish_excludes_pointer_turns_from_the_next_haystack() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let first = run(dir.path(), ANSWER, 0).unwrap();
    let pointer_words = "Freeze the zone snapshot at entry, or dwell on the live label";
    assert!(first.pointer.contains(pointer_words));

    let echo = format!(
        "## Goal\nShip.\n\n## Settled\n- S1: We freeze or dwell.\n  quote: \"{pointer_words}\"\n\n## Plan\n- [ ] T1: Build it\n  accept: `cargo test` → exit 0"
    );
    let second = run(dir.path(), &echo, 1).unwrap();
    assert!(
        second.plan.settled.is_empty() && second.plan.quarantined.len() == 1,
        "a quote found only in an earlier pointer turn does not ground: {:?}",
        second.plan
    );
    assert_eq!(second.artifact_slug, "20260929-120100-build-plan");
    let body = store::read_artifact(dir.path(), SLUG, &second.artifact_slug)
        .unwrap()
        .body;
    assert!(
        body.contains("· 1 capstone turn(s) excluded from evidence"),
        "{body}"
    );
}

#[test]
fn finish_with_an_unusable_plan_persists_nothing() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let before = store::read_conversation(dir.path(), SLUG).unwrap();
    let err = run(
        dir.path(),
        "I could not make a plan from this discussion.",
        0,
    )
    .unwrap_err();
    assert!(matches!(err, ConceptError::PlanUnusable), "{err:?}");
    assert_eq!(store::read_conversation(dir.path(), SLUG).unwrap(), before);
    assert!(store::read_artifacts(dir.path(), SLUG).unwrap().is_empty());
}

#[test]
fn finish_disambiguates_a_second_plan_in_the_same_second() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let a = run(dir.path(), ANSWER, 0).unwrap();
    let b = run(dir.path(), ANSWER, 0).unwrap();
    assert_ne!(a.artifact_slug, b.artifact_slug);
    assert!(
        b.pointer
            .contains(&format!("/idea/trader/artifact/{}.md", b.artifact_slug)),
        "{}",
        b.pointer
    );
    assert_eq!(store::read_artifacts(dir.path(), SLUG).unwrap().len(), 2);
}

#[test]
fn finish_consults_the_latest_open_questions_artifact() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    for (stamp, body) in [
        ("20260801-100000", "- Is close gated like open?"),
        (
            "20260901-100000",
            "- Should close be gated differently from open, or the same?",
        ),
    ] {
        store::write_artifact(
            dir.path(),
            SLUG,
            &idea_vault::domain::Artifact {
                frontmatter: idea_vault::domain::ArtifactFrontmatter {
                    slug: format!("{stamp}-open-questions"),
                    title: "Open questions".into(),
                    kind: ArtifactKind::Finding,
                    lens: Some("extract-open-questions".into()),
                    created: Utc.with_ymd_and_hms(2026, 9, 1, 10, 0, 0).unwrap(),
                    model: "llama3.2".into(),
                },
                body: body.into(),
            },
        )
        .unwrap();
    }
    let done = run(dir.path(), ANSWER, 0).unwrap();
    let artifact = store::read_artifact(dir.path(), SLUG, &done.artifact_slug).unwrap();
    assert!(
        artifact
            .body
            .contains("consulted: 20260901-100000-open-questions"),
        "{}",
        artifact.body
    );
    assert!(
        done.plan
            .open
            .iter()
            .any(|q| q.text.contains("Close is gated differently")),
        "the foil item the latest artifact lists as open is opened: {:?}",
        done.plan.open
    );
}

#[test]
fn finish_records_an_audited_run_and_counts_opened_claims_only() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let probe = SourceProbe::default();
    let audit = AuditView {
        findings: vec![AuditedFinding {
            text: "Who signs off on the dated kill criterion before the backtest runs?".into(),
            lenses: vec!["extract-open-questions".into()],
            label: Label::Uncertain,
            reason: String::new(),
        }],
        failed: true,
        uniform_pass: false,
    };
    let done = finish(PlanInputs {
        vault_dir: dir.path(),
        idea_slug: SLUG,
        answer: ANSWER,
        turn_role: "assistant (workflow: ready-to-build)",
        lens: "ready-to-build",
        model: "llama3.2".into(),
        audit: Some(&audit),
        probe: &probe,
        now: Utc.with_ymd_and_hms(2026, 9, 29, 12, 5, 0).unwrap(),
    })
    .unwrap();
    let artifact = store::read_artifact(dir.path(), SLUG, &done.artifact_slug).unwrap();
    assert!(
        artifact
            .body
            .contains("_audited · audit unavailable · llama3.2 · 2026-09-29 12:05 · 0 capstone"),
        "{}",
        artifact.body
    );
    assert!(
        done.pointer
            .contains("Who signs off on the dated kill criterion"),
        "a harvested open question reaches the owner: {}",
        done.pointer
    );
    assert!(
        done.pointer.contains("· opened 0 ·"),
        "an audit question appended to Open is not a Settled claim opened: {}",
        done.pointer
    );
    let conversation = store::read_conversation(dir.path(), SLUG).unwrap();
    assert!(store::split_turns(&conversation)
        .last()
        .unwrap()
        .starts_with("## assistant (workflow: ready-to-build)\n"));
}

#[test]
fn finish_counts_only_settled_claims_as_opened() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    store::append_turn(
        dir.path(),
        SLUG,
        "user",
        "The live bot must not be built until the backtest survives.",
    )
    .unwrap();
    let no_kill_rows = ANSWER.split("## Kill criteria").next().unwrap();
    let done = run(dir.path(), no_kill_rows, 0).unwrap();
    assert!(
        done.plan
            .open
            .iter()
            .any(|q| q.markers.iter().any(|m| m.contains("gate language"))),
        "the gate-language question is added: {:?}",
        done.plan.open
    );
    assert!(done.pointer.contains("· opened 0 ·"), "{}", done.pointer);
}

#[test]
fn finish_for_a_missing_idea_persists_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let err = run(dir.path(), ANSWER, 0).unwrap_err();
    assert!(matches!(err, ConceptError::Vault(_)), "{err:?}");
    assert!(!dir.path().join(SLUG).exists());
}

#[test]
fn finish_survives_an_unreadable_open_questions_artifact() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let artifacts = dir.path().join(SLUG).join("artifacts");
    std::fs::create_dir_all(&artifacts).unwrap();
    std::fs::write(
        artifacts.join("20260902-100000-open-questions.md"),
        "not frontmatter at all",
    )
    .unwrap();
    let done = run(dir.path(), ANSWER, 0).unwrap();
    let artifact = store::read_artifact(dir.path(), SLUG, &done.artifact_slug).unwrap();
    assert!(
        artifact.body.contains("consulted: none"),
        "{}",
        artifact.body
    );
    assert!(
        artifact
            .body
            .contains("> open-questions artifact unreadable: 20260902-100000-open-questions"),
        "the miss is recorded, not silent: {}",
        artifact.body
    );
}

const PLANNER_ANSWER: &str = "Sure, here is the plan.

## Goal
Disprove the strategy cheaply before building.

## Settled
- S1: Disproof comes before any code.
  quote: \"the cheapest disproof before any Rust exists\"

## Verify first
- none

## Open questions
- Q1: Which market do we backtest first?

## Plan
- [ ] T1: Write the spec with a dated kill criterion
  touches: `SPEC.md`
  accept: `test -s SPEC.md` → exit 0

## Kill criteria
- K1: The backtest prints KILL → stop and report
  checked by: T1
  gates: T1";

fn mock_backend(mock: &support::MockOllama) -> idea_vault::ai::LlmBackend {
    idea_vault::ai::LlmBackend::ollama_only(
        idea_vault::ai::OllamaClient::new(mock.url.clone(), "llama3.2").unwrap(),
    )
}

async fn quick_plan(
    answers: &[&str],
) -> (
    tempfile::TempDir,
    support::MockOllama,
    Result<String, ConceptError>,
) {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let mock = support::spawn_sequence(
        &["llama3.2"],
        answers
            .iter()
            .map(|a| support::ChatScript::Tokens(vec![a.to_string()]))
            .collect(),
    )
    .await;
    let registry = idea_vault::concepts::skills::SkillRegistry::builtin();
    let result = idea_vault::concepts::skills::invoke(
        &mock_backend(&mock),
        &tokio::sync::Semaphore::new(1),
        dir.path(),
        SLUG,
        registry.get("build-prompt").unwrap(),
        idea_vault::concepts::skills::ContextSlot {
            budget: idea_vault::ai::budget::ContextBudget::new(4096),
            related: &|_| String::new(),
        },
        &|_: &str| {},
    )
    .await;
    (dir, mock, result)
}

fn plan_artifacts(vault: &Path) -> Vec<idea_vault::domain::Artifact> {
    store::read_artifacts(vault, SLUG)
        .unwrap()
        .into_iter()
        .filter(|a| a.frontmatter.kind == ArtifactKind::BuildPlan)
        .collect()
}

#[tokio::test]
async fn quick_plan_lands_an_artifact_and_a_pointer_turn() {
    let (dir, _mock, result) = quick_plan(&[PLANNER_ANSWER]).await;
    let pointer = result.unwrap();
    let plans = plan_artifacts(dir.path());
    assert_eq!(plans.len(), 1, "one build-plan artifact");
    let plan = &plans[0];
    assert_eq!(plan.frontmatter.lens.as_deref(), Some("build-prompt"));
    assert_eq!(plan.frontmatter.model, "llama3.2");
    assert!(
        plan.body.contains("_quick · unaudited · llama3.2")
            && plan
                .body
                .contains("- S1: Disproof comes before any code. — you"),
        "{}",
        plan.body
    );

    let conversation = store::read_conversation(dir.path(), SLUG).unwrap();
    let last = store::split_turns(&conversation).pop().unwrap();
    assert!(
        last.starts_with("## assistant (skill: build-prompt)\n**Build plan** → ["),
        "{last}"
    );
    assert!(
        last.contains(&format!(
            "/idea/{SLUG}/artifact/{}.md",
            plan.frontmatter.slug
        )),
        "{last}"
    );
    assert!(
        pointer.starts_with("**Build plan** → [") && last.contains(pointer.trim()),
        "invoke returns the pointer it appended: {pointer}"
    );
    assert!(
        !conversation.contains("## Settled") && !conversation.contains("Sure, here is the plan"),
        "the plan body stays out of the transcript"
    );
}

#[tokio::test]
async fn quick_plan_gates_make_no_model_call() {
    let (_dir, mock, result) = quick_plan(&[PLANNER_ANSWER, "a second call must not happen"]).await;
    result.unwrap();
    let bodies = mock.chat_bodies();
    assert_eq!(bodies.len(), 1, "one planner call, then only code");
    assert!(bodies[0].contains("## Kill criteria") && bodies[0].contains("quote:"));
}

#[tokio::test]
async fn quick_plan_with_an_unusable_answer_persists_nothing() {
    let dir_before = {
        let d = tempfile::tempdir().unwrap();
        seed(d.path());
        store::read_conversation(d.path(), SLUG).unwrap()
    };
    let (dir, mock, result) = quick_plan(&[
        "I could not make a plan from this.",
        "Still nothing to plan here.",
    ])
    .await;
    let err = result.unwrap_err();
    assert!(matches!(err, ConceptError::PlanUnusable), "{err:?}");
    assert_eq!(
        mock.chat_bodies().len(),
        2,
        "the contract's single retry ran"
    );
    assert_eq!(
        store::read_conversation(dir.path(), SLUG).unwrap(),
        dir_before
    );
    assert!(store::read_artifacts(dir.path(), SLUG).unwrap().is_empty());
}

fn without_section(answer: &str, heading: &str) -> String {
    let mut out = Vec::new();
    let mut skipping = false;
    for line in answer.lines() {
        if line.starts_with("## ") {
            skipping = line == heading;
        }
        if !skipping {
            out.push(line);
        }
    }
    out.join("\n")
}

#[tokio::test]
async fn retry_plan_missing_kill_criteria_gets_no_retry() {
    let answer = without_section(PLANNER_ANSWER, "## Kill criteria");
    let (dir, mock, result) = quick_plan(&[&answer, "a retry must not happen"]).await;
    result.unwrap();
    assert_eq!(
        mock.chat_bodies().len(),
        1,
        "an optional section missing is not a contract violation"
    );
    let plans = plan_artifacts(dir.path());
    assert_eq!(plans.len(), 1);
    assert!(
        plans[0].body.contains("## Kill criteria"),
        "{}",
        plans[0].body
    );
}

#[tokio::test]
async fn retry_plan_keeps_a_usable_first_answer_over_an_off_grammar_retry() {
    let first = without_section(PLANNER_ANSWER, "## Settled");
    let (dir, mock, result) = quick_plan(&[&first, "Sorry, I cannot format that."]).await;
    result.unwrap();
    assert_eq!(
        mock.chat_bodies().len(),
        2,
        "a missing required section retries once"
    );
    let plans = plan_artifacts(dir.path());
    assert_eq!(plans.len(), 1);
    assert!(
        plans[0]
            .body
            .contains("T1: Write the spec with a dated kill criterion"),
        "{}",
        plans[0].body
    );
}

const REFUTED_CLAIM: &str = "Call three agencies next week";

async fn audited_plan() -> (
    tempfile::TempDir,
    idea_vault::concepts::workflows::WorkflowOutcome,
) {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    store::append_turn(
        dir.path(),
        SLUG,
        "user",
        "We should call three agencies next week.",
    )
    .unwrap();
    let answer = format!(
        "## Goal\nDisprove the strategy cheaply.\n\n## Settled\n- S1: {REFUTED_CLAIM}.\n  quote: \"call three agencies next week\"\n- S2: Disproof comes before any code.\n  quote: \"the cheapest disproof before any Rust exists\"\n\n## Verify first\n- none\n\n## Open questions\n- none\n\n## Plan\n- [ ] T1: Write the spec\n  touches: `SPEC.md`\n  accept: `test -s SPEC.md` → exit 0\n\n## Kill criteria\n- K1: The backtest prints KILL → stop\n  checked by: T1\n  gates: T1"
    );
    let tokens = |t: &str| support::ChatScript::Tokens(vec![t.to_string()]);
    let mock = support::spawn_sequence(
        &["llama3.2"],
        vec![
            tokens(""),
            tokens(""),
            tokens(""),
            tokens(""),
            tokens(&format!("- {REFUTED_CLAIM}")),
            tokens("F1: REFUTED — the owner only floated it"),
            tokens(&answer),
        ],
    )
    .await;
    let outcome = idea_vault::concepts::workflows::run_workflow(
        &mock_backend(&mock),
        &tokio::sync::Semaphore::new(1),
        &idea_vault::concepts::skills::SkillRegistry::builtin(),
        dir.path(),
        SLUG,
        "ready-to-build",
        idea_vault::ai::budget::ContextBudget::new(8192),
        true,
        &|_| String::new(),
        &|_: &str| {},
    )
    .await
    .unwrap();
    assert_eq!(
        mock.chat_bodies().len(),
        7,
        "5 harvesters + auditor + planner"
    );
    (dir, outcome)
}

#[tokio::test]
async fn audited_plan_carries_the_audit_into_the_gates() {
    let (dir, _) = audited_plan().await;
    let plans = plan_artifacts(dir.path());
    assert_eq!(plans.len(), 1);
    let body = &plans[0].body;
    assert_eq!(plans[0].frontmatter.lens.as_deref(), Some("ready-to-build"));
    assert!(body.contains("_audited · llama3.2"), "{body}");
    let quarantined = body
        .split("## Quarantined")
        .nth(1)
        .unwrap_or_else(|| panic!("a Quarantined section: {body}"));
    assert!(
        quarantined.contains(REFUTED_CLAIM)
            && quarantined.contains("refuted by the audit: the owner only floated it"),
        "the refuted claim is quarantined with the auditor's reason: {body}"
    );
    let settled = body.split("## Settled").nth(1).unwrap();
    let settled = settled.split("\n## ").next().unwrap();
    assert!(
        !settled.contains(REFUTED_CLAIM) && settled.contains("Disproof comes before any code"),
        "{settled}"
    );
}

#[tokio::test]
async fn audited_plan_pointer_names_the_workflow() {
    let (dir, outcome) = audited_plan().await;
    let conversation = store::read_conversation(dir.path(), SLUG).unwrap();
    let last = store::split_turns(&conversation).pop().unwrap();
    assert!(
        last.starts_with("## assistant (workflow: ready-to-build)\n**Build plan** → ["),
        "{last}"
    );
    assert!(last.contains("· audited\n"), "{last}");
    assert!(
        !last.contains("_Audit:") && !last.contains("## Settled"),
        "the pointer carries neither the audit appendix nor the plan body: {last}"
    );
    assert!(
        last.contains(outcome.synthesis.trim()),
        "{}",
        outcome.synthesis
    );
}

#[test]
fn an_unusable_plan_answers_422_with_the_retry_hint() {
    use axum::response::IntoResponse;
    let response = idea_vault::web::WebError::from(ConceptError::PlanUnusable).into_response();
    assert_eq!(
        response.status(),
        axum::http::StatusCode::UNPROCESSABLE_ENTITY
    );
    assert!(ConceptError::PlanUnusable
        .to_string()
        .contains("nothing was saved; try again"));
}
