//! Boot / HTTP surface smoke test (docs/09-web-ui.md D17). Exercises the router end-to-end with a
//! refusing Ollama URL — no network beyond a loopback connection that is refused fast.

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

fn test_state() -> AppState {
    let tmp = tempfile::tempdir().expect("tempdir");
    let vault_dir = tmp.path().join("vault");
    let index_path = tmp.path().join("index.db");
    std::fs::create_dir_all(&vault_dir).expect("vault dir");

    let config = Config {
        bind: "127.0.0.1:0".to_string(),
        vault_dir,
        index_path: index_path.clone(),
        // Port 9 (discard) refuses fast — the probe resolves to Unreachable without hanging.
        ollama_url: "http://127.0.0.1:9".to_string(),
        ollama_model: "llama3.2".to_string(),
        ai_concurrency: 1,
    };

    let conn = index::schema::open_or_create(&index_path).expect("open index");
    let ollama = OllamaClient::new(config.ollama_url.clone(), config.ollama_model.clone())
        .expect("build ollama client");

    // Keep the tempdir alive for the process lifetime.
    std::mem::forget(tmp);

    AppState {
        config: Arc::new(config),
        db: Arc::new(Mutex::new(conn)),
        ollama,
        ai_semaphore: Arc::new(Semaphore::new(1)),
    }
}

async fn body_string(resp: axum::response::Response) -> String {
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("collect")
        .to_bytes();
    String::from_utf8(bytes.to_vec()).expect("utf8")
}

#[tokio::test]
async fn root_lists_empty_state() {
    let app = build_router(test_state());
    let resp = app
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(body_string(resp).await.contains("No ideas yet"));
}

#[tokio::test]
async fn health_reports_unreachable() {
    let app = build_router(test_state());
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/admin/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(body_string(resp).await.contains("unreachable"));
}

#[tokio::test]
async fn static_htmx_is_embedded() {
    let app = build_router(test_state());
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/static/htmx.min.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn create_idea_is_scaffold_501() {
    let app = build_router(test_state());
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/ideas")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
}
