//! The run journal end to end (docs/adr/0037, D39): every AI job writes an append-only
//! `vault/<slug>/.runs/<run_id>.jsonl` that opens with `RunStarted`, records each call verbatim
//! with its tool rounds and contract outcome, and ends with `RunFinished` — cancelled when the
//! owner cancels. The journal is diagnostics only: a journal that cannot be written never fails
//! the turn, reindex never reads it, and fork never copies it. Mock Ollama only.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers outside #[test] fns; HTC-6 binds shipping code"
)]

mod support;

use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::http::StatusCode;
use chrono::{TimeZone, Utc};
use idea_vault::ai::journal::{self, JournalEntry, RunKind, RunOutcome};
use idea_vault::app::AppState;
use idea_vault::domain::{Idea, IdeaFrontmatter, IdeaState};
use idea_vault::sources::SourceConfig;
use idea_vault::vault::store;
use support::web::{get, poll_until, post_form, test_state_with_ollama};
use support::{spawn, spawn_sequence, ChatScript};

fn seed(vault: &Path, slug: &str, sources: Vec<String>) {
    store::write_idea(
        vault,
        &Idea {
            frontmatter: IdeaFrontmatter {
                title: "Journaled".into(),
                slug: slug.into(),
                state: IdeaState::InDiscussion,
                tags: vec![],
                sources,
                created: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
                updated: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
            },
            body: "The idea body.\n".into(),
        },
    )
    .unwrap();
    store::append_turn(vault, slug, "user", "attack it").unwrap();
}

/// Every journal of `slug`, oldest first.
fn journals(vault: &Path, slug: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(journal::runs_dir(vault, slug)) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries.map(|e| e.unwrap().path()).collect();
    paths.sort();
    paths
}

/// Wait until the idea's only journal has its `RunFinished` line (written when the job's slot
/// settles, a moment after the transcript lands), and return its entries.
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
    panic!(
        "no finished run journal for {slug}: {:?}",
        journals(vault, slug)
    );
}

fn kind_of(entry: &JournalEntry) -> &'static str {
    match entry {
        JournalEntry::RunStarted { .. } => "run_started",
        JournalEntry::LlmCall { .. } => "llm_call",
        JournalEntry::ToolCall { .. } => "tool_call",
        JournalEntry::Verdict { .. } => "verdict",
        JournalEntry::Contract { .. } => "contract",
        JournalEntry::RunFinished { .. } => "run_finished",
    }
}

#[tokio::test]
async fn skill_job_writes_started_llmcall_contract_finished() {
    let mock = spawn(
        &["llama3.2"],
        ChatScript::Tokens(vec!["1. Nobody pays.".into()]),
    )
    .await;
    let (state, vault) = test_state_with_ollama(&mock.url, 1);
    seed(&vault, "skilled", vec![]);

    let (status, _) = post_form(state.clone(), "/idea/skilled/skill/premortem", "").await;
    assert_eq!(status, StatusCode::OK);
    poll_until(state, "/idea/skilled/pending", "Nobody pays").await;

    let entries = finished_run(&vault, "skilled").await;
    let kinds: Vec<&str> = entries.iter().map(kind_of).collect();
    assert_eq!(
        kinds,
        [
            "run_started",
            "llm_call",
            "verdict",
            "contract",
            "run_finished"
        ]
    );
    match &entries[0] {
        JournalEntry::RunStarted {
            format_version,
            run_id,
            slug,
            kind,
            ..
        } => {
            assert_eq!(*format_version, journal::FORMAT_VERSION);
            assert_eq!(slug, "skilled");
            assert_eq!(*kind, RunKind::Skill);
            assert!(run_id.ends_with("Z-skill"), "{run_id}");
        }
        other => panic!("{other:?}"),
    }
    match &entries[1] {
        JournalEntry::LlmCall {
            seq,
            role,
            backend,
            response_text,
            temperature_milli,
            meta,
            ..
        } => {
            assert_eq!(*seq, 1);
            assert_eq!(role.as_deref(), Some("critic"));
            assert_eq!(backend, "ollama");
            assert_eq!(response_text, "1. Nobody pays.", "verbatim");
            assert_eq!(*temperature_milli, Some(700));
            assert_eq!(meta.usage.api_calls, 1);
            assert!(meta.num_ctx.is_some(), "the window sent is recorded");
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        &entries[4],
        JournalEntry::RunFinished {
            outcome: RunOutcome::Done,
            llm_calls: 1,
            ..
        }
    ));
}

#[tokio::test]
async fn cancelled_job_leaves_run_finished_cancelled() {
    let mock = spawn(&["llama3.2"], ChatScript::StallAfter(1)).await;
    let (state, vault) = test_state_with_ollama(&mock.url, 1);
    seed(&vault, "stalled", vec![]);

    let (status, _) = post_form(state.clone(), "/idea/stalled/chat", "message=hello").await;
    assert_eq!(status, StatusCode::OK);
    poll_until(state.clone(), "/idea/stalled/pending", "foil-pending").await;
    let (status, _) = post_form(state, "/idea/stalled/cancel", "").await;
    assert_eq!(status, StatusCode::OK);

    let entries = finished_run(&vault, "stalled").await;
    assert!(
        matches!(
            entries.last(),
            Some(JournalEntry::RunFinished {
                outcome: RunOutcome::Cancelled,
                llm_calls: 0,
                ..
            })
        ),
        "{entries:?}"
    );
    assert!(matches!(
        &entries[0],
        JournalEntry::RunStarted {
            kind: RunKind::Chat,
            ..
        }
    ));
}

