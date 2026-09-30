//! Verdict journaling at the parse sites, replayed (docs/adr/0038, D40): a skill, a swarm, a store
//! and a build plan each journal the verdict their parser reached beside the call it judged, and
//! `regrade` over that vault finds every verdict unchanged — the journaled line and today's line
//! come from the same summary function. Mock Ollama only.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers outside #[test] fns; HTC-6 binds shipping code"
)]

mod support;

use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::http::StatusCode;
use chrono::{TimeZone, Utc};
use idea_vault::ai::journal::{self, JournalEntry};
use idea_vault::domain::{Idea, IdeaFrontmatter, IdeaState};
use idea_vault::regrade::{self, Filter, RegradeStats};
use idea_vault::vault::store;
use support::web::{poll_until, post_form, test_state_with_ollama};
use support::{spawn, spawn_sequence, ChatScript};

fn seed(vault: &Path, slug: &str) {
    store::write_idea(
        vault,
        &Idea {
            frontmatter: IdeaFrontmatter {
                title: "Graded".into(),
                slug: slug.into(),
                state: IdeaState::InDiscussion,
                tags: vec![],
                sources: vec![],
                created: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
                updated: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
                extra: Default::default(),
            },
            body: "The idea body.\n".into(),
        },
    )
    .unwrap();
    store::append_turn(
        vault,
        slug,
        "user",
        "agencies pay monthly, and I want the cheapest disproof before any Rust exists",
    )
    .unwrap();
}

fn journals(vault: &Path, slug: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(journal::runs_dir(vault, slug)) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries.map(|e| e.unwrap().path()).collect();
    paths.sort();
    paths
}

