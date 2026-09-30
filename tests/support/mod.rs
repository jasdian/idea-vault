//! Scriptable mock Ollama server (docs/10-testing-strategy.md): a real localhost HTTP listener
//! implementing `/api/tags` and `/api/chat` (streaming NDJSON, or a single JSON object for a
//! `"stream": false` tool-loop round), scriptable to return tokens, stall (timeout tests), or
//! cut the connection early. This is the seam that keeps the whole suite offline and
//! deterministic — AI paths are never tested against a live model.
#![allow(dead_code)] // compiled once per test binary; not every binary uses every helper

pub mod gate;
pub mod web;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// What the mock's `/api/chat` endpoint does after the request arrives.
#[derive(Debug, Clone)]
pub enum ChatScript {
    /// Stream each token as one NDJSON line, then `{done:true}`.
    Tokens(Vec<String>),
    /// Like `Tokens`, but hold the response open for `delay_ms` first — makes concurrent calls
    /// overlap deterministically for max-in-flight instrumentation (docs/10).
    TokensAfterDelay { tokens: Vec<String>, delay_ms: u64 },
    /// Stream N filler tokens (`tok0`, `tok1`, …) then go silent without ever finishing —
    /// exercises the hard inactivity timeout.
    StallAfter(usize),
    /// Stream each token, then close the connection WITHOUT sending `{done:true}` —
    /// exercises the incomplete-stream protocol error.
    EofAfter(Vec<String>),
    /// Answer a `"stream": false` tool-loop round by asking to call tool `name` with `arguments`
    /// (ADR-0017/0021). A streaming request gets an empty completed stream.
    ToolCall {
        name: String,
        arguments: serde_json::Value,
    },
    /// Like `Tokens`, but the terminal `done` line (or the single non-streaming body) carries
    /// `done_reason` and the eval counts, as a real Ollama's does (docs/adr/0037).
    Finished {
        tokens: Vec<String>,
        done_reason: String,
        prompt_eval_count: u64,
        eval_count: u64,
    },
}

/// The terminal fields a [`ChatScript::Finished`] adds to its `done` line.
fn finished_fields(script: &ChatScript) -> serde_json::Map<String, serde_json::Value> {
    let mut m = serde_json::Map::new();
    if let ChatScript::Finished {
        done_reason,
        prompt_eval_count,
        eval_count,
        ..
    } = script
    {
        m.insert("done_reason".into(), serde_json::json!(done_reason));
        m.insert(
            "prompt_eval_count".into(),
            serde_json::json!(prompt_eval_count),
        );
        m.insert("eval_count".into(), serde_json::json!(eval_count));
    }
    m
}

pub struct MockOllama {
    pub url: String,
    /// Raw bodies of every `/api/chat` request received, in arrival order — lets tests assert
    /// what context/prompt actually reached the model.
    pub chat_requests: Arc<Mutex<Vec<String>>>,
    /// Concurrency gauge over `/api/chat`: (currently in flight, max ever observed) — the
    /// instrumentation behind the max-in-flight == K keystone test (docs/10, ADR-0006).
    chat_in_flight: Arc<AtomicUsize>,
    chat_max_in_flight: Arc<AtomicUsize>,
}

impl MockOllama {
    pub fn chat_bodies(&self) -> Vec<String> {
        self.chat_requests.lock().unwrap().clone()
    }

    /// Highest number of `/api/chat` requests the mock ever served simultaneously.
    pub fn max_in_flight(&self) -> usize {
        self.chat_max_in_flight.load(Ordering::SeqCst)
    }
}

/// Bind a listener and immediately drop it: connecting to the returned URL is refused fast.
pub async fn refused_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{addr}")
}

/// Spawn the mock server. `models` populates `/api/tags`; `script` drives every `/api/chat`.
/// `POST /api/show` answers 404 (like unknown paths), so context-window discovery falls back —
/// existing tests keep the pre-dynamic-budget 16 KiB behavior.
pub async fn spawn(models: &[&str], script: ChatScript) -> MockOllama {
    spawn_inner(models, Scripts::Repeat(script), None).await
}

