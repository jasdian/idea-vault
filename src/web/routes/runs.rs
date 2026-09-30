//! The run inspector (R50, `GET /idea/{slug}/runs/{run_id}`, ADR-0037): a read-only page over
//! one run journal. Per call it shows the role, backend and model, how the answer met its output
//! contract, the tokens, the stop reason and the truncation flags, with the verbatim response
//! folded away. The journal is diagnostics, not truth: nothing here writes or indexes it.
//!
//! Lines are read as plain JSON rather than the writer's types, so a journal from an older or
//! newer build still renders what it can; a line that is not an object (a torn tail) is counted,
//! never fatal.

use axum::extract::{Path, State};
use serde_json::Value;

use crate::vault::store;
use crate::web::state::AppState;
use crate::web::templates::{RunCallView, RunPage};
use crate::web::WebError;

/// R50 — `GET /idea/{slug}/runs/{run_id}` — render one run journal.
pub async fn run_page(
    State(state): State<AppState>,
    Path((slug, run_id)): Path<(String, String)>,
) -> Result<RunPage, WebError> {
    let vault_dir = &state.config.vault_dir;
    let idea = store::read_idea(vault_dir, &slug)?; // 404 if missing
    let raw = store::read_run_journal(vault_dir, &slug, &run_id)?;
    Ok(run_view(
        &raw,
        idea.frontmatter.slug,
        idea.frontmatter.title,
        run_id,
    ))
}

fn text(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

fn num(v: &Value, key: &str) -> Option<u64> {
    v.get(key).and_then(Value::as_u64)
}

/// `clean`, `repaired`, `retried` or `off-contract: <violation>` from a serialized
/// `ContractOutcome` (`{"status": …, "violation": …}`).
fn contract_label(outcome: &Value) -> String {
    let status = text(outcome, "status").unwrap_or_else(|| "unknown".to_string());
    match (status.as_str(), text(outcome, "violation")) {
        ("off_contract", Some(why)) => format!("off-contract: {why}"),
        ("off_contract", None) => "off-contract".to_string(),
        _ => status,
    }
}

/// How a `RunFinished` line says the run ended.
fn outcome_label(line: &Value) -> String {
    match (text(line, "outcome"), text(line, "message")) {
        (Some(o), Some(m)) => format!("{o}: {m}"),
        (Some(o), None) => o,
        (None, _) => "unknown".to_string(),
    }
}

/// The page for one journal's raw text. Pure over the text, so the parse is unit-tested.
pub(crate) fn run_view(raw: &str, slug: String, idea_title: String, run_id: String) -> RunPage {
    let mut page = RunPage {
        slug,
        idea_title,
        run_id,
        kind: String::new(),
        build: String::new(),
        outcome: "unfinished — no end recorded".to_string(),
        calls: Vec::new(),
        unreadable: 0,
    };
    for line in raw.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            page.unreadable += 1;
            continue;
        };
        match text(&v, "type").as_deref() {
            Some("run_started") => {
                page.kind = text(&v, "kind").unwrap_or_default();
                page.build = text(&v, "build").unwrap_or_default();
            }
            Some("llm_call") => page.calls.push(call_view(&v)),
            Some("tool_call") => {
                let seq = num(&v, "call_seq");
                if let Some(call) = page.calls.iter_mut().rev().find(|c| Some(c.seq) == seq) {
                    let err = if v.get("is_error").and_then(Value::as_bool) == Some(true) {
                        " (error)"
                    } else {
                        ""
                    };
                    call.tools.push(format!(
                        "round {} · {}{err}",
                        num(&v, "round").unwrap_or(0),
                        text(&v, "name").unwrap_or_default()
                    ));
                }
            }
            Some("contract") => {
                let seq = num(&v, "call_seq");
                if let Some(call) = page.calls.iter_mut().rev().find(|c| Some(c.seq) == seq) {
                    let label = v
                        .get("outcome")
                        .map_or_else(|| "unknown".to_string(), contract_label);
                    call.off_contract = label.starts_with("off-contract");
                    call.contract = match text(&v, "contract") {
                        Some(name) => format!("{name} · {label}"),
                        None => label,
                    };
                }
            }
            Some("run_finished") => page.outcome = outcome_label(&v),
            _ => {}
        }
    }
    page
}

