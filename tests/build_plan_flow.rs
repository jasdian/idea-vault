//! The build-plan persist boundary (docs/adr/0029): a gated plan lands as an artifact, the
//! transcript gets only a pointer turn, and pointer turns never ground a later plan.

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
