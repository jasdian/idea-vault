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
    };

    let conn = index::schema::open_or_create(&index_path).expect("open index");
    let ollama = OllamaClient::new(config.ollama_url.clone(), config.ollama_model.clone())
        .expect("build ollama client");

    std::mem::forget(tmp);

    (
        AppState {
            config: Arc::new(config),
            db: Arc::new(Mutex::new(conn)),
            ollama,
            ai_semaphore: Arc::new(Semaphore::new(ai_concurrency)),
            skills: Arc::new(idea_vault::concepts::skills::SkillRegistry::builtin()),
        },
        vault_dir,
    )
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