fn call_view(v: &Value) -> RunCallView {
    let meta = v.get("meta").cloned().unwrap_or(Value::Null);
    let usage = meta.get("usage").cloned().unwrap_or(Value::Null);
    let tokens = |key: &str| num(&usage, key).map_or_else(|| "?".to_string(), |n| n.to_string());
    let stop_reason = text(&meta, "stop_reason");
    let prompt = num(&usage, "prompt_tokens");
    let num_ctx = num(&meta, "num_ctx");
    // The same rules as `CallMeta` (ADR-0037): a `length` stop cut the answer; a prompt within
    // 2% of the window the call sent was probably cut on the way in. Unknown is not truncated.
    let mut truncation = Vec::new();
    if stop_reason.as_deref() == Some("length") {
        truncation.push("output truncated");
    }
    if let (Some(p), Some(ctx)) = (prompt, num_ctx) {
        if ctx > 0 && p >= ctx * 98 / 100 {
            truncation.push("input truncated");
        }
    }
    RunCallView {
        seq: num(v, "seq").unwrap_or(0),
        role: text(v, "role").unwrap_or_else(|| "—".to_string()),
        backend: text(v, "backend").unwrap_or_default(),
        model: text(v, "model").unwrap_or_default(),
        contract: "not checked".to_string(),
        off_contract: false,
        prompt_tokens: tokens("prompt_tokens"),
        output_tokens: tokens("output_tokens"),
        api_calls: num(&usage, "api_calls").unwrap_or(0),
        stop_reason: stop_reason.unwrap_or_else(|| "unknown".to_string()),
        truncation: truncation.join(" · "),
        ms: num(&meta, "ms").unwrap_or(0),
        response: text(v, "response_text").unwrap_or_default(),
        tools: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const JOURNAL: &str = r#"{"type":"run_started","format_version":1,"run_id":"r1","slug":"s","kind":"skill","build":"0.1.0","ts_ms":1}
{"type":"llm_call","seq":0,"role":"critic","backend":"ollama","model":"qwen","temperature_milli":900,"request_sha256":"ab","response_text":"1. a","meta":{"usage":{"prompt_tokens":990,"output_tokens":20,"api_calls":2},"stop_reason":"length","num_ctx":1000,"ms":42}}
{"type":"tool_call","call_seq":0,"round":1,"name":"web_search","args_sha256":"cd","result_text":"x","is_error":false}
{"type":"contract","call_seq":0,"contract":"ranked_list","outcome":{"status":"off_contract","violation":"no numbered list"}}
{"type":"run_finished","outcome":"failed","message":"boom","llm_calls":1,"ts_ms":2}
{"type":"llm_c"#;

    #[test]
    fn a_journal_renders_calls_contract_tools_and_a_torn_tail() {
        let page = run_view(JOURNAL, "s".into(), "S".into(), "r1".into());
        assert_eq!(
            (page.kind.as_str(), page.build.as_str()),
            ("skill", "0.1.0")
        );
        assert_eq!(page.outcome, "failed: boom");
        assert_eq!(
            page.unreadable, 1,
            "the torn last line is counted, not fatal"
        );
        let call = &page.calls[0];
        assert_eq!(call.role, "critic");
        assert_eq!(
            call.contract,
            "ranked_list · off-contract: no numbered list"
        );
        assert!(call.off_contract);
        assert_eq!((call.prompt_tokens.as_str(), call.api_calls), ("990", 2));
        assert_eq!(call.truncation, "output truncated · input truncated");
        assert_eq!(call.tools, ["round 1 · web_search"]);
    }

    #[test]
    fn unknown_meta_is_never_truncated() {
        let raw = r#"{"type":"llm_call","seq":3,"backend":"claude-code","model":"m","response_text":"hi","meta":{"usage":{"api_calls":1},"ms":5}}"#;
        let page = run_view(raw, "s".into(), "S".into(), "r".into());
        let call = &page.calls[0];
        assert_eq!(call.truncation, "");
        assert_eq!(call.stop_reason, "unknown");
        assert_eq!(call.prompt_tokens, "?");
        assert_eq!(page.outcome, "unfinished — no end recorded");
    }
}
