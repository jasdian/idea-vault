//! The plan workbench over HTTP (docs/adr/0032, R46–R48): answering a build plan's open
//! questions and owner-held tasks makes a new version without a model call, never touches the
//! base, and refuses while a model job runs; the artifact page carries the forms on the head
//! only; re-planning is a background job whose plan joins the lineage.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers outside #[test] fns; HTC-6 binds shipping code"
)]

mod support;

use std::path::Path;

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use chrono::{DateTime, TimeZone, Utc};
use http_body_util::BodyExt;
use idea_vault::ai::sources::SourceProbe;
use idea_vault::app::{build_router, AppState};
use idea_vault::concepts::build_plan::finish::{finish, PlanInputs};
use idea_vault::concepts::build_plan::lineage;
use idea_vault::domain::{Idea, IdeaFrontmatter, IdeaState};
use idea_vault::vault::store;
use support::web::{get, post_form, test_state, test_state_with_ollama};
use support::{spawn, ChatScript};
use tower::ServiceExt;

const SLUG: &str = "trader";
const BASE: &str = "20260929-120000-build-plan";
const Q1_ANSWER: &str = "Freeze it at entry; dwell makes the backtest lie about fills.";
const T4_ANSWER: &str = "Trade on the paper account at the broker for the first month.";

const PLAN: &str = "## Goal
Ship the zone snapshot tool.

## Settled
- S1: Disproof comes before any code.
  quote: \"the cheapest disproof before any Rust exists\"

## Open questions
- Q1: Freeze the zone snapshot at entry, or dwell on the live label?
- Q2: Which exchange feeds the backtest data?

## Plan
- [ ] T1: Write the spec
  touches: `SPEC.md`
  accept: `test -s SPEC.md` → exit 0
- [ ] T2: Build the snapshot freezer
  depends: T1, Q1
  touches: `src/snap.rs`
  accept: `cargo test snap` → exit 0
- [ ] T3: Wire the freezer into the runner
  depends: T2
  touches: `src/run.rs`
  accept: `cargo test run` → exit 0
- [?] T4: Pick the broker account to trade on
  touches: `config.toml`
  accept: `test -s config.toml` → exit 0

## Kill criteria
- K1: The spec cannot name a dated kill → stop
  checked by: T1
  gates: T2";

fn at(minute: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 29, 12, minute, 0).unwrap()
}

fn write_idea(vault: &Path, state: IdeaState) {
    store::write_idea(
        vault,
        &Idea {
            frontmatter: IdeaFrontmatter {
                title: "Trader".into(),
                slug: SLUG.into(),
                state,
                tags: vec![],
                sources: vec![],
                created: at(0),
                updated: at(0),
            },
            body: "A zone snapshot trading tool.\n".into(),
        },
    )
    .unwrap();
}

/// An idea in discussion with one owner turn and a first plan, [`BASE`], made from [`PLAN`].
fn seed(vault: &Path) {
    write_idea(vault, IdeaState::InDiscussion);
    store::append_turn(
        vault,
        SLUG,
        "user",
        "We run the cheapest disproof before any Rust exists.",
    )
    .unwrap();
    let done = finish(PlanInputs {
        vault_dir: vault,
        idea_slug: SLUG,
        answer: PLAN,
        turn_role: "assistant (skill: build-prompt)",
        lens: "build-prompt",
        recipe: None,
        model: "llama3.2".into(),
        audit: None,
        probe: &SourceProbe::default(),
        now: at(0),
    })
    .unwrap();
    assert_eq!(done.artifact_slug, BASE);
}

/// A request whose response headers matter (`HX-Redirect`, `Location`).
async fn send(
    state: AppState,
    method: &str,
    uri: &str,
    form: &str,
) -> (StatusCode, HeaderMap, String) {
    let resp = build_router(state)
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(form.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, headers) = (resp.status(), resp.headers().clone());
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
}

fn answer_uri(stem: &str) -> String {
    format!("/idea/{SLUG}/plan/{stem}/answer")
}

fn form(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={}", v.replace(' ', "+").replace(';', "%3B")))
        .collect::<Vec<_>>()
        .join("&")
}

fn base_bytes(vault: &Path) -> Vec<u8> {
    std::fs::read(
        vault
            .join(SLUG)
            .join("artifacts")
            .join(format!("{BASE}.md")),
    )
    .unwrap()
}

/// Everything an answer could write: the transcript and the artifact files.
fn snapshot(vault: &Path) -> (String, usize) {
    (
        store::read_conversation(vault, SLUG).unwrap(),
        store::list_artifact_files(vault, SLUG).unwrap().len(),
    )
}

