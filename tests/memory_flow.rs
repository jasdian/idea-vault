//! D12/D13 memory pipeline tests against the mock Ollama: store extracts + consolidates,
//! re-store merges + dedupes (memory only grows), AI failure leaves truth untouched, and
//! reopen-time load assembles MEMORY.md-first context under budget. No live model.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers outside #[test] fns; HTC-6 binds shipping code"
)]

mod support;

use std::path::Path;

use chrono::{TimeZone, Utc};
use idea_vault::ai::budget::ContextBudget;
use idea_vault::ai::{LlmBackend, OllamaClient};
use idea_vault::domain::{Idea, IdeaFrontmatter, IdeaState};
use idea_vault::memory::{extract, load};
use idea_vault::vault::store;
use support::{refused_url, spawn_sequence, ChatScript};

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
            body: "Original statement.\n".into(),
        },
    )
    .unwrap();
    store::append_conversation(
        vault,
        slug,
        "## user\nlet's dig in: we ship the first version solo\n",
    )
    .unwrap();
    store::append_conversation(
        vault,
        slug,
        "## assistant\ndigging — the buyers are small agencies who pay monthly\n",
    )
    .unwrap();
}

fn sem() -> std::sync::Arc<tokio::sync::Semaphore> {
    std::sync::Arc::new(tokio::sync::Semaphore::new(2))
}

fn tokens(text: &str) -> ChatScript {
    ChatScript::Tokens(vec![text.to_string()])
}

#[tokio::test]
async fn store_consolidates_extracts_and_rebuilds_memory_index() {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path(), "i");

    // Call 1 = consolidation, call 2 = fact extraction (D12 order).
    let mock = spawn_sequence(
        &["llama3.2"],
        vec![
            tokens("A sharper consolidated statement."),
            tokens(
                "FACT: Key insight\nQUOTE: \"we ship the first version solo\"\nIt links [[other-idea]].\n\
                 FACT: Second point\nQUOTE: \"small agencies who pay monthly\"\nBody two.\n",
            ),
        ],
    )
    .await;
    let client = LlmBackend::ollama_only(OllamaClient::new(mock.url.clone(), "llama3.2").unwrap());

    let convo_before = store::read_conversation(tmp.path(), "i").unwrap();
    let outcome =
        extract::extract_and_store(&client, &sem(), tmp.path(), "i", ContextBudget::new(4096))
            .await
            .unwrap();

    // Consolidated body written, state flipped to stored — canonical in frontmatter.
    let idea = store::read_idea(tmp.path(), "i").unwrap();
    assert_eq!(idea.body, "A sharper consolidated statement.\n");
    assert_eq!(idea.frontmatter.state, IdeaState::Stored);
    assert_eq!(outcome.new_facts, 2);
    assert_eq!(outcome.quarantined, 0);
    assert!(outcome.quarantine_artifact.is_none());

    // Consolidate THEN distil: the extraction call reads the rewritten statement.
    let bodies = mock.chat_bodies();
    assert!(!bodies[0].contains("A sharper consolidated statement."));
    assert!(bodies[1].contains("A sharper consolidated statement."));
    assert!(
        bodies[1].contains("QUOTE:"),
        "the evidence format is asked for"
    );

    // Facts on disk, MEMORY.md rebuilt, [[links]] mined into frontmatter.
    let facts = store::read_memory_facts(tmp.path(), "i").unwrap();
    assert_eq!(facts.len(), 2);
    assert_eq!(facts[0].frontmatter.slug, "key-insight");
    assert_eq!(facts[0].frontmatter.links, vec!["other-idea".to_string()]);
    let index = store::read_memory_index(tmp.path(), "i").unwrap();
    assert_eq!(index.entries.len(), 2);

    // Append-only invariant: the conversation was not touched by storing.
    assert_eq!(
        store::read_conversation(tmp.path(), "i").unwrap(),
        convo_before
    );
}

