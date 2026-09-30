//! Web handler tests for R6 skill / R7 swarm (D18/D14): 200 `_turn.html` partials appended,
//! guards, and the persist rules (skill output appended by invoke; swarm persists only the
//! synthesis). Mock Ollama only.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers outside #[test] fns; HTC-6 binds shipping code"
)]

mod support;

use axum::http::StatusCode;
use chrono::{TimeZone, Utc};
use idea_vault::domain::{Idea, IdeaFrontmatter, IdeaState};
use idea_vault::vault::store;
use support::web::{post_form, test_state_with_ollama};
use support::{spawn, ChatScript};

fn seed(vault: &std::path::Path, state: IdeaState) {
    store::write_idea(
        vault,
        &Idea {
            frontmatter: IdeaFrontmatter {
                title: "Movable".into(),
                slug: "movable".into(),
                state,
                tags: vec![],
                sources: vec![],
                created: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
                updated: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
                extra: Default::default(),
            },
            body: "The idea body.\n".into(),
        },
    )
    .unwrap();
    store::append_turn(vault, "movable", "user", "attack it").unwrap();
}

#[tokio::test]
async fn run_skill_returns_turn_partial_and_appends_it() {
    let mock = spawn(
        &["llama3.2"],
        ChatScript::Tokens(vec!["Ranked failure causes.".into()]),
    )
    .await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    seed(&vault_dir, IdeaState::InDiscussion);

    let (status, _) = post_form(state.clone(), "/idea/movable/skill/premortem", "").await;
    assert_eq!(status, StatusCode::OK);
    // The skill runs as a background job; its labelled turn arrives via /pending.
    let body = support::web::poll_until(state, "/idea/movable/pending", "foil · premortem").await;
    assert!(body.contains("Ranked failure causes."));

    // Persisted as a labelled assistant turn; the skill template reached the model.
    let convo = store::read_conversation(&vault_dir, "movable").unwrap();
    assert!(convo.contains("## assistant (skill: premortem)\nRanked failure causes."));
    assert!(mock.chat_bodies()[0].contains("failed badly 12 months"));
}

#[tokio::test]
async fn structured_dissent_skills_render_as_described_chips_and_run() {
    let mock = spawn(
        &["llama3.2"],
        ChatScript::Tokens(vec!["Core contradiction named.".into()]),
    )
    .await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    seed(&vault_dir, IdeaState::InDiscussion);

    // Every new move is a chip whose tooltip carries its description, and an opt-in (unchecked)
    // swarm angle whose label carries the same tooltip.
    let (status, page) = support::web::get(state.clone(), "/idea/movable").await;
    assert_eq!(status, StatusCode::OK);
    for (name, blurb) in [
        ("pr-faq", "Work backwards from launch"),
        ("dialectical-inquiry", "Build the strongest rival plan"),
        (
            "triz",
            "core contradiction and resolve it without compromise",
        ),
    ] {
        assert!(
            page.contains(&format!("hx-post=\"/idea/movable/skill/{name}\"")),
            "no chip for {name}"
        );
        assert!(
            page.matches(blurb).count() >= 2,
            "{name}'s description should title both its chip and its swarm angle"
        );
        assert!(
            page.contains(&format!("value=\"{name}\"> {name}")),
            "{name} should be an unchecked swarm angle"
        );
    }

    let (status, _) = post_form(state.clone(), "/idea/movable/skill/triz", "").await;
    assert_eq!(status, StatusCode::OK);
    let body = support::web::poll_until(state, "/idea/movable/pending", "foil · triz").await;
    assert!(body.contains("Core contradiction named."));
    let convo = store::read_conversation(&vault_dir, "movable").unwrap();
    assert!(convo.contains("## assistant (skill: triz)\nCore contradiction named."));
    assert!(mock.chat_bodies()[0].contains("TRIZ contradiction analysis"));
}