/// The idea's only journal once its `RunFinished` line is written.
async fn finished_run(vault: &Path, slug: &str) -> Vec<JournalEntry> {
    for _ in 0..400 {
        if let [path] = journals(vault, slug).as_slice() {
            let entries = journal::read_run(path).unwrap();
            if matches!(entries.last(), Some(JournalEntry::RunFinished { .. })) {
                return entries;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no finished run journal for {slug}");
}

/// The parser families of the run's verdicts, each checked to point at a call in the run.
fn verdicts(entries: &[JournalEntry]) -> Vec<&'static str> {
    entries
        .iter()
        .filter_map(|e| match e {
            JournalEntry::Verdict {
                call_seq, parser, ..
            } => {
                assert!(
                    entries
                        .iter()
                        .any(|c| matches!(c, JournalEntry::LlmCall { seq, .. } if seq == call_seq)),
                    "verdict on call #{call_seq} has no call"
                );
                Some(parser.family())
            }
            _ => None,
        })
        .collect()
}

fn regrade_all(vault: &Path) -> RegradeStats {
    let mut out = Vec::new();
    let stats = regrade::run(vault, &Filter::default(), &mut out).unwrap();
    let out = String::from_utf8(out).unwrap();
    assert_eq!(stats.flips(), 0, "{out}");
    assert_eq!(stats.skipped, 0, "{out}");
    stats
}

#[tokio::test]
async fn skill_call_journals_a_contract_verdict_that_regrades_unchanged() {
    let mock = spawn(
        &["llama3.2"],
        ChatScript::Tokens(vec!["1. Nobody pays.".into()]),
    )
    .await;
    let (state, vault) = test_state_with_ollama(&mock.url, 1);
    seed(&vault, "skilled");
    let (status, _) = post_form(state.clone(), "/idea/skilled/skill/premortem", "").await;
    assert_eq!(status, StatusCode::OK);
    poll_until(state, "/idea/skilled/pending", "Nobody pays").await;

    let entries = finished_run(&vault, "skilled").await;
    assert_eq!(verdicts(&entries), ["contract"]);
    assert!(entries.iter().any(|e| matches!(
        e,
        JournalEntry::Verdict { summary, .. } if summary.starts_with("pass=1 ")
    )));
    assert_eq!(regrade_all(&vault).same, 1);
}

#[tokio::test]
async fn swarm_journals_lens_contracts_and_the_audit_verdict() {
    let mock = spawn(
        &["llama3.2"],
        ChatScript::Tokens(vec!["- converged finding".into()]),
    )
    .await;
    let (state, vault) = test_state_with_ollama(&mock.url, 2);
    seed(&vault, "swarmed");
    let (status, _) = post_form(state.clone(), "/idea/swarmed/swarm", "").await;
    assert_eq!(status, StatusCode::OK);
    poll_until(state, "/idea/swarmed/pending", "foil · swarm").await;

    let entries = finished_run(&vault, "swarmed").await;
    let families = verdicts(&entries);
    // The mock's non-verdict audit answer earns the one targeted re-ask (ADR-0023 amendment), and
    // each audit call journals its own verdict (ADR-0038): two audit verdicts.
    assert_eq!(families.iter().filter(|f| **f == "audit").count(), 2);
    assert_eq!(families.iter().filter(|f| **f == "contract").count(), 4);
    assert_eq!(regrade_all(&vault).same, 6);
}

#[tokio::test]
async fn store_journals_a_facts_verdict_that_survives_the_body_rewrite() {
    let mock = spawn_sequence(
        &["llama3.2"],
        vec![
            ChatScript::Tokens(vec!["Consolidated best statement.".into()]),
            ChatScript::Tokens(vec![
                "FACT: Durable point\nQUOTE: \"agencies pay monthly\"\nThe body.\n\
                 FACT: Invented\nQUOTE: \"never said\"\nMade up.\n"
                    .into(),
            ]),
        ],
    )
    .await;
    let (state, vault) = test_state_with_ollama(&mock.url, 1);
    seed(&vault, "stored");
    let (status, _) = post_form(state.clone(), "/idea/stored/store", "").await;
    assert_eq!(status, StatusCode::OK);
    poll_until(
        state,
        "/idea/stored/pending",
        "Consolidated best statement.",
    )
    .await;

    let entries = finished_run(&vault, "stored").await;
    assert_eq!(verdicts(&entries), ["facts"]);
    let summary = entries
        .iter()
        .find_map(|e| match e {
            JournalEntry::Verdict { summary, .. } => Some(summary.clone()),
            _ => None,
        })
        .unwrap();
    assert!(
        summary.contains("fact1=add:kept fact2=add:held"),
        "{summary}"
    );
    // The store replaced idea.md's body, yet the verdict still regrades: it kept the text.
    assert_ne!(
        store::read_idea(&vault, "stored").unwrap().body,
        "The idea body.\n"
    );
    assert_eq!(regrade_all(&vault).same, 1);
}

const PLANNER_ANSWER: &str = "## Goal
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

#[tokio::test]
async fn build_plan_journals_a_gates_verdict_on_the_planner_call() {
    let mock = spawn(
        &["llama3.2"],
        ChatScript::Tokens(vec![PLANNER_ANSWER.into()]),
    )
    .await;
    let (state, vault) = test_state_with_ollama(&mock.url, 1);
    seed(&vault, "planned");
    let (status, _) = post_form(state.clone(), "/idea/planned/skill/build-prompt", "").await;
    assert_eq!(status, StatusCode::OK);
    poll_until(state, "/idea/planned/pending", "build-plan").await;

    let entries = finished_run(&vault, "planned").await;
    assert_eq!(verdicts(&entries), ["contract", "plan-gates"]);
    let gates_on = entries.iter().find_map(|e| match e {
        JournalEntry::Verdict {
            call_seq,
            parser,
            haystack,
            ..
        } if parser.family() == "plan-gates" => {
            assert!(haystack.is_some(), "the gates name their evidence");
            Some(*call_seq)
        }
        _ => None,
    });
    assert_eq!(gates_on, Some(1), "the gates judged the planner's call");
    // The pointer turn appended after the plan leaves the recorded prefix intact.
    assert_eq!(regrade_all(&vault).same, 2);
}