#[tokio::test]
async fn restore_updates_by_appending_dedupes_and_quarantines_ungrounded_facts() {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path(), "i");

    // First store: two facts.
    let mock = spawn_sequence(
        &["llama3.2"],
        vec![
            tokens("v1 statement."),
            tokens(
                "FACT: Key insight\nQUOTE: \"we ship the first version solo\"\nBody.\n\
                 FACT: Second point\nQUOTE: \"small agencies who pay monthly\"\nBody two.\n",
            ),
        ],
    )
    .await;
    let client = LlmBackend::ollama_only(OllamaClient::new(mock.url.clone(), "llama3.2").unwrap());
    extract::extract_and_store(&client, &sem(), tmp.path(), "i", ContextBudget::new(4096))
        .await
        .unwrap();
    let before = store::read_memory_facts(tmp.path(), "i").unwrap();
    let first_body = before
        .iter()
        .find(|f| f.frontmatter.slug == "key-insight")
        .unwrap()
        .body
        .clone();
    let second_body = before
        .iter()
        .find(|f| f.frontmatter.slug == "second-point")
        .unwrap()
        .body
        .clone();

    // Re-store (as after a Reopen): an UPDATE of an existing fact, an ADD that duplicates an
    // existing title (the slug backstop), a NOOP, a genuinely new fact, and one whose quote was
    // never said.
    let mock = spawn_sequence(
        &["llama3.2"],
        vec![
            tokens("v2 statement."),
            tokens(
                "FACT: Key insight\nOP: UPDATE key-insight\nQUOTE: \"we ship the first version solo\"\nStill true after review.\n\
                 FACT: Second point\nQUOTE: \"small agencies who pay monthly\"\nRe-extracted duplicate.\n\
                 FACT: Nothing new\nOP: NOOP\n\
                 FACT: Fresh angle\nQUOTE: \"let's dig in: we ship\"\nNew body.\n\
                 FACT: Invented\nQUOTE: \"we raise a seed round in March\"\nNever said.\n",
            ),
        ],
    )
    .await;
    let client = LlmBackend::ollama_only(OllamaClient::new(mock.url.clone(), "llama3.2").unwrap());
    let outcome =
        extract::extract_and_store(&client, &sem(), tmp.path(), "i", ContextBudget::new(4096))
            .await
            .unwrap();

    // The extractor was shown what memory already holds, by slug.
    assert!(mock.chat_bodies()[1].contains("[[key-insight]] — Key insight"));

    assert_eq!(outcome.new_facts, 1);
    assert_eq!(outcome.updated_facts, 1);
    assert_eq!(outcome.quarantined, 1);
    let facts = store::read_memory_facts(tmp.path(), "i").unwrap();
    assert_eq!(facts.len(), 3, "memory only grows");
    let key = facts
        .iter()
        .find(|f| f.frontmatter.slug == "key-insight")
        .unwrap();
    assert!(
        key.body.starts_with(first_body.trim_end()),
        "UPDATE appends; the prior text is kept verbatim:\n{}",
        key.body
    );
    assert!(key.body.contains("Still true after review."));
    let second = facts
        .iter()
        .find(|f| f.frontmatter.slug == "second-point")
        .unwrap();
    assert_eq!(second.body, second_body, "a duplicate ADD never rewrites");
    assert!(facts.iter().any(|f| f.frontmatter.slug == "fresh-angle"));
    assert!(!facts.iter().any(|f| f.frontmatter.slug == "invented"));

    // The ungrounded fact is kept where the owner can see it, never in memory.
    let artifact_slug = outcome.quarantine_artifact.expect("quarantine written");
    let artifacts = store::read_artifacts(tmp.path(), "i").unwrap();
    let quarantine = artifacts
        .iter()
        .find(|a| a.frontmatter.slug == artifact_slug)
        .unwrap();
    assert_eq!(
        quarantine.frontmatter.kind,
        idea_vault::domain::ArtifactKind::Quarantine
    );
    assert!(quarantine.body.contains("### Invented"));
    assert!(quarantine.body.contains("we raise a seed round in March"));
    assert_eq!(
        store::read_memory_index(tmp.path(), "i")
            .unwrap()
            .entries
            .len(),
        3
    );
}

