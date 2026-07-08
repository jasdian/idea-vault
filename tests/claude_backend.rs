//! claude-code backend tests (docs/adr/0009) against a fake `claude` CLI script that emits canned
//! `stream-json`. No real Claude, no network. Proves the streaming/parse contract; the persist
//! boundaries above the backend are identical to the Ollama path (same code in web::routes::chat).

use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use idea_vault::ai::claude_code::{ClaudeCodeClient, ClaudeCodeConfig};
use idea_vault::ai::ollama::ChatMessage;
use idea_vault::ai::{AiError, AiHealth};

fn fake_claude() -> String {
    format!(
        "{}/tests/fixtures/fake-claude.sh",
        env!("CARGO_MANIFEST_DIR")
    )
}

/// Build a client pointed at the fake, selecting its behavior via the `model` field (which the
/// fake reads from `--model`).
fn client(binary: &str, mode: Option<&str>) -> ClaudeCodeClient {
    ClaudeCodeClient::new(ClaudeCodeConfig {
        binary: binary.to_string(),
        cwd: PathBuf::from("."),
        add_dirs: Vec::new(),
        allowed_tools: Vec::new(),
        disallowed_tools: Vec::new(),
        model: mode.map(str::to_string),
        system_prompt: None,
        skip_permissions: true,
        token_timeout: Duration::from_secs(10),
    })
}

fn msg(text: &str) -> Vec<ChatMessage> {
    vec![ChatMessage {
        role: "user".into(),
        content: text.into(),
    }]
}

#[tokio::test]
async fn streams_text_deltas_and_ignores_tool_events() {
    let c = client(&fake_claude(), Some("tokens"));
    let mut stream = c.chat_stream(msg("hi")).await.unwrap();
    let mut tokens = Vec::new();
    while let Some(item) = stream.next().await {
        tokens.push(item.expect("clean stream has no errors"));
    }
    // The tool_use and system/init lines are consumed silently; only prose deltas surface.
    assert_eq!(tokens, ["Hello ", "world"]);
}

#[tokio::test]
async fn chat_concatenates_the_stream() {
    let c = client(&fake_claude(), Some("tokens"));
    assert_eq!(c.chat(msg("hi")).await.unwrap(), "Hello world");
}

#[tokio::test]
async fn result_only_falls_back_to_result_text() {
    // No streaming deltas, just a terminal result — the text must not be lost.
    let c = client(&fake_claude(), Some("resulttext"));
    assert_eq!(c.chat(msg("hi")).await.unwrap(), "whole answer");
}

#[tokio::test]
async fn eof_before_result_is_a_terminal_backend_error() {
    // The fake streams one token then exits without a `result` — the partial must surface an error
    // (so the caller persists nothing), not a clean end.
    let c = client(&fake_claude(), Some("eof"));
    let mut stream = c.chat_stream(msg("hi")).await.unwrap();
    assert_eq!(stream.next().await.unwrap().unwrap(), "partial");
    assert!(matches!(
        stream.next().await.unwrap().unwrap_err(),
        AiError::Backend(_)
    ));
    assert!(stream.next().await.is_none(), "error is terminal");
}

#[tokio::test]
async fn auth_failure_surfaces_as_backend_error() {
    let c = client(&fake_claude(), Some("auth"));
    match c.chat(msg("hi")).await {
        Err(AiError::Backend(detail)) => assert!(detail.contains("401")),
        other => panic!("expected auth backend error, got {other:?}"),
    }
}

#[tokio::test]
async fn probe_available_when_binary_runs_unreachable_otherwise() {
    assert_eq!(
        client(&fake_claude(), None).probe().await,
        AiHealth::Available
    );
    assert_eq!(
        client("/nonexistent/claude-binary", None).probe().await,
        AiHealth::Unreachable
    );
}

#[tokio::test]
async fn spawn_failure_is_a_backend_error() {
    let c = client("/nonexistent/claude-binary", Some("tokens"));
    match c.chat_stream(msg("hi")).await {
        Err(AiError::Backend(_)) => {}
        Err(other) => panic!("expected spawn Backend error, got {other:?}"),
        Ok(_) => panic!("expected spawn to fail"),
    }
}
