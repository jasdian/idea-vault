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
        vault_dir: vault_dir.clone(),
        index_path: index_path.clone(),
        // Port 9 (discard) refuses fast — the probe resolves to Unreachable without hanging.
        ollama_url: "http://127.0.0.1:9".to_string(),
        ollama_model: "llama3.2".to_string(),
        ai_concurrency: 1,
        ollama_timeout: std::time::Duration::from_secs(5),
        ollama_temperature: 0.7,
        llm_backend: idea_vault::config::LlmBackendKind::Ollama,
        claude: idea_vault::config::ClaudeSettings {
            binary: "claude".to_string(),
            cwd: std::path::PathBuf::from("."),
            add_dirs: Vec::new(),
            allowed_tools: Vec::new(),
            model: None,
            skip_permissions: true,
            timeout: std::time::Duration::from_secs(5),
            effort: "high".to_string(),
        },
        auto_compact: true,
        compact_threshold: 0.80,
        ollama_ctx_tokens: 0,
        claude_ctx_tokens: 0,
        web_access: false,
        audit_findings: true,
        mcp_config_path: tmp.path().join(".mcp-servers.json"),
        // Beside the vault (like the MCP registry), NOT inside it — the registry writes its
        // override + .gitignore next to its config file.
        sources_config_path: tmp.path().join(".sources.json"),
        sources_dir: None,
        sources_applied: None,
        skills_dir: vault_dir.join(".skills"),
        mcp_server_token: None,
    };

    let conn = index::schema::open_or_create(&index_path).expect("open index");
    let ollama = OllamaClient::new(config.ollama_url.clone(), config.ollama_model.clone())
        .expect("build ollama client");
    let mcp = Arc::new(idea_vault::mcp::McpRegistry::load(&config.mcp_config_path));
    let sources = Arc::new(idea_vault::sources::SourceRegistry::load(
        config.sources_config_path.clone(),
        tmp.path().join(idea_vault::sources::OVERRIDE_FILENAME),
        None,
        None,
    ));

    let skills = Arc::new(idea_vault::concepts::skills::LiveSkills::load(
        config.skills_dir.clone(),
    ));

    // Keep the tempdir alive for the process lifetime.
    std::mem::forget(tmp);

    AppState {
        config: Arc::new(config),
        db: Arc::new(Mutex::new(conn)),
        llm: idea_vault::ai::LlmBackend::ollama_only(ollama),
        ai_semaphore: Arc::new(Semaphore::new(1)),
        skills,
        jobs: idea_vault::web::jobs::new_registry(),
        queues: idea_vault::web::jobs::new_queues(),
        mcp,
        sources,
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
    assert!(body_string(resp).await.contains("Nothing here yet"));
}

/// D20 preserved: an absent model is a valid state. The Docker HEALTHCHECK must still pass on a
/// model-less stack, so an unreachable LLM reports in the body but never fails the probe.
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
    let body = body_string(resp).await;
    assert!(body.contains("unreachable"));
    assert!(body.contains("\"vault\":\"ok\""), "usable vault: {body}");
    // The advisory sources object is always present, even with nothing registered.
    assert!(body.contains("\"sources\""), "sources field: {body}");
    assert!(body.contains("\"total\":0"), "empty registry: {body}");
}

/// Sources are advisory the same way the LLM is: a missing or not-yet-applied source degrades the
/// turns that attach it, never the app — so the counts report honestly but the status code never
/// moves off 200.
#[tokio::test]
async fn health_counts_sources_without_moving_the_status_code() {
    let state = test_state();
    // Bare mode probes host paths directly: one listable source, one long gone.
    let real = tempfile::tempdir().expect("tempdir");
    state
        .sources
        .add(idea_vault::sources::SourceConfig {
            name: idea_vault::domain::Name::try_from("real").unwrap(),
            host_path: real.path().to_path_buf(),
        })
        .unwrap();
    state
        .sources
        .add(idea_vault::sources::SourceConfig {
            name: idea_vault::domain::Name::try_from("gone").unwrap(),
            host_path: std::path::PathBuf::from("/nonexistent/idea-vault-health-test"),
        })
        .unwrap();

    let resp = build_router(state)
        .oneshot(
            Request::builder()
                .uri("/admin/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a broken source must never fail the probe"
    );
    let body = body_string(resp).await;
    assert!(body.contains("\"total\":2"), "body: {body}");
    assert!(body.contains("\"mounted\":1"), "body: {body}");
    assert!(body.contains("\"missing\":1"), "body: {body}");
    assert!(body.contains("\"needs_reup\":0"), "body: {body}");
}

/// ADR-0019, the direct regression test for "the Docker healthcheck stayed green for two days
/// while the vault was gone". An unusable vault is not a degraded state — it is a broken one, and
/// `curl -fsS` in the HEALTHCHECK must see it.
#[tokio::test]
async fn health_is_503_when_the_vault_is_unusable() {
    let state = test_state();
    // The vault vanishes underneath the running process — precisely the incident's shape.
    std::fs::remove_dir_all(&state.config.vault_dir).unwrap();

    let resp = build_router(state)
        .oneshot(
            Request::builder()
                .uri("/admin/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = body_string(resp).await;
    assert!(body.contains("\"vault\":\"unreadable\""), "body: {body}");
    assert!(
        body.contains("\"status\":\"vault-unusable\""),
        "body: {body}"
    );
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
async fn last_stub_route_is_real_and_create_validates_input() {
    // The former last stub (/admin/reindex) now runs a real rebuild and returns counts on an
    // empty vault. (A code-level grep confirms no NotImplemented is constructed anywhere —
    // this test only spot-checks the route that held out longest.)
    let app = build_router(test_state());
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/reindex")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(body_string(resp).await.contains("\"ideas\":0"));

    // Input validation stands in for the old honest-501 canary (empty form → 400).
    let app = build_router(test_state());
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/ideas")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from("title="))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