/// Spawn a mock whose `/api/chat` answers consume `scripts` in order — call 1 gets scripts[0],
/// call 2 gets scripts[1], … For multi-call flows (consolidate then extract, D12). A call past
/// the end of the sequence answers with an empty completed stream.
pub async fn spawn_sequence(models: &[&str], scripts: Vec<ChatScript>) -> MockOllama {
    spawn_inner(
        models,
        Scripts::Sequence(Arc::new(Mutex::new(scripts.into()))),
        None,
    )
    .await
}

/// Like [`spawn`], but `POST /api/show` advertises `context_length` (arch-prefixed, the real
/// Ollama shape) — for dynamic-budget tests.
pub async fn spawn_with_context_length(
    models: &[&str],
    script: ChatScript,
    context_length: u64,
) -> MockOllama {
    spawn_inner(models, Scripts::Repeat(script), Some(context_length)).await
}

#[derive(Clone)]
enum Scripts {
    Repeat(ChatScript),
    Sequence(Arc<Mutex<VecDeque<ChatScript>>>),
}

impl Scripts {
    fn next(&self) -> ChatScript {
        match self {
            Scripts::Repeat(script) => script.clone(),
            Scripts::Sequence(queue) => queue
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(ChatScript::Tokens(Vec::new())),
        }
    }
}

async fn spawn_inner(
    models: &[&str],
    scripts: Scripts,
    show_context_length: Option<u64>,
) -> MockOllama {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let tags_body = serde_json::json!({
        "models": models.iter().map(|m| serde_json::json!({"name": m})).collect::<Vec<_>>(),
    })
    .to_string();
    let chat_requests: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let chat_requests_srv = chat_requests.clone();
    let chat_in_flight = Arc::new(AtomicUsize::new(0));
    let chat_max_in_flight = Arc::new(AtomicUsize::new(0));
    let gauge_srv = (chat_in_flight.clone(), chat_max_in_flight.clone());

    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else {
                break;
            };
            let tags_body = tags_body.clone();
            let scripts = scripts.clone();
            let chat_requests = chat_requests_srv.clone();
            let gauge = gauge_srv.clone();
            tokio::spawn(async move {
                let _ = handle(
                    sock,
                    tags_body,
                    scripts,
                    chat_requests,
                    gauge,
                    show_context_length,
                )
                .await;
            });
        }
    });

    MockOllama {
        url: format!("http://{addr}"),
        chat_requests,
        chat_in_flight,
        chat_max_in_flight,
    }
}

