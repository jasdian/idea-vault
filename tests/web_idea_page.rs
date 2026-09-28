//! Web handler tests for R2, the idea page: sanitized markdown rendering, transcript, memory
//! panel, and the D20 degraded/available compose-box states.

mod support;

use axum::http::StatusCode;
use chrono::{TimeZone, Utc};
use idea_vault::domain::{Idea, IdeaFrontmatter, IdeaState, MemoryFact, MemoryFactFrontmatter};
use idea_vault::vault::store;
use support::web::{get, post_form, test_state, test_state_with_ollama};
use support::{spawn, ChatScript};

fn seed(vault: &std::path::Path, state: IdeaState, body: &str) {
    store::write_idea(
        vault,
        &Idea {
            frontmatter: IdeaFrontmatter {
                title: "Sharp Idea".into(),
                slug: "sharp-idea".into(),
                state,
                tags: vec![],
                sources: vec![],
                created: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
                updated: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
            },
            body: body.into(),
        },
    )
    .unwrap();
}

#[tokio::test]
async fn idea_page_renders_sanitized_body_transcript_and_memory() {
    let (state, vault_dir) = test_state();
    seed(
        &vault_dir,
        IdeaState::InDiscussion,
        "Some **bold** claim.\n\n<script>alert('xss')</script>\n",
    );
    store::append_conversation(
        &vault_dir,
        "sharp-idea",
        "## user\nfirst *probing* question\n",
    )
    .unwrap();
    store::append_conversation(&vault_dir, "sharp-idea", "## assistant\na counterpoint\n").unwrap();
    store::write_memory_fact(
        &vault_dir,
        "sharp-idea",
        &MemoryFact {
            frontmatter: MemoryFactFrontmatter {
                slug: "core-tension".into(),
                title: "Core tension".into(),
                tags: vec![],
                created: Utc.with_ymd_and_hms(2026, 7, 7, 11, 0, 0).unwrap(),
                links: vec![],
            },
            body: "The one durable conclusion.\n".into(),
        },
    )
    .unwrap();
    store::rebuild_memory_index(&vault_dir, "sharp-idea").unwrap();

    let (status, body) = get(state, "/idea/sharp-idea").await;
    assert_eq!(status, StatusCode::OK);

    // Body: markdown rendered, the injected script stripped (sanitized server-side). The page's
    // own trusted <script> (copy-button JS in base.html) is fine — assert the XSS payload is gone.
    assert!(body.contains("<strong>bold</strong>"));
    assert!(
        !body.contains("alert('xss')"),
        "injected scripts must never reach the browser"
    );
    // Transcript: both turns rendered with roles and markdown.
    assert!(body.contains("<em>probing</em>"));
    assert!(body.contains("a counterpoint"));
    assert!(body.contains("turn--you") && body.contains("turn--foil"));
    // Memory panel: index entry visible.
    assert!(body.contains("[[core-tension]]") && body.contains("The one durable conclusion."));
    // Degraded AI (harness refuses): banner with the Unreachable remedy + disabled compose (D20).
    assert!(body.contains("The foil is offline"));
    assert!(body.contains("ollama serve"), "Unreachable remedy copy");
}

#[tokio::test]
async fn compose_box_is_live_when_ollama_is_available() {
    let mock = spawn(&["llama3.2"], ChatScript::Tokens(vec![])).await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    seed(&vault_dir, IdeaState::InDiscussion, "body\n");

    let (status, body) = get(state, "/idea/sharp-idea").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("hx-post=\"/idea/sharp-idea/chat\""));
    assert!(!body.contains("The foil is offline"));
}

#[tokio::test]
async fn draft_page_has_oob_targets_but_no_oob_fragments() {
    // The full page must render the badge and the (empty) actions container — the anchors the
    // out-of-band swaps replace later — but never the `hx-swap-oob` fragments themselves, which
    // belong only to transcript responses (duplicate-id guard for the
    // transcript_inner vs respond_with_transcript split).
    let mock = spawn(&["llama3.2"], ChatScript::Tokens(vec![])).await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    seed(&vault_dir, IdeaState::Draft, "body\n");

    let (status, body) = get(state, "/idea/sharp-idea").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("id=\"idea-state\""));
    assert!(body.contains("id=\"idea-actions\""), "OOB target exists");
    assert!(!body.contains("hx-swap-oob"), "full pages carry no OOB");
    assert!(
        !body.contains("/idea/sharp-idea/store"),
        "a Draft offers no store control"
    );
}

