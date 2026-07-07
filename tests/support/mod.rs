//! Scriptable mock Ollama server (docs/10-testing-strategy.md): a real localhost HTTP listener
//! implementing `/api/tags` and streaming `/api/chat`, scriptable to return tokens, stall
//! (timeout tests), or cut the connection early. This is the seam that keeps the whole suite
//! offline and deterministic — AI paths are never tested against a live model.
#![allow(dead_code)] // compiled once per test binary; not every binary uses every helper

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// What the mock's `/api/chat` endpoint does after the request arrives.
#[derive(Debug, Clone)]
pub enum ChatScript {
    /// Stream each token as one NDJSON line, then `{done:true}`.
    Tokens(Vec<String>),
    /// Stream N filler tokens (`tok0`, `tok1`, …) then go silent without ever finishing —
    /// exercises the hard inactivity timeout.
    StallAfter(usize),
    /// Stream each token, then close the connection WITHOUT sending `{done:true}` —
    /// exercises the incomplete-stream protocol error.
    EofAfter(Vec<String>),
}

pub struct MockOllama {
    pub url: String,
}

/// Bind a listener and immediately drop it: connecting to the returned URL is refused fast.
pub async fn refused_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{addr}")
}

/// Spawn the mock server. `models` populates `/api/tags`; `script` drives every `/api/chat`.
pub async fn spawn(models: &[&str], script: ChatScript) -> MockOllama {
    spawn_inner(models, Scripts::Repeat(script)).await
}

/// Spawn a mock whose `/api/chat` answers consume `scripts` in order — call 1 gets scripts[0],
/// call 2 gets scripts[1], … For multi-call flows (consolidate then extract, D12). A call past
/// the end of the sequence answers with an empty completed stream.
pub async fn spawn_sequence(models: &[&str], scripts: Vec<ChatScript>) -> MockOllama {
    spawn_inner(
        models,
        Scripts::Sequence(Arc::new(Mutex::new(scripts.into()))),
    )
    .await
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

async fn spawn_inner(models: &[&str], scripts: Scripts) -> MockOllama {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let tags_body = serde_json::json!({
        "models": models.iter().map(|m| serde_json::json!({"name": m})).collect::<Vec<_>>(),
    })
    .to_string();

    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else {
                break;
            };
            let tags_body = tags_body.clone();
            let scripts = scripts.clone();
            tokio::spawn(async move {
                let _ = handle(sock, tags_body, scripts).await;
            });
        }
    });

    MockOllama {
        url: format!("http://{addr}"),
    }
}

async fn handle(mut sock: TcpStream, tags_body: String, scripts: Scripts) -> std::io::Result<()> {
    // Read until the end of headers; consume any body best-effort (the mock never inspects it).
    let mut req = Vec::new();
    let mut byte = [0u8; 1024];
    while !req.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = sock.read(&mut byte).await?;
        if n == 0 {
            return Ok(());
        }
        req.extend_from_slice(&byte[..n]);
        if req.len() > 64 * 1024 {
            return Ok(());
        }
    }
    let request_line = String::from_utf8_lossy(&req)
        .lines()
        .next()
        .unwrap_or_default()
        .to_string();

    if request_line.starts_with("GET /api/tags") {
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            tags_body.len(),
            tags_body
        );
        sock.write_all(resp.as_bytes()).await?;
        return Ok(());
    }

    if request_line.starts_with("POST /api/chat") {
        let script = scripts.next();
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
            ChatScript::Tokens(tokens) => {
                for t in &tokens {
                    sock.write_all(token_line(t).as_bytes()).await?;
                    sock.flush().await?;
                    tokio::time::sleep(Duration::from_millis(2)).await;
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
        }
        return Ok(());
    }

    sock.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        .await?;
    Ok(())
}