/// Decrements the in-flight gauge on drop, so early returns and errors can't leak a count.
struct InFlightGuard(Arc<AtomicUsize>);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn handle(
    mut sock: TcpStream,
    tags_body: String,
    scripts: Scripts,
    chat_requests: Arc<Mutex<Vec<String>>>,
    gauge: (Arc<AtomicUsize>, Arc<AtomicUsize>),
    show_context_length: Option<u64>,
) -> std::io::Result<()> {
    // Read until the end of headers.
    let mut req = Vec::new();
    let mut byte = [0u8; 1024];
    while !req.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = sock.read(&mut byte).await?;
        if n == 0 {
            return Ok(());
        }
        req.extend_from_slice(&byte[..n]);
        if req.len() > 1024 * 1024 {
            return Ok(());
        }
    }
    let header_end = req
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("checked above")
        + 4;
    let head = String::from_utf8_lossy(&req[..header_end]).to_string();
    let request_line = head.lines().next().unwrap_or_default().to_string();

    // Read the full body per Content-Length so tests can assert what the client sent.
    let content_length = head
        .lines()
        .find_map(|l| {
            let (name, value) = l.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    let mut body = req[header_end..].to_vec();
    while body.len() < content_length {
        let n = sock.read(&mut byte).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&byte[..n]);
    }

    if request_line.starts_with("GET /api/tags") {
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            tags_body.len(),
            tags_body
        );
        sock.write_all(resp.as_bytes()).await?;
        return Ok(());
    }

    if request_line.starts_with("POST /api/show") {
        // Scriptable model metadata; unscripted mocks fall through to 404 (fail-safe fallback).
        if let Some(ctx) = show_context_length {
            let body = serde_json::json!({
                "model_info": { "llama.context_length": ctx }
            })
            .to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            sock.write_all(resp.as_bytes()).await?;
            return Ok(());
        }
    }

    if request_line.starts_with("POST /api/chat") {
        chat_requests
            .lock()
            .unwrap()
            .push(String::from_utf8_lossy(&body).into_owned());

        // Gauge: track how many chat requests are being served simultaneously.
        let (in_flight, max_in_flight) = &gauge;
        let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        max_in_flight.fetch_max(now, Ordering::SeqCst);
        let _guard = InFlightGuard(in_flight.clone());

        let script = scripts.next();

        // A `"stream": false` request is a tool-loop round (`OllamaClient::chat_tools`,
        // ADR-0017/0021): the client parses the response body as ONE JSON object, so a Tokens
        // script answers as a single non-streaming completion (tokens concatenated — the same
        // text a streaming consumer would assemble). Stall/Eof keep their raw-socket behavior:
        // they script transport failures, which look the same to both paths.
        let wants_stream =
            serde_json::from_str::<serde_json::Value>(String::from_utf8_lossy(&body).as_ref())
                .ok()
                .and_then(|v| v.get("stream").and_then(serde_json::Value::as_bool))
                .unwrap_or(true);
        if !wants_stream {
            if let ChatScript::ToolCall { name, arguments } = &script {
                let call = serde_json::json!({"function": {"name": name, "arguments": arguments}});
                let payload = serde_json::json!({
                    "message": {"role": "assistant", "content": "", "tool_calls": [call]},
                    "done": true,
                })
                .to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    payload.len(),
                    payload
                );
                sock.write_all(resp.as_bytes()).await?;
                return Ok(());
            }
            let tokens = match &script {
                ChatScript::Tokens(tokens) | ChatScript::Finished { tokens, .. } => {
                    Some(tokens.clone())
                }
                ChatScript::TokensAfterDelay { tokens, delay_ms } => {
                    tokio::time::sleep(Duration::from_millis(*delay_ms)).await;
                    Some(tokens.clone())
                }
                ChatScript::StallAfter(_)
                | ChatScript::EofAfter(_)
                | ChatScript::ToolCall { .. } => None,
            };
            if let Some(tokens) = tokens {
                let mut payload = serde_json::json!({
                    "message": {"role": "assistant", "content": tokens.concat()},
                    "done": true,
                });
                payload
                    .as_object_mut()
                    .unwrap()
                    .extend(finished_fields(&script));
                let payload = payload.to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    payload.len(),
                    payload
                );
                sock.write_all(resp.as_bytes()).await?;
                return Ok(());
            }
        }

        sock.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nConnection: close\r\n\r\n",
        )
        .await?;

        let token_line = |content: &str| {
            format!(
                "{}\n",
                serde_json::json!({"message": {"content": content}, "done": false})
            )
        };
        match script {
            ChatScript::TokensAfterDelay { tokens, delay_ms } => {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                for t in &tokens {
                    sock.write_all(token_line(t).as_bytes()).await?;
                    sock.flush().await?;
                }
                sock.write_all(
                    format!(
                        "{}\n",
                        serde_json::json!({"message": {"content": ""}, "done": true})
                    )
                    .as_bytes(),
                )
                .await?;
                sock.flush().await?;
            }
            ChatScript::Tokens(ref tokens) | ChatScript::Finished { ref tokens, .. } => {
                for t in tokens {
                    sock.write_all(token_line(t).as_bytes()).await?;
                    sock.flush().await?;
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
                let mut done = serde_json::json!({"message": {"content": ""}, "done": true});
                done.as_object_mut()
                    .unwrap()
                    .extend(finished_fields(&script));
                sock.write_all(format!("{done}\n").as_bytes()).await?;
                sock.flush().await?;
            }
            ChatScript::StallAfter(n) => {
                for i in 0..n {
                    sock.write_all(token_line(&format!("tok{i}")).as_bytes())
                        .await?;
                    sock.flush().await?;
                }
                // Go silent, holding the connection open far longer than any test timeout.
                tokio::time::sleep(Duration::from_secs(600)).await;
            }
            ChatScript::EofAfter(tokens) => {
                for t in &tokens {
                    sock.write_all(token_line(t).as_bytes()).await?;
                    sock.flush().await?;
                }
                // Drop the socket without `{done:true}`.
            }
            ChatScript::ToolCall { .. } => {
                sock.write_all(
                    format!(
                        "{}\n",
                        serde_json::json!({"message": {"content": ""}, "done": true})
                    )
                    .as_bytes(),
                )
                .await?;
                sock.flush().await?;
            }
        }
        return Ok(());
    }

    sock.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        .await?;
    Ok(())
}