#[tokio::test]
async fn stored_idea_shows_reopen_panel_not_compose() {
    let (state, vault_dir) = test_state();
    seed(&vault_dir, IdeaState::Stored, "Consolidated statement.\n");

    let (status, body) = get(state, "/idea/sharp-idea").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("hx-post=\"/idea/sharp-idea/reopen\""));
    assert!(
        !body.contains("/idea/sharp-idea/chat"),
        "no compose when Stored"
    );
    assert!(body.contains("state--stored"));
}

#[tokio::test]
async fn missing_idea_is_404() {
    let (state, _vault_dir) = test_state();
    let (status, _) = get(state, "/idea/ghost").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn malformed_slug_is_404_not_500() {
    let (state, _vault_dir) = test_state();
    // Invalid slug charset (space, uppercase) must be answered like a missing idea.
    let (status, _) = get(state.clone(), "/idea/Bad%20Slug").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = get(state, "/idea/%2e%2e").await;
    assert_ne!(status, StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn model_missing_disables_compose_with_pull_hint() {
    // Ollama server up, but the configured model (llama3.2) is not in the tags list.
    let mock = spawn(&["mistral"], ChatScript::Tokens(vec![])).await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    seed(&vault_dir, IdeaState::InDiscussion, "body\n");

    let (status, body) = get(state, "/idea/sharp-idea").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("The foil is offline"));
    assert!(
        body.contains("ollama pull llama3.2"),
        "D20 per-state remedy"
    );
    // No composer is rendered while the model is unavailable — just the note.
    assert!(
        !body.contains("/idea/sharp-idea/chat"),
        "compose box absent when offline"
    );
}

#[tokio::test]
async fn deleting_a_memory_fact_removes_it_and_shrinks_reopen_context() {
    let (state, vault_dir) = test_state();
    seed(&vault_dir, IdeaState::Stored, "A stored idea.\n");
    for (slug, title) in [("keep-me", "Keep me"), ("drop-me", "Drop me")] {
        store::write_memory_fact(
            &vault_dir,
            "sharp-idea",
            &MemoryFact {
                frontmatter: MemoryFactFrontmatter {
                    slug: slug.into(),
                    title: title.into(),
                    tags: vec![],
                    created: Utc.with_ymd_and_hms(2026, 7, 7, 11, 0, 0).unwrap(),
                    links: vec![],
                },
                body: format!("Body of {title}.\n"),
            },
        )
        .unwrap();
    }
    store::rebuild_memory_index(&vault_dir, "sharp-idea").unwrap();

    let (status, body) = post_form(state, "/idea/sharp-idea/memory/drop-me/delete", "").await;
    assert_eq!(status, StatusCode::OK);
    // The re-rendered panel keeps the other fact and drops the deleted one.
    assert!(body.contains("keep-me") && !body.contains("drop-me"));
    // On disk: the fact file is gone and MEMORY.md no longer references it (reopen loads less).
    assert!(!vault_dir.join("sharp-idea/memory/drop-me.md").is_file());
    assert!(vault_dir.join("sharp-idea/memory/keep-me.md").is_file());
    let idx = store::read_memory_index(&vault_dir, "sharp-idea").unwrap();
    assert_eq!(idx.entries.len(), 1);
}

#[tokio::test]
async fn the_spine_strip_shows_coverage_the_next_move_and_wrong_turns() {
    // The actions block only renders while the foil is reachable (D20).
    let mock = spawn(&["llama3.2"], ChatScript::Tokens(vec!["x".into()])).await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    seed(&vault_dir, IdeaState::InDiscussion, "An idea.\n");
    store::append_turn(&vault_dir, "sharp-idea", "user", "go").unwrap();
    store::append_turn(&vault_dir, "sharp-idea", "assistant", "a reply").unwrap();

    // Fresh discussion: nothing covered, steelman suggested, store flagged as untested.
    let (status, page) = get(state.clone(), "/idea/sharp-idea").await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("aria-label=\"ideation spine\""));
    assert!(page.contains("○ steelman") && page.contains("○ attack"));
    assert!(page.contains("hx-post=\"/idea/sharp-idea/skill/steelman\""));
    assert!(page.contains("next › steelman"));
    assert!(page.contains("no attack move has run yet"));

    // A build prompt with no attack before it is a wrong turn; a steelman covers its stage.
    store::append_turn(
        &vault_dir,
        "sharp-idea",
        "assistant (skill: steelman)",
        "best case",
    )
    .unwrap();
    store::append_turn(
        &vault_dir,
        "sharp-idea",
        "assistant (skill: build-prompt)",
        "```markdown\nx\n```",
    )
    .unwrap();
    let (_, page) = get(state, "/idea/sharp-idea").await;
    assert!(page.contains("✓ steelman") && page.contains("✓ capstone"));
    assert!(page.contains("next › premortem"));
    assert!(page.contains("before any attack move ran"));
}

fn seed_idea(vault: &std::path::Path, slug: &str, title: &str, tags: &[&str], body: &str) {
    store::write_idea(
        vault,
        &Idea {
            frontmatter: IdeaFrontmatter {
                title: title.into(),
                slug: slug.into(),
                state: IdeaState::InDiscussion,
                tags: tags.iter().map(|t| t.to_string()).collect(),
                sources: vec![],
                created: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
                updated: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
            },
            body: body.into(),
        },
    )
    .unwrap();
}

fn reindex_state(state: &idea_vault::app::AppState, vault: &std::path::Path) {
    let mut conn = state.db.lock().unwrap();
    idea_vault::index::reindex::reindex(&mut conn, vault).unwrap();
}

fn related_section(page: &str) -> &str {
    let start = page.find("id=\"related\"").expect("related panel present");
    let rest = &page[start..];
    let end = rest
        .find("<aside class=\"artifacts\"")
        .unwrap_or(rest.len());
    &rest[..end]
}

#[tokio::test]
async fn idea_page_shows_related_panel() {
    let (state, vault) = test_state();
    seed_idea(&vault, "alpha", "Alpha Idea", &[], "Builds on [[beta]].\n");
    seed_idea(&vault, "beta", "Beta Idea", &[], "Standalone.\n");
    reindex_state(&state, &vault);

    let (status, body) = get(state, "/idea/alpha").await;
    assert_eq!(status, StatusCode::OK);
    let panel = related_section(&body);
    assert!(panel.contains("href=\"/idea/beta\""));
    assert!(panel.contains("Beta Idea"));
    assert!(panel.contains("link:"));
    assert!(
        !panel.contains("alpha"),
        "own slug must be redacted: {panel}"
    );
}

#[tokio::test]
async fn idea_page_related_panel_shows_tag_drift() {
    let (state, vault) = test_state();
    seed_idea(&vault, "alpha", "Alpha Idea", &["system-design"], "One.\n");
    seed_idea(&vault, "beta", "Beta Idea", &["systems-design"], "Two.\n");
    reindex_state(&state, &vault);

    let (_, body) = get(state, "/idea/alpha").await;
    let panel = related_section(&body);
    assert!(panel.contains("Tag drift"));
    assert!(panel.contains("system-design") && panel.contains("systems-design"));
    assert!(panel.contains("beta"));
}

#[tokio::test]
async fn idea_page_related_panel_empty_state() {
    let (state, vault) = test_state();
    seed_idea(&vault, "alpha", "Alpha Idea", &[], "Alone.\n");
    reindex_state(&state, &vault);

    let (status, body) = get(state, "/idea/alpha").await;
    assert_eq!(status, StatusCode::OK);
    assert!(related_section(&body)
        .contains("No related ideas yet — links and shared tags create them."));
}

#[tokio::test]
async fn idea_page_related_panel_hides_below_noise_floor() {
    let (state, vault) = test_state();
    for i in 0..10 {
        let tags: &[&str] = if i < 8 { &["common"] } else { &[] };
        seed_idea(
            &vault,
            &format!("idea-{i}"),
            &format!("Ideanumber {i}"),
            tags,
            "Body.\n",
        );
    }
    reindex_state(&state, &vault);

    let (status, body) = get(state, "/idea/idea-0").await;
    assert_eq!(status, StatusCode::OK);
    let panel = related_section(&body);
    assert!(!panel.contains("Ideanumber"), "{panel}");
    assert!(panel.contains("No related ideas yet"));
}

#[tokio::test]
async fn idea_page_related_panel_labels_two_hop_ideas_and_escapes_titles() {
    let (state, vault) = test_state();
    seed_idea(&vault, "ay", "Ay", &[], "Links [[bee]].\n");
    seed_idea(
        &vault,
        "bee",
        "Bee <script>x</script>",
        &[],
        "Links [[sea]].\n",
    );
    seed_idea(&vault, "sea", "Sea", &[], "Leaf.\n");
    reindex_state(&state, &vault);

    let (_, page) = get(state, "/idea/ay").await;
    let panel = related_section(&page);
    assert!(panel.contains("via bee"), "got {panel}");
    assert!(panel.contains("/idea/sea"), "got {panel}");
    assert!(
        !panel.contains("<script>x</script>"),
        "title must be escaped: {panel}"
    );
}

#[tokio::test]
async fn idea_page_related_panel_caps_drift_carriers() {
    let (state, vault) = test_state();
    seed_idea(&vault, "own", "Own", &["system-design"], "Body.\n");
    for i in 0..8 {
        seed_idea(
            &vault,
            &format!("carrier-{i}"),
            "Carrier",
            &["systems-design"],
            "Body.\n",
        );
    }
    reindex_state(&state, &vault);

    let (_, page) = get(state, "/idea/own").await;
    let panel = related_section(&page);
    assert!(panel.contains("carrier-4"), "got {panel}");
    assert!(!panel.contains("carrier-5"), "got {panel}");
    assert!(panel.contains("+3 more"), "got {panel}");
}

#[tokio::test]
async fn idea_page_related_panel_shows_fact_titles() {
    let (state, vault) = test_state();
    seed_idea(&vault, "alpha", "Alpha Idea", &[], "Builds on [[beta]].\n");
    seed_idea(&vault, "beta", "Beta Idea", &[], "Standalone.\n");
    for (slug, title, hour) in [
        ("oldest", "Oldest orchard fact", 1),
        ("middle", "Middle orchard fact", 2),
        ("newest", "Newest orchard fact", 3),
    ] {
        store::write_memory_fact(
            &vault,
            "beta",
            &MemoryFact {
                frontmatter: MemoryFactFrontmatter {
                    slug: slug.into(),
                    title: title.into(),
                    tags: vec![],
                    created: Utc.with_ymd_and_hms(2026, 7, 7, hour, 0, 0).unwrap(),
                    links: vec![],
                },
                body: "Fact body.\n".into(),
            },
        )
        .unwrap();
    }
    reindex_state(&state, &vault);

    let (status, body) = get(state, "/idea/alpha").await;
    assert_eq!(status, StatusCode::OK);
    let panel = related_section(&body);
    assert!(panel.contains("Newest orchard fact"), "got {panel}");
    assert!(panel.contains("Middle orchard fact"), "got {panel}");
    assert!(!panel.contains("Oldest orchard fact"), "got {panel}");
}

#[tokio::test]
async fn idea_page_related_panel_unavailable_on_poisoned_lock() {
    let (state, vault) = test_state();
    seed_idea(&vault, "alpha", "Alpha Idea", &[], "Builds on [[beta]].\n");
    seed_idea(&vault, "beta", "Beta Idea", &[], "Standalone.\n");
    reindex_state(&state, &vault);

    let db = state.db.clone();
    let joined = std::thread::spawn(move || {
        let _guard = db.lock().unwrap();
        panic!("poison the index mutex");
    })
    .join();
    assert!(joined.is_err());
    assert!(state.db.is_poisoned());

    let (status, body) = get(state, "/idea/alpha").await;
    assert_eq!(status, StatusCode::OK);
    let panel = related_section(&body);
    assert!(
        panel.contains("Related ideas are unavailable right now."),
        "got {panel}"
    );
    assert!(!panel.contains("No related ideas yet"), "got {panel}");
}
