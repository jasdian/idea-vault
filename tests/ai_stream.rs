//! AI streaming tests against the scriptable mock Ollama (docs/10-testing-strategy.md):
//! tokens-then-done, hard timeout on stall, protocol error on early EOF, connection refused,
//! and probe health states. No live model, no external network — everything is loopback.

mod support;

use std::time::Duration;

use futures::StreamExt;
use idea_vault::ai::ollama::ChatMessage;
use idea_vault::ai::{AiError, AiHealth, OllamaClient};
use support::{refused_url, spawn, ChatScript};

fn msg(content: &str) -> Vec<ChatMessage> {
    vec![ChatMessage {
        role: "user".to_string(),
        content: content.to_string(),
    }]
}

#[tokio::test]
async fn streams_tokens_in_order_then_ends_after_done() {
    let mock = spawn(
        &["llama3.2"],
        ChatScript::Tokens(vec!["Hel".into(), "lo ".into(), "world".into()]),
    )
    .await;
    let client = OllamaClient::new(mock.url.clone(), "llama3.2").unwrap();

    let mut stream = client.chat_stream(msg("hi")).await.unwrap();
    let mut tokens = Vec::new();
    while let Some(item) = stream.next().await {
        tokens.push(item.expect("no errors in a clean scripted stream"));
    }
    assert_eq!(tokens, ["Hel", "lo ", "world"]);
}

#[tokio::test]
async fn stalled_stream_hits_hard_timeout_after_partial_tokens() {
    let mock = spawn(&["llama3.2"], ChatScript::StallAfter(1)).await;
    let client = OllamaClient::new(mock.url.clone(), "llama3.2")
        .unwrap()
        .with_token_timeout(Duration::from_millis(200));

    let mut stream = client.chat_stream(msg("hi")).await.unwrap();
    // The one scripted token arrives …
    assert_eq!(stream.next().await.unwrap().unwrap(), "tok0");
    // … then silence: the hard timeout must fire (bounded, no hang) and be terminal.
    assert!(matches!(
        stream.next().await.unwrap().unwrap_err(),
        AiError::Timeout
    ));
    assert!(stream.next().await.is_none(), "error items are terminal");
}

#[tokio::test]
async fn eof_before_done_is_a_protocol_error_not_a_clean_end() {
    // Persist boundary (D11): a stream that dies before `done:true` must error so the caller
    // never persists the partial assistant turn as truth.
    let mock = spawn(&["llama3.2"], ChatScript::EofAfter(vec!["partial".into()])).await;
    let client = OllamaClient::new(mock.url.clone(), "llama3.2").unwrap();

    let mut stream = client.chat_stream(msg("hi")).await.unwrap();
    assert_eq!(stream.next().await.unwrap().unwrap(), "partial");
    assert!(matches!(
        stream.next().await.unwrap().unwrap_err(),
        AiError::Protocol(_)
    ));
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn refused_connection_errors_at_send_not_hang() {
    let url = refused_url().await;
    let client = OllamaClient::new(url, "llama3.2").unwrap();
    match client.chat_stream(msg("hi")).await {
        Err(AiError::Http(_)) => {}
        Err(other) => panic!("expected Http error, got: {other}"),
        Ok(_) => panic!("expected refused connection to error"),
    }
}

#[tokio::test]
async fn probe_reports_available_and_model_missing_against_mock() {
    // Model present (with implicit :latest tag) → Available.
    let mock = spawn(&["llama3.2:latest"], ChatScript::Tokens(vec![])).await;
    let client = OllamaClient::new(mock.url.clone(), "llama3.2").unwrap();
    assert_eq!(client.probe().await, AiHealth::Available);

    // Server up but the configured model is not pulled → ModelMissing (D20).
    let mock = spawn(&["mistral"], ChatScript::Tokens(vec![])).await;
    let client = OllamaClient::new(mock.url.clone(), "llama3.2").unwrap();
    assert_eq!(client.probe().await, AiHealth::ModelMissing);
}
