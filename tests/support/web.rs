//! Shared web-handler test harness: a real `AppState` over temp dirs, a router, and
//! request/response helpers. Ollama defaults to a refused loopback URL; tests that need a live
//! mock override `ollama_url` via [`test_state_with_ollama`].

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use idea_vault::ai::OllamaClient;
use idea_vault::app::{build_router, AppState};
use idea_vault::config::Config;
use idea_vault::index;
use tokio::sync::Semaphore;
use tower::ServiceExt;

/// A test AppState over fresh temp dirs. Returns the state and its vault directory so tests can
/// assert on-disk truth directly. The tempdir is leaked for the process lifetime.
pub fn test_state_with_ollama(ollama_url: &str, ai_concurrency: usize) -> (AppState, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let vault_dir = tmp.path().join("vault");
    let index_path = tmp.path().join("index.db");
    std::fs::create_dir_all(&vault_dir).expect("vault dir");

    let config = Config {
        bind: "127.0.0.1:0".to_string(),
        vault_dir: vault_dir.clone(),
        index_path: index_path.clone(),
        ollama_url: ollama_url.to_string(),
        ollama_model: "llama3.2".to_string(),
        ai_concurrency,
        ollama_timeout: std::time::Duration::from_secs(5),
        ollama_temperature: 0.7,
        llm_backend: idea_vault::config::LlmBackendKind::Ollama,
        claude: default_claude_settings(&vault_dir),
        auto_compact: true,
        compact_threshold: 0.80,
        ollama_ctx_tokens: 0,
        claude_ctx_tokens: 0,
    };

    let conn = index::schema::open_or_create(&index_path).expect("open index");
    let ollama = OllamaClient::new(config.ollama_url.clone(), config.ollama_model.clone())
        .expect("build ollama client");

    std::mem::forget(tmp);

    (
        AppState {
            config: Arc::new(config),
            db: Arc::new(Mutex::new(conn)),
            llm: idea_vault::ai::LlmBackend::ollama_only(ollama),
            ai_semaphore: Arc::new(Semaphore::new(ai_concurrency)),
            skills: Arc::new(idea_vault::concepts::skills::SkillRegistry::builtin()),
            jobs: idea_vault::web::jobs::new_registry(),
        },
        vault_dir,
    )
}

/// A benign claude-code settings block for test `Config`s (unused by Ollama-backed tests).
pub fn default_claude_settings(vault_dir: &std::path::Path) -> idea_vault::config::ClaudeSettings {
    idea_vault::config::ClaudeSettings {
        binary: "claude".to_string(),
        cwd: vault_dir.to_path_buf(),
        add_dirs: Vec::new(),
        allowed_tools: Vec::new(),
        model: None,
        skip_permissions: true,
        timeout: std::time::Duration::from_secs(5),
        effort: "high".to_string(),
    }
}

/// Default harness: Ollama refused fast (port 9), concurrency 1.
pub fn test_state() -> (AppState, PathBuf) {
    test_state_with_ollama("http://127.0.0.1:9", 1)
}

/// POST a `application/x-www-form-urlencoded` body to `uri` on a fresh router over `state`.
pub async fn post_form(state: AppState, uri: &str, form_body: &str) -> (StatusCode, String) {
    let app = build_router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(form_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.expect("body").to_bytes();
    (status, String::from_utf8(bytes.to_vec()).expect("utf8"))
}

/// GET `uri` on a fresh router over `state`.
pub async fn get(state: AppState, uri: &str) -> (StatusCode, String) {
    let app = build_router(state);
    let resp = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.expect("body").to_bytes();
    (status, String::from_utf8(bytes.to_vec()).expect("utf8"))
}

/// Poll a GET route until its body contains `needle`, then return that body. For the async
/// background-job model: a POST kicks off the work and the transcript fills in via `/pending`.
/// Panics with the last body if the needle never appears.
pub async fn poll_until(state: AppState, uri: &str, needle: &str) -> String {
    for _ in 0..300 {
        let (_, body) = get(state.clone(), uri).await;
        if body.contains(needle) {
            return body;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let (_, body) = get(state, uri).await;
    panic!("'{needle}' not found after polling {uri}; last body:\n{body}");
}