fn register_source(state: &AppState) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("note.md"), "real notes\n").unwrap();
    state
        .sources
        .add(SourceConfig {
            name: idea_vault::domain::Name::try_from("refs").unwrap(),
            host_path: dir.path().to_path_buf(),
        })
        .unwrap();
    std::mem::forget(dir);
}

#[tokio::test]
async fn tool_loop_rounds_count_as_api_calls() {
    let read = || ChatScript::ToolCall {
        name: "source_read".into(),
        arguments: serde_json::json!({"source": "refs", "file": "note.md"}),
    };
    let mock = spawn_sequence(
        &["llama3.2"],
        vec![
            read(),
            read(),
            read(),
            ChatScript::Tokens(vec!["Weighed reply".into()]),
        ],
    )
    .await;
    let (state, vault) = test_state_with_ollama(&mock.url, 1);
    register_source(&state);
    seed(&vault, "tooled", vec!["refs".into()]);

    let (status, _) = post_form(state.clone(), "/idea/tooled/chat", "message=check").await;
    assert_eq!(status, StatusCode::OK);
    poll_until(state, "/idea/tooled/pending", "Weighed reply").await;

    let entries = finished_run(&vault, "tooled").await;
    let (seq, meta) = entries
        .iter()
        .find_map(|e| match e {
            JournalEntry::LlmCall { seq, meta, .. } => Some((*seq, meta.clone())),
            _ => None,
        })
        .expect("the reply call is journaled");
    assert_eq!(meta.usage.api_calls, 4, "3 tool rounds + the answer");
    assert_eq!(mock.chat_bodies().len(), 4);
    let tools: Vec<(u32, u32, &str, &str)> = entries
        .iter()
        .filter_map(|e| match e {
            JournalEntry::ToolCall {
                call_seq,
                round,
                name,
                result_text,
                ..
            } => Some((*call_seq, *round, name.as_str(), result_text.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(tools.len(), 3, "{tools:?}");
    for (i, (call_seq, round, name, result)) in tools.into_iter().enumerate() {
        assert_eq!(call_seq, seq);
        assert_eq!(round as usize, i);
        assert_eq!(name, "source_read");
        assert!(result.contains("real notes"), "{result}");
    }
}

#[tokio::test]
async fn journal_open_failure_does_not_fail_turn() {
    let mock = spawn(&["llama3.2"], ChatScript::Tokens(vec!["Still here".into()])).await;
    let (state, vault) = test_state_with_ollama(&mock.url, 1);
    seed(&vault, "blocked", vec![]);
    // A file where the journal directory would go: the journal cannot be opened.
    std::fs::write(journal::runs_dir(&vault, "blocked"), "not a dir").unwrap();

    let (status, _) = post_form(state.clone(), "/idea/blocked/chat", "message=hello").await;
    assert_eq!(status, StatusCode::OK);
    poll_until(state, "/idea/blocked/pending", "Still here").await;
    let convo = store::read_conversation(&vault, "blocked").unwrap();
    assert!(convo.contains("## assistant\nStill here"), "{convo}");
}

#[tokio::test]
async fn reindex_ignores_runs_dir() {
    let mock = spawn(&["llama3.2"], ChatScript::Tokens(vec!["plain".into()])).await;
    let (state, vault) = test_state_with_ollama(&mock.url, 1);
    seed(&vault, "indexed", vec![]);
    // Journal-shaped files carrying a word nothing else in the vault holds, including a markdown
    // file, which reindex would read anywhere it was not told to skip.
    let runs = journal::runs_dir(&vault, "indexed");
    std::fs::create_dir_all(&runs).unwrap();
    std::fs::write(runs.join("20260101T000000000Z-chat.jsonl"), "zebracorn\n").unwrap();
    std::fs::write(
        runs.join("notes.md"),
        "---\ntitle: zebracorn\n---\nzebracorn\n",
    )
    .unwrap();

    let (status, _) = post_form(state.clone(), "/idea/indexed/chat", "message=hello").await;
    assert_eq!(status, StatusCode::OK);
    poll_until(state.clone(), "/idea/indexed/pending", "plain").await;

    let (status, results) = get(state, "/search?q=zebracorn").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!results.contains("/idea/indexed"), "{results}");
}

#[tokio::test]
async fn fork_does_not_copy_runs() {
    let mock = spawn(&["llama3.2"], ChatScript::Tokens(vec!["answered".into()])).await;
    let (state, vault) = test_state_with_ollama(&mock.url, 1);
    seed(&vault, "original", vec![]);

    let (status, _) = post_form(state.clone(), "/idea/original/chat", "message=hello").await;
    assert_eq!(status, StatusCode::OK);
    poll_until(state.clone(), "/idea/original/pending", "answered").await;
    finished_run(&vault, "original").await;

    let (status, _) = post_form(state, "/idea/original/fork", "").await;
    assert_eq!(status, StatusCode::OK);
    let fork = std::fs::read_dir(&vault)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.file_name().unwrap().to_string_lossy().contains("fork"))
        .expect("a fork was created");
    assert!(
        store::read_conversation(&vault, fork.file_name().unwrap().to_str().unwrap())
            .unwrap()
            .contains("answered"),
        "the fork carries the conversation"
    );
    assert!(
        !fork.join(journal::RUNS_DIR).exists(),
        "the fork carries no journal"
    );
}