#[tokio::test]
async fn facts_without_a_quote_are_quarantined_not_remembered() {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path(), "i");
    let mock = spawn_sequence(
        &["llama3.2"],
        vec![
            tokens("Statement."),
            tokens("FACT: Unsupported\nA claim with no quote.\n"),
        ],
    )
    .await;
    let client = LlmBackend::ollama_only(OllamaClient::new(mock.url.clone(), "llama3.2").unwrap());
    let outcome =
        extract::extract_and_store(&client, &sem(), tmp.path(), "i", ContextBudget::new(4096))
            .await
            .unwrap();
    assert_eq!((outcome.new_facts, outcome.quarantined), (0, 1));
    assert!(store::read_memory_facts(tmp.path(), "i")
        .unwrap()
        .is_empty());
    // Still stored: the gate withholds facts, it never blocks the transition.
    assert_eq!(
        store::read_idea(tmp.path(), "i").unwrap().frontmatter.state,
        IdeaState::Stored
    );
}

#[tokio::test]
async fn unreachable_model_aborts_store_with_truth_untouched() {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path(), "i");
    let before_idea = store::read_idea(tmp.path(), "i").unwrap();

    let client =
        LlmBackend::ollama_only(OllamaClient::new(refused_url().await, "llama3.2").unwrap());
    let err =
        extract::extract_and_store(&client, &sem(), tmp.path(), "i", ContextBudget::new(4096))
            .await;
    assert!(err.is_err());

    // Nothing written: body, state, and (absence of) memory all unchanged.
    assert_eq!(store::read_idea(tmp.path(), "i").unwrap(), before_idea);
    assert!(store::read_memory_facts(tmp.path(), "i")
        .unwrap()
        .is_empty());
    assert!(!tmp.path().join("i/MEMORY.md").exists());
}

#[tokio::test]
async fn load_context_is_memory_first_and_truth_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path(), "i");
    // Fact body: first line becomes the MEMORY.md summary; the second line exists only in the
    // full fact body — so the two inclusion levels are distinguishable below.
    let mock = spawn_sequence(
        &["llama3.2"],
        vec![
            tokens("Stored statement."),
            tokens(
                "FACT: Key insight\nQUOTE: \"we ship the first version solo\"\nSummary line.\nDeep detail sentence.\n",
            ),
        ],
    )
    .await;
    let client = LlmBackend::ollama_only(OllamaClient::new(mock.url.clone(), "llama3.2").unwrap());
    extract::extract_and_store(&client, &sem(), tmp.path(), "i", ContextBudget::new(4096))
        .await
        .unwrap();
    let idea_before = store::read_idea(tmp.path(), "i").unwrap();

    // Generous budget: index line, full fact body, and conversation all present.
    let ctx = load::load_context(tmp.path(), "i", ContextBudget::new(8192)).unwrap();
    assert!(ctx.text.contains("Stored statement."));
    assert!(
        ctx.text.contains("[[key-insight]]"),
        "MEMORY.md index line loaded"
    );
    assert!(
        ctx.text.contains("Deep detail sentence."),
        "full fact body pulled in"
    );
    assert!(
        ctx.text.contains("let's dig in"),
        "recent conversation included"
    );
    assert!(!ctx.truncated);

    // Tight budget: body always survives; index lines outrank fact bodies (index-first, D13).
    let tight = load::load_context(tmp.path(), "i", ContextBudget::new(120)).unwrap();
    assert!(tight.text.contains("Stored statement."));
    assert!(tight.truncated);
    // Index-first (D13): at this budget the MEMORY.md line fits — the assertion must not be
    // conditional, or a byte-accounting refactor could silently stop testing ordering.
    assert!(
        tight.included_memory > 0,
        "index line must fit at 120 bytes"
    );
    assert!(tight.text.contains("[[key-insight]]"), "index line present");
    assert!(
        !tight.text.contains("Deep detail sentence."),
        "full fact body excluded at tight budget"
    );

    // Truth-idempotent: loading changed nothing on disk.
    assert_eq!(store::read_idea(tmp.path(), "i").unwrap(), idea_before);
}