fn plan_stems(vault: &Path) -> Vec<String> {
    lineage::list_plans(vault, SLUG)
        .unwrap()
        .into_iter()
        .map(|p| p.stem)
        .collect()
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get(name)
        .unwrap_or_else(|| panic!("no {name} header"))
        .to_str()
        .unwrap()
}

#[tokio::test]
async fn answer_appends_user_turns_and_pointer_and_redirects_to_new_version() {
    let (state, vault) = test_state();
    seed(&vault);
    let base_before = base_bytes(&vault);
    let turns_before = store::read_conversation(&vault, SLUG).unwrap();

    let (status, headers, body) = send(
        state,
        "POST",
        &answer_uri(BASE),
        &form(&[("Q1", Q1_ANSWER), ("Q2", ""), ("T4", T4_ANSWER)]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let stems = plan_stems(&vault);
    assert_eq!(stems.len(), 2, "exactly one new version: {stems:?}");
    let new = stems.iter().find(|s| *s != BASE).unwrap();
    assert_eq!(
        header(&headers, "HX-Redirect"),
        format!("/idea/{SLUG}/artifact/{new}.md#work")
    );
    let artifact = store::read_artifact(&vault, SLUG, new).unwrap();
    assert_eq!(artifact.frontmatter.revises.as_deref(), Some(BASE));
    assert_eq!(artifact.frontmatter.version, Some(2));
    assert_eq!(artifact.frontmatter.answered, ["Q1", "T4"]);
    assert_eq!(
        base_bytes(&vault),
        base_before,
        "the base is never modified"
    );

    let added = store::read_conversation(&vault, SLUG).unwrap()[turns_before.len()..].to_string();
    assert_eq!(added.matches("## user\n").count(), 2, "{added}");
    assert!(
        added.contains(&format!("Re Q1 ({BASE}): {Q1_ANSWER}")),
        "{added}"
    );
    assert!(
        added.contains(&format!("Re T4 ({BASE}): {T4_ANSWER}")),
        "{added}"
    );
    assert_eq!(
        added
            .matches("## assistant (skill: build-prompt)\n**Build plan** → [")
            .count(),
        1,
        "{added}"
    );
    assert!(
        !added.contains("Freeze the zone snapshot at entry, or dwell"),
        "the question text is never copied into a turn: {added}"
    );
}

#[tokio::test]
async fn answer_while_job_running_is_409_and_writes_nothing() {
    let (state, vault) = test_state();
    seed(&vault);
    let before = snapshot(&vault);
    assert!(idea_vault::web::jobs::try_claim(&state.jobs, SLUG));

    let (status, body) = post_form(
        state.clone(),
        &answer_uri(BASE),
        &form(&[("Q1", Q1_ANSWER)]),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body.contains("the foil is thinking"), "{body}");
    assert_eq!(snapshot(&vault), before);
    assert!(
        idea_vault::web::jobs::is_running(&state.jobs, SLUG),
        "the refusal must not touch the running job's slot"
    );
}

#[tokio::test]
async fn answer_on_stored_idea_is_rejected() {
    let (state, vault) = test_state();
    seed(&vault);
    write_idea(&vault, IdeaState::Stored);
    let before = snapshot(&vault);

    let (status, body) = post_form(state, &answer_uri(BASE), &form(&[("Q1", Q1_ANSWER)])).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("reopen it"), "{body}");
    assert_eq!(snapshot(&vault), before);
}

#[tokio::test]
async fn answer_on_superseded_plan_is_409_naming_head() {
    let (state, vault) = test_state();
    seed(&vault);
    let (status, _) = post_form(
        state.clone(),
        &answer_uri(BASE),
        &form(&[("Q1", Q1_ANSWER)]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let head = plan_stems(&vault).into_iter().find(|s| s != BASE).unwrap();
    let before = snapshot(&vault);

    let (status, body) = post_form(
        state,
        &answer_uri(BASE),
        &form(&[("Q2", "Binance spot data, the free daily candles.")]),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains(&head), "the refusal names the head: {body}");
    assert_eq!(snapshot(&vault), before);
}

#[tokio::test]
async fn short_answer_is_422_and_writes_nothing() {
    let (state, vault) = test_state();
    seed(&vault);
    let before = snapshot(&vault);

    let (status, body) = post_form(
        state,
        &answer_uri(BASE),
        &form(&[
            ("Q1", "freeze"),
            ("Q2", "Binance spot data, daily candles."),
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body.contains(r#"<section class="planwork" id="work""#),
        "{body}"
    );
    // htmx 2 fires htmx:beforeSwap on the swap target (#work), not on the requesting form, so
    // the 422 opt-in must sit on the section's own tag or the re-render is never swapped in.
    let section_tag = &body[body.find(r#"<section class="planwork""#).unwrap()..];
    let section_tag = &section_tag[..section_tag.find('>').unwrap()];
    assert!(
        section_tag.contains("hx-on::before-swap") && section_tag.contains("422"),
        "the 422 opt-in is on the swap target: {section_tag}"
    );
    let form_tag = &body[body.find(r#"<form class="planwork__form""#).unwrap()..];
    let form_tag = &form_tag[..form_tag.find('>').unwrap()];
    assert!(
        !form_tag.contains("before-swap"),
        "a before-swap on the form never fires: {form_tag}"
    );
    assert!(body.contains("Q1: answer in at least"), "{body}");
    assert!(
        body.contains(">freeze</textarea>")
            && body.contains(">Binance spot data, daily candles.</textarea>"),
        "the owner's answers are kept: {body}"
    );
    assert_eq!(snapshot(&vault), before);
}

#[tokio::test]
async fn plan_page_renders_one_form_per_open_q_and_blocked_rows() {
    let (state, vault) = test_state();
    seed(&vault);

    let (status, body) = get(state, &format!("/idea/{SLUG}/artifact/{BASE}.md")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body.matches(&format!(r#"hx-post="/idea/{SLUG}/plan/{BASE}/answer""#))
            .count(),
        1,
        "{body}"
    );
    for id in ["Q1", "Q2"] {
        assert!(body.contains(&format!(r#"id="q-{id}""#)), "{id}: {body}");
        assert!(body.contains(&format!(r#"name="{id}""#)), "{id}: {body}");
    }
    assert_eq!(
        body.matches(r#"<details class="planwork__item""#).count(),
        2
    );
    // The owner-held rows: T2 waits on Q1 (linked to its question), T4 is the owner's to decide.
    for id in ["T2", "T4"] {
        assert!(body.contains(&format!(r#"id="t-{id}""#)), "{id}: {body}");
    }
    assert!(body.contains(r##"<a href="#q-Q1">Q1</a>"##), "{body}");
    assert!(body.contains(r#"name="T4""#), "T4 is answerable: {body}");
    assert!(
        !body.contains(r#"name="T2""#),
        "a Q-block is answered on its Q: {body}"
    );
    assert!(body.contains("Save answers → new version"));
    assert!(body.contains(&format!(r#"hx-post="/idea/{SLUG}/plan/{BASE}/replan""#)));
    // The workbench comes before the copy blocks.
    assert!(body.find(r#"id="work""#).unwrap() < body.find(r#"id="use-it""#).unwrap());
}

#[tokio::test]
async fn superseded_plan_page_has_banner_and_no_forms() {
    let (state, vault) = test_state();
    seed(&vault);
    let (status, _) = post_form(
        state.clone(),
        &answer_uri(BASE),
        &form(&[("Q1", Q1_ANSWER)]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let head = plan_stems(&vault).into_iter().find(|s| s != BASE).unwrap();

    let (status, body) = get(state.clone(), &format!("/idea/{SLUG}/artifact/{BASE}.md")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Superseded"), "{body}");
    assert!(
        body.contains(&format!("/artifact/{head}.md#work")),
        "{body}"
    );
    assert!(!body.contains("<textarea"), "{body}");
    assert!(!body.contains("/answer\""), "{body}");
    assert!(!body.contains("/replan\""), "{body}");

    // The head carries the forms and its lineage line.
    let (_, body) = get(state, &format!("/idea/{SLUG}/artifact/{head}.md")).await;
    assert!(body.contains(&format!("/plan/{head}/answer\"")), "{body}");
    assert!(body.contains("answered Q1"), "{body}");
    assert!(
        !body.contains(r#"id="q-Q1""#),
        "an answered question is gone: {body}"
    );
}

#[tokio::test]
async fn plan_latest_redirects_to_head() {
    // A reachable model, so the idea page renders its moves block (and the plan chip in it).
    let mock = spawn(&["llama3.2"], ChatScript::Tokens(vec![])).await;
    let (state, vault) = test_state_with_ollama(&mock.url, 1);
    write_idea(&vault, IdeaState::InDiscussion);
    let (status, _, body) = send(
        state.clone(),
        "GET",
        &format!("/idea/{SLUG}/plan/latest"),
        "",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("no build plan yet"), "{body}");

    seed(&vault);
    let (status, headers, _) = send(
        state.clone(),
        "GET",
        &format!("/idea/{SLUG}/plan/latest"),
        "",
    )
    .await;
    assert_eq!(status, StatusCode::FOUND);
    assert_eq!(
        header(&headers, "location"),
        format!("/idea/{SLUG}/artifact/{BASE}.md#work")
    );

    post_form(
        state.clone(),
        &answer_uri(BASE),
        &form(&[("Q1", Q1_ANSWER)]),
    )
    .await;
    let head = plan_stems(&vault).into_iter().find(|s| s != BASE).unwrap();
    let (_, headers, _) = send(
        state.clone(),
        "GET",
        &format!("/idea/{SLUG}/plan/latest"),
        "",
    )
    .await;
    assert_eq!(
        header(&headers, "location"),
        format!("/idea/{SLUG}/artifact/{head}.md#work")
    );

    // The idea page's chip points there, with the head's version and open count.
    let (_, page) = get(state, &format!("/idea/{SLUG}")).await;
    assert!(
        page.contains(&format!(r#"href="/idea/{SLUG}/plan/latest""#)),
        "{page}"
    );
    assert!(page.contains("Plan · v2 · 1 open"), "{page}");
}

#[tokio::test]
async fn replan_claims_job_and_redirects_to_idea() {
    let mock = spawn(&["llama3.2"], ChatScript::Tokens(vec![PLAN.into()])).await;
    let (state, vault) = test_state_with_ollama(&mock.url, 1);
    seed(&vault);
    let (status, _) = post_form(
        state.clone(),
        &answer_uri(BASE),
        &form(&[("Q1", Q1_ANSWER)]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let head = plan_stems(&vault).into_iter().find(|s| s != BASE).unwrap();

    let (status, headers, _) = send(
        state.clone(),
        "POST",
        &format!("/idea/{SLUG}/plan/{head}/replan"),
        "audited=0",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(header(&headers, "HX-Redirect"), format!("/idea/{SLUG}"));
    assert!(
        idea_vault::web::jobs::is_running(&state.jobs, SLUG) || plan_stems(&vault).len() == 3,
        "the re-plan runs as a background job"
    );

    for _ in 0..300 {
        if !idea_vault::web::jobs::is_running(&state.jobs, SLUG) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        mock.chat_bodies().len(),
        1,
        "a quick re-plan is one model call"
    );
    let stems = plan_stems(&vault);
    assert_eq!(stems.len(), 3, "{stems:?}");
    let newest = stems.iter().find(|s| **s != BASE && **s != head).unwrap();
    let replanned = store::read_artifact(&vault, SLUG, newest).unwrap();
    assert_eq!(
        replanned.frontmatter.revises.as_deref(),
        Some(head.as_str())
    );
    assert_eq!(replanned.frontmatter.version, Some(3));
    let view =
        idea_vault::concepts::build_plan::workbench::plan_view(&vault, SLUG, Some(newest)).unwrap();
    assert!(
        view.open
            .iter()
            .all(|q| !q.text.contains("Freeze the zone snapshot")),
        "the answered question is not asked again: {:?}",
        view.open
    );
    assert!(
        view.plan
            .settled
            .iter()
            .any(|s| s.field("answers") == Some("Q1")),
        "the owner's answer is carried in: {:?}",
        view.plan.settled
    );
}

#[tokio::test]
async fn workflow_route_behaviour_unchanged_after_seam_split() {
    let mock = spawn(
        &["llama3.2"],
        ChatScript::Tokens(vec!["workflow synthesis".into()]),
    )
    .await;
    let (state, vault) = test_state_with_ollama(&mock.url, 2);
    write_idea(&vault, IdeaState::InDiscussion);
    store::append_turn(&vault, SLUG, "user", "A first thought about the zones.").unwrap();

    // Unknown name: a synchronous 404, no job.
    let (status, _) = post_form(state.clone(), &format!("/idea/{SLUG}/workflow/nope"), "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(!idea_vault::web::jobs::is_running(&state.jobs, SLUG));

    // A lost claim re-renders the transcript and starts nothing.
    assert!(idea_vault::web::jobs::try_claim(&state.jobs, SLUG));
    let (status, _) = post_form(
        state.clone(),
        &format!("/idea/{SLUG}/workflow/interrogate"),
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(mock.chat_bodies().is_empty());
    idea_vault::web::jobs::mark_done(&state.jobs, SLUG);

    // The run itself: one persisted turn from the fixed stages.
    let (status, _) = post_form(
        state.clone(),
        &format!("/idea/{SLUG}/workflow/interrogate"),
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    support::web::poll_until(
        state.clone(),
        &format!("/idea/{SLUG}/pending"),
        "foil · workflow interrogate",
    )
    .await;
    assert_eq!(mock.chat_bodies().len(), 6);
    let convo = store::read_conversation(&vault, SLUG).unwrap();
    assert_eq!(
        convo
            .matches("## assistant (workflow: interrogate)")
            .count(),
        1
    );

    // A stored idea: the discussion guard answers first.
    write_idea(&vault, IdeaState::Stored);
    let (status, body) = post_form(state, &format!("/idea/{SLUG}/workflow/interrogate"), "").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("reopen it"), "{body}");
}