#[tokio::test]
async fn run_skill_guards_unknown_stored_and_missing() {
    let mock = spawn(&["llama3.2"], ChatScript::Tokens(vec!["x".into()])).await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    seed(&vault_dir, IdeaState::InDiscussion);

    let (status, _) = post_form(state.clone(), "/idea/movable/skill/not-a-skill", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = post_form(state.clone(), "/idea/ghost/skill/premortem", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(mock.chat_bodies().is_empty(), "no AI call for rejects");

    let mock2 = spawn(&["llama3.2"], ChatScript::Tokens(vec!["x".into()])).await;
    let (state2, vault_dir2) = test_state_with_ollama(&mock2.url, 1);
    seed(&vault_dir2, IdeaState::Stored);
    let (status, _) = post_form(state2, "/idea/movable/skill/premortem", "").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn run_swarm_defaults_to_the_canonical_angles_and_persists_only_synthesis() {
    // Repeat script: every fan-out agent and the synthesizer answer the same way.
    let mock = spawn(
        &["llama3.2"],
        ChatScript::Tokens(vec!["converged finding".into()]),
    )
    .await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 2);
    seed(&vault_dir, IdeaState::InDiscussion);

    let (status, _) = post_form(state.clone(), "/idea/movable/swarm", "").await;
    assert_eq!(status, StatusCode::OK);
    let body = support::web::poll_until(state, "/idea/movable/pending", "foil · swarm").await;
    assert!(body.contains("converged finding"));

    // Canonical D14 set: 4 angles + 1 auditor (on by default) + its one re-ask ("converged
    // finding" is no verdict, ADR-0023 amendment) + 1 synthesizer = 7 model calls.
    assert_eq!(mock.chat_bodies().len(), 7);
    // Only the synthesis persisted, exactly one swarm turn, headed by its angles.
    let convo = store::read_conversation(&vault_dir, "movable").unwrap();
    assert_eq!(
        convo
            .matches(
                "## assistant (swarm: premortem, cheapest-disproof, constraints, second-order-effects)"
            )
            .count(),
        1
    );
}

#[tokio::test]
async fn run_workflow_interrogate_persists_only_synthesis_and_guards() {
    // Repeat script: the 4 fixed D19 steps and the synthesizer answer the same way.
    let mock = spawn(
        &["llama3.2"],
        ChatScript::Tokens(vec!["workflow synthesis".into()]),
    )
    .await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 2);
    seed(&vault_dir, IdeaState::InDiscussion);

    let (status, _) = post_form(state.clone(), "/idea/movable/workflow/interrogate", "").await;
    assert_eq!(status, StatusCode::OK);
    let body = support::web::poll_until(
        state.clone(),
        "/idea/movable/pending",
        "foil · workflow interrogate",
    )
    .await;
    assert!(body.contains("workflow synthesis"));

    // Fixed stages: 4 fan-out steps + 1 auditor + 1 synthesizer = 6 calls, one turn persisted.
    assert_eq!(mock.chat_bodies().len(), 6);
    let convo = store::read_conversation(&vault_dir, "movable").unwrap();
    assert_eq!(
        convo
            .matches("## assistant (workflow: interrogate)")
            .count(),
        1
    );

    // Unknown workflow → synchronous 404, no job started.
    let (status, _) = post_form(state, "/idea/movable/workflow/nope", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn r22_404s_workflow_present_only_as_invalid_file() {
    let mock = spawn(&["llama3.2"], ChatScript::Tokens(vec!["x".into()])).await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    seed(&vault_dir, IdeaState::InDiscussion);
    let dir = state.workflows.dir().to_path_buf();
    std::fs::create_dir_all(&dir).unwrap();
    // A well-formed file naming a skill that does not exist: it loads as an issue, not a workflow.
    std::fs::write(
        dir.join("broken.md"),
        "---\nname: broken\ndescription: d\nstages:\n  - kind: fan_out\n    steps:\n      - {role: critic, skill: no-such-skill}\n  - kind: synthesize\n---\n",
    )
    .unwrap();
    state.workflows.reload(&state.skills);
    assert!(state
        .workflows
        .issues()
        .iter()
        .any(|i| i.file == "broken.md"));
    let before = store::read_conversation(&vault_dir, "movable").unwrap();

    let (status, _) = post_form(state.clone(), "/idea/movable/workflow/broken", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(mock.chat_bodies().is_empty(), "no job started");
    assert_eq!(
        store::read_conversation(&vault_dir, "movable").unwrap(),
        before
    );
}

#[tokio::test]
async fn run_swarm_custom_angles_and_unknown_angle_400() {
    let mock = spawn(&["llama3.2"], ChatScript::Tokens(vec!["out".into()])).await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    seed(&vault_dir, IdeaState::InDiscussion);

    let (status, _) = post_form(state.clone(), "/idea/movable/swarm", "angles=premortem").await;
    assert_eq!(status, StatusCode::OK);
    support::web::poll_until(state.clone(), "/idea/movable/pending", "foil · swarm").await;
    assert_eq!(
        mock.chat_bodies().len(),
        4,
        "1 angle + 1 auditor + its re-ask (\"out\" is no verdict) + 1 synthesizer"
    );

    // Unknown angle is rejected synchronously (validated in the handler before any job starts).
    let (status, _) = post_form(state, "/idea/movable/swarm", "angles=nope").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn draft_ideas_refuse_moves_with_400_and_stay_untouched() {
    // D9 has no Draft skill/swarm edge — a Draft must never gain assistant turns while its
    // frontmatter still says draft (state is canonical).
    let mock = spawn(&["llama3.2"], ChatScript::Tokens(vec!["x".into()])).await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    store::write_idea(
        &vault_dir,
        &Idea {
            frontmatter: IdeaFrontmatter {
                title: "Drafty".into(),
                slug: "drafty".into(),
                state: IdeaState::Draft,
                tags: vec![],
                sources: vec![],
                created: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
                updated: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
                extra: Default::default(),
            },
            body: "seed\n".into(),
        },
    )
    .unwrap();

    let (status, _) = post_form(state.clone(), "/idea/drafty/skill/premortem", "").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = post_form(state, "/idea/drafty/swarm", "").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(mock.chat_bodies().is_empty(), "no AI calls for a draft");
    assert_eq!(store::read_conversation(&vault_dir, "drafty").unwrap(), "");
}

#[tokio::test]
async fn oversized_angle_list_is_400_with_no_ai_calls() {
    let mock = spawn(&["llama3.2"], ChatScript::Tokens(vec!["x".into()])).await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    seed(&vault_dir, IdeaState::InDiscussion);

    let nine = std::iter::repeat_n("premortem", 9)
        .collect::<Vec<_>>()
        .join(",");
    let (status, _) = post_form(state, "/idea/movable/swarm", &format!("angles={nine}")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(mock.chat_bodies().is_empty());
}

#[tokio::test]
async fn swarm_picker_caps_selection_at_max_angles() {
    use idea_vault::concepts::swarm::MAX_ANGLES;
    let mock = spawn(&["llama3.2"], ChatScript::Tokens(vec!["x".into()])).await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    seed(&vault_dir, IdeaState::InDiscussion);

    let (status, page) = support::web::get(state, "/idea/movable").await;
    assert_eq!(status, StatusCode::OK);

    let offered = page.matches("class=\"swarm-angle\"").count();
    assert!(
        offered > MAX_ANGLES,
        "the setup must offer more angles than the cap ({offered} offered, cap {MAX_ANGLES})"
    );
    assert!(
        page.contains(&format!("data-max-angles=\"{MAX_ANGLES}\"")),
        "the picker must carry the server's cap"
    );
    let menu_start = page
        .find("data-max-angles=")
        .expect("cap attribute present");
    // The handler's own `>=` makes `>` useless as a tag end; the menu's first child opens with `<`.
    let menu_tag = &page[menu_start..menu_start + page[menu_start..].find('<').unwrap()];
    assert!(
        menu_tag.contains("hx-on:change=")
            && menu_tag.contains("dataset.maxAngles")
            && menu_tag.contains(".disabled"),
        "the element carrying the cap must also carry a change handler that reads it and disables boxes"
    );
    assert!(mock.chat_bodies().is_empty());
}

#[tokio::test]
async fn run_swarm_all_agents_failed_surfaces_error_and_persists_nothing() {
    let mock = spawn(&["llama3.2"], ChatScript::EofAfter(vec![])).await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    seed(&vault_dir, IdeaState::InDiscussion);
    let convo_before = store::read_conversation(&vault_dir, "movable").unwrap();

    let (status, _) = post_form(state.clone(), "/idea/movable/swarm", "").await;
    assert_eq!(status, StatusCode::OK);
    // Every agent fails → the job errors and the failure surfaces via /pending; nothing persisted.
    support::web::poll_until(state, "/idea/movable/pending", "could not respond").await;
    assert_eq!(
        store::read_conversation(&vault_dir, "movable").unwrap(),
        convo_before
    );
}

#[tokio::test]
async fn turning_the_audit_off_in_settings_drops_the_auditor_call() {
    let mock = spawn(&["llama3.2"], ChatScript::Tokens(vec!["out".into()])).await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    seed(&vault_dir, IdeaState::InDiscussion);

    // The settings form posts every field; an unticked checkbox is simply absent.
    let (status, form) = post_form(state.clone(), "/settings", "backend=ollama").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!state.llm.settings().audit_findings);
    assert!(form.contains("name=\"audit_findings\""));

    let (status, _) = post_form(state.clone(), "/idea/movable/swarm", "angles=premortem").await;
    assert_eq!(status, StatusCode::OK);
    support::web::poll_until(state, "/idea/movable/pending", "foil · swarm").await;
    assert_eq!(
        mock.chat_bodies().len(),
        2,
        "1 angle + 1 synthesizer, no auditor"
    );
    assert!(!mock
        .chat_bodies()
        .iter()
        .any(|b| b.contains("You are the Auditor")));
}

fn seed_linked_pair(vault: &std::path::Path) {
    for (slug, title, body) in [
        (
            "orchard-sensor",
            "Orchard sensor",
            "It builds on [[frost-alarm]].\n",
        ),
        ("frost-alarm", "Frost alarm for growers", "Standalone.\n"),
    ] {
        store::write_idea(
            vault,
            &Idea {
                frontmatter: IdeaFrontmatter {
                    title: title.into(),
                    slug: slug.into(),
                    state: IdeaState::InDiscussion,
                    tags: vec![],
                    sources: vec![],
                    created: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
                    updated: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
                    extra: Default::default(),
                },
                body: body.into(),
            },
        )
        .unwrap();
    }
    store::append_turn(vault, "orchard-sensor", "user", "attack it").unwrap();
}

async fn related_route_bodies(route: &str, done: &str) -> Vec<String> {
    let mock = spawn(&["llama3.2"], ChatScript::Tokens(vec!["1. x".into()])).await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    seed_linked_pair(&vault_dir);
    idea_vault::index::reindex::reindex(&mut state.db.lock().unwrap(), &vault_dir).unwrap();

    let (status, _) = post_form(state.clone(), &format!("/idea/orchard-sensor/{route}"), "").await;
    assert_eq!(status, StatusCode::OK);
    support::web::poll_until(state, "/idea/orchard-sensor/pending", done).await;
    mock.chat_bodies()
}

#[tokio::test]
async fn related_block_reaches_the_model_through_skill_swarm_and_workflow_routes() {
    for (route, done) in [
        ("skill/premortem", "foil · premortem"),
        ("swarm", "foil · swarm"),
        ("workflow/interrogate", "foil · workflow interrogate"),
    ] {
        let bodies = related_route_bodies(route, done).await;
        let (audits, others): (Vec<&String>, Vec<&String>) = bodies
            .iter()
            .partition(|b| b.contains("You are the Auditor"));
        assert!(
            others.iter().any(|b| b.contains("Frost alarm for growers")),
            "{route}: the linked idea never reached the model"
        );
        assert!(
            audits
                .iter()
                .all(|b| !b.contains("Frost alarm for growers")),
            "{route}: the audit saw another idea"
        );
    }
}

/// Harvest `movable` with every lens answering `answer`; returns the state and vault.
async fn extracted(answer: &str) -> (idea_vault::web::state::AppState, std::path::PathBuf) {
    let mock = spawn(&["llama3.2"], ChatScript::Tokens(vec![answer.into()])).await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    seed(&vault_dir, IdeaState::InDiscussion);
    let (status, _) = post_form(state.clone(), "/idea/movable/extract", "").await;
    assert_eq!(status, StatusCode::OK);
    support::web::poll_until(state.clone(), "/idea/movable/pending", "foil · knowledge").await;
    (state, vault_dir)
}

fn lens_artifact(vault: &std::path::Path, lens: &str) -> idea_vault::domain::Artifact {
    store::read_artifacts(vault, "movable")
        .unwrap()
        .into_iter()
        .find(|a| a.frontmatter.lens.as_deref() == Some(lens))
        .unwrap_or_else(|| panic!("no {lens} artifact"))
}

#[tokio::test]
async fn skill_artifact_stamps_recipe() {
    let (state, vault_dir) = extracted("- a decision").await;
    let artifact = lens_artifact(&vault_dir, "extract-key-decisions");
    let recipe = artifact
        .frontmatter
        .recipe
        .expect("a skill artifact carries its recipe");
    let live = state.skills.snapshot();
    let skill = live.get("extract-key-decisions").unwrap();
    assert_eq!(recipe.skill.as_deref(), Some("extract-key-decisions"));
    assert_eq!(recipe.skill_digest.as_deref(), Some(skill.digest.as_str()));
    assert_eq!(recipe.skill_source.as_deref(), Some("built-in"));
    assert!(recipe.build.starts_with(env!("CARGO_PKG_VERSION")));
    assert!(recipe.contract.is_empty(), "a bullet answer is on contract");

    let page = format!("/idea/movable/artifact/{}.md", artifact.frontmatter.slug);
    let (status, body) = support::web::get(state, &page).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(&format!(
            "skill extract-key-decisions @ {} (built-in)",
            skill.digest
        )),
        "{body}"
    );
    assert!(!body.contains("recipe changed since"));
    assert!(!body.contains("provenance unknown"));
}

#[tokio::test]
async fn editing_vault_skill_shows_recipe_changed_badge() {
    let (state, vault_dir) = extracted("- a decision").await;
    let artifact = lens_artifact(&vault_dir, "extract-key-decisions");
    let page = format!("/idea/movable/artifact/{}.md", artifact.frontmatter.slug);

    let dir = state.skills.dir().to_path_buf();
    std::fs::create_dir_all(&dir).unwrap();
    let edited = format!(
        "{}\nAlso name who decided.\n",
        include_str!("../src/concepts/skills/extract-key-decisions.md").trim_end()
    );
    std::fs::write(dir.join("extract-key-decisions.md"), edited).unwrap();
    let (_, before) = support::web::get(state.clone(), &page).await;
    assert!(
        !before.contains("recipe changed since"),
        "the live book changes on reload, not on a file write"
    );
    let (status, _) = post_form(state.clone(), "/skills/reload", "").await;
    assert_eq!(status, StatusCode::OK);
    let (_, after) = support::web::get(state.clone(), &page).await;
    assert!(after.contains("recipe changed since"), "{after}");

    // An artifact from before recipes is "provenance unknown", never stale.
    let mut legacy = artifact.clone();
    legacy.frontmatter.slug = "legacy-finding".into();
    legacy.frontmatter.recipe = None;
    store::write_artifact(&vault_dir, "movable", &legacy).unwrap();
    let (_, old) = support::web::get(state, "/idea/movable/artifact/legacy-finding.md").await;
    assert!(old.contains("provenance unknown"));
    assert!(!old.contains("recipe changed since"));
}

#[tokio::test]
async fn extraction_artifact_shows_off_contract_badge() {
    let (state, vault_dir) = extracted("plain prose, no bullets").await;
    let artifact = lens_artifact(&vault_dir, "extract-open-questions");
    let recipe = artifact.frontmatter.recipe.unwrap();
    assert_eq!(recipe.contract.len(), 1, "{:?}", recipe.contract);
    assert!(recipe.contract[0].starts_with("extract-open-questions: off-contract: "));
    let page = format!("/idea/movable/artifact/{}.md", artifact.frontmatter.slug);
    let (_, body) = support::web::get(state.clone(), &page).await;
    assert!(body.contains(r#"class="recipe__off""#), "{body}");
    assert!(body.contains("extract-open-questions: off-contract: "));

    // The synthesis names every lens that fell off its contract.
    let synthesis = store::read_artifacts(&vault_dir, "movable")
        .unwrap()
        .into_iter()
        .find(|a| a.frontmatter.kind == idea_vault::domain::ArtifactKind::Synthesis)
        .unwrap();
    assert_eq!(
        synthesis.frontmatter.recipe.unwrap().contract.len(),
        idea_vault::concepts::knowledge::LENSES.len()
    );
}

#[tokio::test]
async fn truncated_but_shape_valid_lens_is_flagged_in_the_recipe() {
    // Every lens answers a valid bullet cut off at the output limit: the recorded outcome, not a
    // re-validation of the kept text, decides the recipe note (ADR-0040).
    let mock = spawn(
        &["llama3.2"],
        ChatScript::Finished {
            tokens: vec!["- a decision".into()],
            done_reason: "length".into(),
            prompt_eval_count: 100,
            eval_count: 512,
        },
    )
    .await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    seed(&vault_dir, IdeaState::InDiscussion);
    let (status, _) = post_form(state.clone(), "/idea/movable/extract", "").await;
    assert_eq!(status, StatusCode::OK);
    support::web::poll_until(state.clone(), "/idea/movable/pending", "foil · knowledge").await;
    let artifact = lens_artifact(&vault_dir, "extract-key-decisions");
    assert_eq!(
        artifact.body.trim(),
        "- a decision",
        "the valid text is kept"
    );
    let recipe = artifact.frontmatter.recipe.unwrap();
    assert_eq!(recipe.contract.len(), 1, "{:?}", recipe.contract);
    assert!(
        recipe.contract[0].starts_with("extract-key-decisions: off-contract: ")
            && recipe.contract[0].contains("output limit"),
        "{:?}",
        recipe.contract
    );
}

const RUN_ID: &str = "20260930T120000123Z-skill";

#[tokio::test]
async fn run_inspector_renders_calls_r50() {
    let (state, vault_dir) = support::web::test_state();
    seed(&vault_dir, IdeaState::InDiscussion);
    let (_, page) = support::web::get(state.clone(), "/idea/movable").await;
    assert!(!page.contains("last run"), "no journal, no link");

    let runs = vault_dir.join("movable").join(store::RUNS_DIR);
    std::fs::create_dir_all(&runs).unwrap();
    let journal = concat!(
        r#"{"type":"run_started","format_version":1,"run_id":"20260930T120000123Z-skill","slug":"movable","kind":"skill","build":"0.1.0","ts_ms":1}"#,
        "\n",
        r#"{"type":"llm_call","seq":0,"role":"critic","backend":"ollama","model":"llama3.2","temperature_milli":900,"request_sha256":"ab","response_text":"VERBATIM-ANSWER <b>","meta":{"usage":{"prompt_tokens":812,"output_tokens":64,"api_calls":1},"stop_reason":"length","num_ctx":4096,"ms":930}}"#,
        "\n",
        r#"{"type":"contract","call_seq":0,"contract":"ranked_list","outcome":{"status":"retried"}}"#,
        "\n",
        r#"{"type":"run_finished","outcome":"done","llm_calls":1,"ts_ms":2}"#,
        "\n",
    );
    std::fs::write(runs.join(format!("{RUN_ID}.jsonl")), journal).unwrap();
    std::fs::write(runs.join("20260101T000000000Z-chat.jsonl"), "").unwrap();

    let (_, page) = support::web::get(state.clone(), "/idea/movable").await;
    assert!(
        page.contains(&format!("/idea/movable/runs/{RUN_ID}")),
        "the last-run link names the newest journal"
    );

    let (status, body) =
        support::web::get(state.clone(), &format!("/idea/movable/runs/{RUN_ID}")).await;
    assert_eq!(status, StatusCode::OK);
    for needle in [
        "critic",
        "ollama · llama3.2",
        "ranked_list · retried",
        "tokens in 812 · out 64",
        "stop · length",
        "output truncated",
        "<details",
        "VERBATIM-ANSWER &#60;b&#62;",
    ] {
        assert!(body.contains(needle), "missing {needle}: {body}");
    }
    assert!(
        !body.contains("VERBATIM-ANSWER <b>"),
        "the response is escaped"
    );

    // Documented failures: an unknown run, a hostile run id, a missing idea.
    for uri in [
        "/idea/movable/runs/20990101T000000000Z-chat",
        "/idea/movable/runs/..%2Fidea",
        "/idea/nope/runs/20260930T120000123Z-skill",
    ] {
        let (status, _) = support::web::get(state.clone(), uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
    }
}
