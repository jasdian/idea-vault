//! Make-skill distil against the mock Ollama (docs/adr/0042, D42): one contract-held call (two on
//! a violation) turns a discussion into exactly one `skill_draft` artifact the skill loader
//! accepts, with every evidence quote grounded or marked, and never a transcript turn.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers outside #[test] fns; HTC-6 binds shipping code"
)]

mod support;

use std::path::Path;
use std::sync::Arc;

use chrono::{TimeZone, Utc};
use idea_vault::ai::budget::ContextBudget;
use idea_vault::ai::{LlmBackend, OllamaClient};
use idea_vault::concepts::make_skill::{self, parse_draft_body};
use idea_vault::concepts::skills::{check_candidate, SkillRegistry, DISTILL_SKILL};
use idea_vault::domain::{ArtifactKind, Idea, IdeaFrontmatter, IdeaState};
use idea_vault::vault::store;
use support::{refused_url, spawn, spawn_sequence, ChatScript};
use tokio::sync::Semaphore;

const SLUG: &str = "tutoring";

const CONVERSATION: &str = "## user\nA peer-tutoring marketplace for high-schoolers.\n\n\
## assistant (skill: premortem)\n1. **Regulators ban it** — child-safety law.\n\n\
## user\nNow assume a regulator hates it and wants it dead within a year.\n\n\
## assistant\nA hostile regulator would first classify the app as an employer.\n";

pub const GOOD_DRAFT: &str = "Here is the move.\n\n~~~skill\n---\nname: hostile-regulator\ndescription: \"Attack an idea as a regulator who wants it dead.\"\nstage: attack\nrole: critic\ncontract: ranked_list\n---\n\nAssume a regulator hates the idea below; list the rules they reach for, cheapest to enforce first.\n{context}\n~~~\n\n## Evidence\n- \"assume a regulator hates it and wants it dead\"\n- \"regulators adore every tutoring startup\"\n";

fn seed(vault: &Path) {
    store::write_idea(
        vault,
        &Idea {
            frontmatter: IdeaFrontmatter {
                title: "Tutoring".into(),
                slug: SLUG.into(),
                state: IdeaState::InDiscussion,
                tags: vec![],
                sources: vec![],
                created: Utc.with_ymd_and_hms(2026, 9, 30, 10, 0, 0).unwrap(),
                updated: Utc.with_ymd_and_hms(2026, 9, 30, 10, 0, 0).unwrap(),
            },
            body: "A peer-tutoring marketplace.\n".into(),
        },
    )
    .unwrap();
    store::append_conversation(vault, SLUG, CONVERSATION).unwrap();
}

async fn run(url: &str, vault: &Path) -> Result<make_skill::DistillOutcome, String> {
    let llm = LlmBackend::ollama_only(OllamaClient::new(url.to_string(), "llama3.2").unwrap());
    make_skill::distill(
        &llm,
        &Arc::new(Semaphore::new(2)),
        vault,
        SLUG,
        &SkillRegistry::builtin(),
        ContextBudget::new(16 * 1024),
        &|_: &str| {},
    )
    .await
    .map_err(|e| e.to_string())
}

#[tokio::test]
async fn distill_writes_one_loadable_draft_artifact_and_no_turn() {
    let tmp = tempfile::tempdir().unwrap();
    seed(tmp.path());
    let before = store::read_conversation(tmp.path(), SLUG).unwrap();
    let mock = spawn(&["llama3.2"], ChatScript::Tokens(vec![GOOD_DRAFT.into()])).await;

    let outcome = run(&mock.url, tmp.path()).await.unwrap();
    assert_eq!(outcome.name, "hostile-regulator");
    assert_eq!(outcome.ungrounded, 1);

    let bodies = mock.chat_bodies();
    assert_eq!(bodies.len(), 1, "one call on contract");
    assert!(
        bodies[0].contains("Move trace (code-built"),
        "trace block sent"
    );
    assert!(bodies[0].contains("premortem"), "skill book listed");
    assert!(
        !bodies[0].contains(".runs"),
        "the run journal never reaches the prompt"
    );

    assert_eq!(store::read_conversation(tmp.path(), SLUG).unwrap(), before);
    let artifacts = store::read_artifacts(tmp.path(), SLUG).unwrap();
    assert_eq!(artifacts.len(), 1);
    let a = &artifacts[0];
    assert_eq!(a.frontmatter.slug, outcome.artifact_slug);
    assert_eq!(a.frontmatter.kind, ArtifactKind::SkillDraft);
    assert_eq!(a.frontmatter.lens.as_deref(), Some(DISTILL_SKILL));
    let recipe = a.frontmatter.recipe.as_ref().expect("a recipe");
    assert_eq!(recipe.skill.as_deref(), Some(DISTILL_SKILL));

    let draft = parse_draft_body(&a.body).expect("a draft body");
    let skill = check_candidate(&draft.raw).expect("the loader accepts the draft");
    assert_eq!(skill.origin.as_deref(), Some(SLUG));
    assert_eq!(skill.prompt.matches("{context}").count(), 1);
    let flags: Vec<(bool, bool)> = draft
        .evidence
        .iter()
        .map(|e| (e.grounded, e.owner))
        .collect();
    assert_eq!(flags, [(true, true), (false, false)]);
}

#[tokio::test]
async fn an_off_contract_first_answer_gets_exactly_one_retry() {
    let tmp = tempfile::tempdir().unwrap();
    seed(tmp.path());
    let mock = spawn_sequence(
        &["llama3.2"],
        vec![
            ChatScript::Tokens(vec!["Sure, the move is: attack it like a regulator.".into()]),
            ChatScript::Tokens(vec![GOOD_DRAFT.into()]),
        ],
    )
    .await;
    let outcome = run(&mock.url, tmp.path()).await.unwrap();
    assert_eq!(mock.chat_bodies().len(), 2);
    assert!(
        mock.chat_bodies()[1].contains("~~~skill"),
        "the retry note names the fence"
    );
    assert_eq!(outcome.name, "hostile-regulator");
    assert_eq!(store::read_artifacts(tmp.path(), SLUG).unwrap().len(), 1);
}

#[tokio::test]
async fn an_unusable_answer_or_a_dead_backend_writes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    seed(tmp.path());
    let mock = spawn(
        &["llama3.2"],
        ChatScript::Tokens(vec!["no skill here".into()]),
    )
    .await;
    assert!(run(&mock.url, tmp.path()).await.is_err());
    assert_eq!(mock.chat_bodies().len(), 2, "the one retry, then give up");
    assert!(store::read_artifacts(tmp.path(), SLUG).unwrap().is_empty());

    assert!(run(&refused_url().await, tmp.path()).await.is_err());
    assert!(store::read_artifacts(tmp.path(), SLUG).unwrap().is_empty());
    assert_eq!(
        store::read_conversation(tmp.path(), SLUG).unwrap(),
        CONVERSATION
    );
}

#[tokio::test]
async fn a_second_draft_of_the_same_name_gets_its_own_artifact() {
    let tmp = tempfile::tempdir().unwrap();
    seed(tmp.path());
    let mock = spawn(&["llama3.2"], ChatScript::Tokens(vec![GOOD_DRAFT.into()])).await;
    let first = run(&mock.url, tmp.path()).await.unwrap();
    let second = run(&mock.url, tmp.path()).await.unwrap();
    assert_ne!(first.artifact_slug, second.artifact_slug);
    assert_eq!(store::read_artifacts(tmp.path(), SLUG).unwrap().len(), 2);
}
