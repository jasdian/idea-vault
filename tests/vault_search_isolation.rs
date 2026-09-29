//! `index::queries::vault_search` and `index::queries::turn_fact_hits` are offline experiment
//! instruments (ADR-0027, ADR-0031): context reaches the model by push, never by pull, and the
//! query-driven retriever was killed by its pre-registered experiment. Characterization: no
//! model-facing tool list and no module outside `index` may mention either.

mod support;

use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use idea_vault::ai::{sources, web};
use idea_vault::app::build_router;
use idea_vault::domain::Name;
use idea_vault::sources::ResolvedSource;
use serde_json::{json, Value};
use support::web::{test_state, with_mcp_token};
use tower::ServiceExt;

const TOKEN: &str = "isolation-token";
const NEEDLES: [&str; 2] = ["vault_search", "turn_fact_hits"];

async fn mcp_post(
    app: &axum::Router,
    body: Value,
    session: Option<&str>,
) -> (Option<String>, String) {
    let mut req = Request::builder()
        .method("POST")
        .uri("/api/mcp")
        .header("host", "localhost")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("authorization", format!("Bearer {TOKEN}"));
    if let Some(sid) = session {
        req = req.header("mcp-session-id", sid);
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let sid = resp
        .headers()
        .get("mcp-session-id")
        .map(|v| v.to_str().unwrap().to_string());
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (sid, String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test]
async fn vault_search_is_not_model_exposed() {
    let web_defs = web::tool_definitions().to_string();
    for needle in NEEDLES {
        assert!(!web_defs.contains(needle), "web tool defs: {web_defs}");
    }

    let tmp = tempfile::tempdir().unwrap();
    let src = ResolvedSource {
        name: Name::try_from("notes").unwrap(),
        root: tmp.path().to_path_buf(),
    };
    let source_defs = sources::tool_definitions(&[src]).to_string();
    for needle in NEEDLES {
        assert!(
            !source_defs.contains(needle),
            "source tool defs: {source_defs}"
        );
    }

    let (state, _vault) = test_state();
    let app = build_router(with_mcp_token(state, TOKEN));
    let init = json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": { "name": "isolation-test", "version": "0" }
        }
    });
    let (sid, _) = mcp_post(&app, init, None).await;
    let sid = sid.expect("session id");
    let notified = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
    mcp_post(&app, notified, Some(&sid)).await;

    let list = json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} });
    let (_, raw) = mcp_post(&app, list, Some(&sid)).await;
    assert!(
        raw.contains("list_ideas"),
        "tools/list did not return the catalog: {raw}"
    );
    let backend_src = include_str!("../src/ai/backend.rs");
    for needle in NEEDLES {
        assert!(!raw.contains(needle), "mcp tools/list: {raw}");
        assert!(
            !backend_src.contains(needle),
            "src/ai/backend.rs must not reference {needle}"
        );
    }
}

fn rust_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn vault_search_is_referenced_only_inside_the_index_module() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let index = src.join("index");
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    assert!(files.iter().any(|f| f.starts_with(&index)));
    let offenders: Vec<_> = files
        .iter()
        .filter(|f| !f.starts_with(&index))
        .filter(|f| {
            let text = std::fs::read_to_string(f).unwrap();
            NEEDLES.iter().any(|n| text.contains(n))
        })
        .collect();
    assert!(offenders.is_empty(), "{offenders:?}");
}
