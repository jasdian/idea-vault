//! ADR-0034 workflow stages against the mock Ollama: Ground skips at no cost without sources and
//! carries only verified anchors with them; Panel scorers see one proposal each, as the Auditor;
//! a Loop stops on a dry round; stage artifacts and the run record land only with the final
//! persist, never on a cancel or a failed final stage, and never as evidence. No live model.

mod support;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{TimeZone, Utc};
use idea_vault::ai::budget::ContextBudget;
use idea_vault::ai::{LlmBackend, OllamaClient};
use idea_vault::concepts::build_plan::gates::Evidence;
use idea_vault::concepts::skills::SkillRegistry;
use idea_vault::concepts::workflows::{
    run_workflow, Book, RunCtx, WorkflowOutcome, WorkflowRegistry,
};
use idea_vault::concepts::ConceptError;
use idea_vault::domain::evidence::POINTER_PREFIX;
use idea_vault::domain::{Artifact, ArtifactKind, Idea, IdeaFrontmatter, IdeaState, Name};
use idea_vault::sources::ResolvedSource;
use idea_vault::vault::store;
use support::{spawn, spawn_sequence, ChatScript, MockOllama};
use tokio::sync::Semaphore;

const SLUG: &str = "i";

fn seed_idea(vault: &Path) {
    store::write_idea(
        vault,
        &Idea {
            frontmatter: IdeaFrontmatter {
                title: "Streaming chat".into(),
                slug: SLUG.into(),
                state: IdeaState::InDiscussion,
                tags: vec![],
                sources: vec![],
                created: Utc.with_ymd_and_hms(2026, 9, 1, 10, 0, 0).unwrap(),
                updated: Utc.with_ymd_and_hms(2026, 9, 1, 10, 0, 0).unwrap(),
            },
            body: "Make chat.rs stream its replies instead of buffering them.\n".into(),
        },
    )
    .unwrap();
    store::append_conversation(
        vault,
        SLUG,
        "## user\nThe handler lives somewhere under src/web; `post_chat` is the entry point.\n",
    )
    .unwrap();
}

fn tokens(text: &str) -> ChatScript {
    ChatScript::Tokens(vec![text.to_string()])
}

/// A source tree the Ground stage can verify anchors against.
fn source_tree() -> (tempfile::TempDir, Vec<ResolvedSource>) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("src/web/routes")).unwrap();
    std::fs::create_dir_all(root.join("scripts")).unwrap();
    std::fs::write(
        root.join("src/web/routes/chat.rs"),
        "use axum;\n\npub async fn post_chat() {}\n",
    )
    .unwrap();
    std::fs::write(root.join("scripts/gate.sh"), "#!/bin/sh\ncargo test\n").unwrap();
    let sources = vec![ResolvedSource {
        name: Name::try_from("app").unwrap(),
        root,
    }];
    (dir, sources)
}

/// Everything a run needs besides the vault, owned so a test can also move it into a task.
struct Rig {
    llm: LlmBackend,
    sem: Arc<Semaphore>,
    book: Book,
    audit_on: bool,
    notes: Arc<Mutex<Vec<String>>>,
}

impl Rig {
    fn new(mock: &MockOllama, k: usize) -> Self {
        Rig {
            llm: LlmBackend::ollama_only(OllamaClient::new(mock.url.clone(), "llama3.2").unwrap()),
            sem: Arc::new(Semaphore::new(k)),
            book: Book::builtin(),
            audit_on: false,
            notes: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn with_sources(mut self, sources: Vec<ResolvedSource>) -> Self {
        self.llm = self.llm.with_turn_sources(sources);
        self
    }

    async fn run(&self, vault: &Path, name: &str) -> Result<WorkflowOutcome, ConceptError> {
        let notes = self.notes.clone();
        let progress = move |n: &str| notes.lock().unwrap().push(n.to_string());
        run_workflow(
            &RunCtx {
                llm: &self.llm,
                sem: &self.sem,
                book: &self.book,
                vault_dir: vault,
                idea_slug: SLUG,
                budget: ContextBudget::new(12_000),
                audit_on: self.audit_on,
                related: &|_| "## Related ideas\nRELATED-MARKER\n\n".to_string(),
                progress: &progress,
            },
            name,
        )
        .await
    }
}

/// The user prompt of one captured `/api/chat` body.
fn prompt(body: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(body).unwrap();
    let messages = v["messages"].as_array().unwrap();
    messages
        .iter()
        .rev()
        .find(|m| m["role"] == "user")
        .and_then(|m| m["content"].as_str())
        .unwrap_or_default()
        .to_string()
}

fn artifacts(vault: &Path) -> Vec<Artifact> {
    store::read_artifacts(vault, SLUG).unwrap()
}

fn kinds(vault: &Path) -> Vec<ArtifactKind> {
    let mut k: Vec<ArtifactKind> = artifacts(vault)
        .iter()
        .map(|a| a.frontmatter.kind)
        .collect();
    k.sort_by_key(|k| k.as_str());
    k
}

const PROPOSALS: [&str; 3] = [
    "## Proposal\n- alpha-spine plan streams through the router",
    "## Proposal\n- bravo cheap test first",
    "## Proposal\n- charlie ship this week plan",
];

/// P1 totals 8, P2 7, P3 5: P1 wins, P2 grafts on fit, P3 on evidence.
const SCORES: [&str; 3] = [
    "C1: 2 — a\nC2: 2 — a\nC3: 0 — a\nC4: 0 — a",
    "C4: 1 — b\nC3: 2 — b\nC2: 1 — b\nC1: 1 — b",
    "C1: 0 — c\nC2: 1 — c\nC3: 1 — c\nC4: 2 — c",
];

const SYNTHESIS: &str =
    "P1 is the spine.\nGrafted from P2: bravo cheap test first\nGrafted from P7: an invented graft";

fn panel_scripts() -> Vec<ChatScript> {
    PROPOSALS
        .iter()
        .chain(SCORES.iter())
        .chain([SYNTHESIS].iter())
        .map(|t| tokens(t))
        .collect()
}

const READER_ONE: &str = "- `src/web/routes/chat.rs:3` | `post_chat` | VERIFIED-CLAIM-ALPHA\n- `src/nope.rs:1` | `ghost` | DISPROVED-CLAIM-BRAVO";
const READER_TWO: &str = "- `src/web/routes/chat.rs:1` | `missing_fn` | DISPROVED-CLAIM-CHARLIE";

#[tokio::test]
async fn design_panel_without_sources_skips_ground_zero_calls() {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path());
    let mock = spawn_sequence(&["llama3.2"], panel_scripts()).await;
    let rig = Rig::new(&mock, 1);

    let outcome = rig.run(tmp.path(), "design-panel").await.unwrap();

    assert_eq!(
        mock.chat_bodies().len(),
        7,
        "3 proposals + 3 scores + synthesis"
    );
    let notes = rig.notes.lock().unwrap().clone();
    assert!(
        notes.iter().any(|n| n.starts_with(
            "workflow · design-panel · 1/4 ground: no sources attached — ground skipped"
        )),
        "{notes:?}"
    );
    assert!(
        notes.iter().any(|n| n.contains("2/4 panel: P1 wins 8/12")),
        "{notes:?}"
    );
    assert!(!mock
        .chat_bodies()
        .iter()
        .any(|b| prompt(b).contains("grounded map")));
    assert!(!kinds(tmp.path()).contains(&ArtifactKind::GroundMap));

    // Graft mode: the synthesizer is told the spine and the grafts; a graft line naming a
    // proposal that does not exist is stripped in code.
    let synth = prompt(&mock.chat_bodies()[6]);
    assert!(synth.contains("Take P1 as the spine"), "{synth}");
    assert!(
        synth.contains("from P2 on fit") && synth.contains("from P3 on evidence"),
        "{synth}"
    );
    assert!(outcome.synthesis.contains("Grafted from P2"));
    assert!(!outcome.synthesis.contains("P7"), "{}", outcome.synthesis);
    let convo = store::read_conversation(tmp.path(), SLUG).unwrap();
    assert!(convo.contains("## assistant (workflow: design-panel)\nP1 is the spine."));
    assert!(!convo.contains("an invented graft"));
}

#[tokio::test]
async fn design_panel_with_sources_carries_only_verified_claims() {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path());
    let (_src, sources) = source_tree();
    let mut scripts = vec![tokens(READER_ONE), tokens(READER_TWO)];
    scripts.extend(panel_scripts());
    let mock = spawn_sequence(&["llama3.2"], scripts).await;
    let rig = Rig::new(&mock, 1).with_sources(sources);

    rig.run(tmp.path(), "design-panel").await.unwrap();

    let bodies = mock.chat_bodies();
    assert_eq!(
        bodies.len(),
        9,
        "2 readers + 3 proposals + 3 scores + synthesis"
    );
    let reader = prompt(&bodies[0]);
    assert!(
        reader.contains("## Code outline") && reader.contains("src/web/"),
        "{reader}"
    );
    assert!(reader.contains("## Your angle\nwhere the code this idea changes lives"));
    for proposer in &bodies[2..5] {
        let p = prompt(proposer);
        let block = p
            .split("## Prior stage: grounded map")
            .nth(1)
            .unwrap_or_else(|| panic!("the grounded map is carried: {p}"));
        assert!(block.contains("VERIFIED-CLAIM-ALPHA"), "{block}");
        assert!(
            !block.contains("DISPROVED-CLAIM-BRAVO") && !block.contains("DISPROVED-CLAIM-CHARLIE"),
            "a disproved claim's text is never carried: {block}"
        );
        let absent = block.split("Does not exist").nth(1).unwrap_or_default();
        assert!(absent.contains("src/nope.rs"), "{block}");
        assert!(
            block.contains("- `chat.rs` is `src/web/routes/chat.rs`"),
            "{block}"
        );
    }
    let notes = rig.notes.lock().unwrap().clone();
    assert!(
        notes
            .iter()
            .any(|n| n.contains("1/4 ground: verified 1 of 3 (0 moved, 2 disproved)")),
        "{notes:?}"
    );
}

#[tokio::test]
async fn scorer_prompt_holds_exactly_one_proposal_and_no_related_block() {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path());
    let mock = spawn_sequence(&["llama3.2"], panel_scripts()).await;
    Rig::new(&mock, 1)
        .run(tmp.path(), "design-panel")
        .await
        .unwrap();
    let bodies = mock.chat_bodies();
    let markers = ["alpha-spine", "bravo cheap", "charlie ship"];
    for proposer in &bodies[..3] {
        assert!(
            prompt(proposer).contains("RELATED-MARKER"),
            "proposers see related ideas"
        );
    }
    for (k, scorer) in bodies[3..6].iter().enumerate() {
        let p = prompt(scorer);
        let seen: Vec<&str> = markers.iter().copied().filter(|m| p.contains(m)).collect();
        assert_eq!(
            seen,
            [markers[k]],
            "scorer {k} sees only its own proposal: {p}"
        );
        assert!(!p.contains("RELATED-MARKER"), "{p}");
        assert!(
            p.contains("## Rubric") && p.contains("C1 cost (weight 2)"),
            "{p}"
        );
    }
}

#[tokio::test]
async fn scorer_uses_auditor_profile() {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path());
    let mock = spawn_sequence(&["llama3.2"], panel_scripts()).await;
    let rig = Rig::new(&mock, 1);
    let mut settings = rig.llm.settings();
    settings.role_tuning = true;
    settings.role_profiles = idea_vault::concepts::agents::default_role_profiles();
    rig.llm.set_settings(settings);

    rig.run(tmp.path(), "design-panel").await.unwrap();

    let temperature = |body: &str| {
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        v["options"]["temperature"].as_f64().unwrap()
    };
    let bodies = mock.chat_bodies();
    let auditor = f64::from(
        idea_vault::concepts::agents::AgentRole::Auditor
            .default_profile()
            .temperature,
    );
    let critic = f64::from(
        idea_vault::concepts::agents::AgentRole::Critic
            .default_profile()
            .temperature,
    );
    for scorer in &bodies[3..6] {
        assert!(prompt(scorer).starts_with("You are the Auditor"));
        assert!((temperature(scorer) - auditor).abs() < 1e-6);
        assert!(
            (temperature(scorer) - critic).abs() > 1e-3,
            "not the skill file's critic role"
        );
    }
}

#[tokio::test]
async fn design_panel_writes_groundmap_scorecard_run_artifacts() {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path());
    let (_src, sources) = source_tree();
    let mut scripts = vec![tokens(READER_ONE), tokens(READER_TWO)];
    scripts.extend(panel_scripts());
    let mock = spawn_sequence(&["llama3.2"], scripts).await;
    let rig = Rig::new(&mock, 1).with_sources(sources);

    let outcome = rig.run(tmp.path(), "design-panel").await.unwrap();

    assert_eq!(
        kinds(tmp.path()),
        [
            ArtifactKind::GroundMap,
            ArtifactKind::Scorecard,
            ArtifactKind::WorkflowRun
        ]
    );
    let all = artifacts(tmp.path());
    let slugs: Vec<&str> = all.iter().map(|a| a.frontmatter.slug.as_str()).collect();
    assert_eq!(outcome.artifacts.len(), 3);
    for s in &outcome.artifacts {
        assert!(slugs.contains(&s.as_str()), "{s} written");
    }
    let by = |k: ArtifactKind| all.iter().find(|a| a.frontmatter.kind == k).unwrap();
    let ground = by(ArtifactKind::GroundMap);
    assert!(ground.frontmatter.slug.ends_with("-design-panel-1-ground"));
    assert!(
        ground.body.contains("DISPROVED-CLAIM-BRAVO") && ground.body.contains("| disproved |"),
        "the artifact keeps the full table: {}",
        ground.body
    );
    let scorecard = by(ArtifactKind::Scorecard);
    assert!(scorecard
        .frontmatter
        .slug
        .ends_with("-design-panel-2-panel"));
    assert!(
        scorecard.body.contains("| P1 | 2 | 2 | 0 | 0 | 8/12 |"),
        "{}",
        scorecard.body
    );
    assert!(scorecard.body.contains("## P3"));
    assert!(scorecard.body.contains("charlie ship this week plan"));
    let run = by(ArtifactKind::WorkflowRun);
    assert!(run.body.contains("| 1 | ground | ran |"), "{}", run.body);
    assert!(
        run.body
            .contains("| 3 | audit | skipped — audit off in Settings | 0 |"),
        "{}",
        run.body
    );
    assert!(run.body.contains("9 of 12 model calls"), "{}", run.body);
    for a in &all {
        assert_eq!(a.frontmatter.revises, None);
        assert!(a.frontmatter.answered.is_empty());
    }
    let convo = store::read_conversation(tmp.path(), SLUG).unwrap();
    let last = store::split_turns(&convo).pop().unwrap();
    let line = last
        .lines()
        .find(|l| l.starts_with("Stage artifacts: "))
        .expect("the turn names its stage artifacts");
    for s in &outcome.artifacts {
        assert!(line.contains(&format!("[[{s}]]")), "{line}");
    }
}

#[tokio::test]
async fn stage_concurrency_never_exceeds_k() {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path());
    let (_src, sources) = source_tree();
    let mock = spawn(
        &["llama3.2"],
        ChatScript::TokensAfterDelay {
            tokens: vec!["- `src/web/routes/chat.rs:3` | `post_chat` | ok".into()],
            delay_ms: 60,
        },
    )
    .await;
    let rig = Rig::new(&mock, 2).with_sources(sources);
    rig.run(tmp.path(), "design-panel").await.unwrap();
    assert_eq!(
        mock.max_in_flight(),
        2,
        "readers, proposals and scorers overlap up to K"
    );
}

#[tokio::test]
async fn exhaust_stops_on_dry_round() {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path());
    let mock = spawn_sequence(
        &["llama3.2"],
        vec![
            tokens("- the market is too small\n- churn will be high"),
            tokens("- pricing is unproven"),
            tokens("- support load grows with every user"),
            tokens("- The market is too small."),
            tokens("- churn will be high"),
            tokens("- pricing is unproven"),
            tokens("F1: CONFIRMED — a\nF2: CONFIRMED — b\nF3: CONFIRMED — c\nF4: CONFIRMED — d"),
            tokens("one converged position"),
        ],
    )
    .await;
    let mut rig = Rig::new(&mock, 1);
    rig.audit_on = true;

    let outcome = rig.run(tmp.path(), "exhaust").await.unwrap();

    let bodies = mock.chat_bodies();
    assert_eq!(
        bodies.len(),
        8,
        "2 rounds of 3 + audit + synthesis; the clean audit skips refine"
    );
    assert!(!prompt(&bodies[0]).contains("## Already found"));
    for round_two in &bodies[3..6] {
        let p = prompt(round_two);
        assert!(
            p.contains("## Already found (do not repeat)\n- the market is too small"),
            "{p}"
        );
    }
    assert_eq!(outcome.synthesis, "one converged position");
    let found = artifacts(tmp.path());
    let loop_artifact = found
        .iter()
        .find(|a| a.frontmatter.lens.as_deref() == Some("loop"))
        .expect("the loop's findings artifact");
    assert_eq!(loop_artifact.frontmatter.kind, ArtifactKind::Finding);
    assert!(
        loop_artifact
            .body
            .contains("Stopped: dry after 2 round(s), 6 call(s), 4 distinct item(s)."),
        "{}",
        loop_artifact.body
    );
    let run = found
        .iter()
        .find(|a| a.frontmatter.kind == ArtifactKind::WorkflowRun)
        .unwrap();
    assert!(
        run.body
            .contains("| 3 | refine | skipped — nothing refuted or uncertain | 0 |"),
        "{}",
        run.body
    );
    let notes = rig.notes.lock().unwrap().clone();
    assert!(
        notes
            .iter()
            .any(|n| n.contains("1/4 loop: round 2/3 · +0 new (4 total)")),
        "{notes:?}"
    );
}

/// Poll until the mock has seen `n` chat requests.
async fn until_calls(mock: &MockOllama, n: usize) {
    for _ in 0..400 {
        if mock.chat_bodies().len() >= n {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("the mock never saw {n} calls");
}

#[tokio::test]
async fn cancel_mid_panel_persists_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path());
    let before = store::read_conversation(tmp.path(), SLUG).unwrap();
    let mock = spawn(
        &["llama3.2"],
        ChatScript::TokensAfterDelay {
            tokens: vec!["## Proposal\n- a proposal".into()],
            delay_ms: 80,
        },
    )
    .await;
    let rig = Rig::new(&mock, 1);
    let vault: PathBuf = tmp.path().to_path_buf();
    let task = tokio::spawn(async move { rig.run(&vault, "design-panel").await });
    until_calls(&mock, 4).await; // proposals done, the first scorer in flight
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    assert!(artifacts(tmp.path()).is_empty());
    assert_eq!(store::read_conversation(tmp.path(), SLUG).unwrap(), before);
}

#[tokio::test]
async fn final_stage_failure_persists_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path());
    let before = store::read_conversation(tmp.path(), SLUG).unwrap();
    let mut scripts: Vec<ChatScript> = PROPOSALS
        .iter()
        .chain(SCORES.iter())
        .map(|t| tokens(t))
        .collect();
    scripts.push(ChatScript::EofAfter(vec!["P1 is".into()]));
    let mock = spawn_sequence(&["llama3.2"], scripts).await;

    let result = Rig::new(&mock, 1).run(tmp.path(), "design-panel").await;

    assert!(result.is_err(), "the synthesizer died");
    assert_eq!(mock.chat_bodies().len(), 7);
    assert!(
        artifacts(tmp.path()).is_empty(),
        "no scorecard and no run record"
    );
    assert_eq!(store::read_conversation(tmp.path(), SLUG).unwrap(), before);
}

/// The ready-to-build definition as it was before it gained a Ground stage.
const READY_TO_BUILD_WITHOUT_GROUND: &str = "---\nname: ready-to-build\ndescription: \"d\"\nstages:\n  - kind: fan_out\n    steps:\n      - {role: harvester, skill: extract-key-decisions}\n      - {role: harvester, skill: extract-durable-facts}\n      - {role: harvester, skill: extract-open-questions}\n      - {role: harvester, skill: extract-risks-assumptions}\n      - {role: harvester, skill: extract-next-actions}\n  - kind: audit\n  - kind: chain\n    role: synthesizer\n    skill: build-prompt\n---\n\nBefore Ground.\n";

const PLAN: &str = "## Goal\nStream the chat replies.\n\n## Settled\n- none\n\n## Verify first\n- none\n\n## Open questions\n- none\n\n## Plan\n- [ ] T1: Stream the handler\n  accept: `cargo test` → exit 0\n\n## Kill criteria\n- none";

fn ready_scripts() -> Vec<ChatScript> {
    vec![
        tokens("- stream replies"),
        tokens("- the handler is post_chat"),
        tokens("- which transport?"),
        tokens("- buffering hides errors"),
        tokens("- write the streaming test"),
        tokens("F1: CONFIRMED — a\nF2: CONFIRMED — b\nF3: UNCERTAIN — c\nF4: CONFIRMED — d\nF5: CONFIRMED — e"),
        tokens(PLAN),
    ]
}

#[tokio::test]
async fn ready_to_build_without_sources_matches_prior_calls_and_prompts() {
    let now = tempfile::tempdir().unwrap();
    let then = tempfile::tempdir().unwrap();
    let old_defs = tempfile::tempdir().unwrap();
    std::fs::write(
        old_defs.path().join("ready-to-build.md"),
        READY_TO_BUILD_WITHOUT_GROUND,
    )
    .unwrap();
    let skills = SkillRegistry::builtin();
    let (old_registry, issues) = WorkflowRegistry::load(old_defs.path(), &skills);
    assert!(issues.is_empty(), "{issues:?}");

    let mut bodies = Vec::new();
    let mut outcomes = Vec::new();
    for (vault, old) in [(now.path(), false), (then.path(), true)] {
        seed_idea(vault);
        let mock = spawn_sequence(&["llama3.2"], ready_scripts()).await;
        let mut rig = Rig::new(&mock, 1);
        rig.audit_on = true;
        if old {
            rig.book = Book {
                skills: Arc::new(skills.clone()),
                workflows: Arc::new(old_registry.clone()),
            };
        }
        outcomes.push(rig.run(vault, "ready-to-build").await.unwrap());
        bodies.push(mock.chat_bodies());
    }
    assert_eq!(bodies[0].len(), 7, "5 harvesters + auditor + planner");
    assert_eq!(bodies[0], bodies[1], "the same calls with the same prompts");
    assert!(
        outcomes[0].artifacts.is_empty(),
        "no stage artifact, no run record"
    );
    assert_eq!(kinds(now.path()), [ArtifactKind::BuildPlan]);
}

#[tokio::test]
async fn ready_to_build_with_sources_planner_gets_grounded_map_within_third_share() {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path());
    let (_src, sources) = source_tree();
    let mut scripts = vec![tokens(READER_ONE), tokens(READER_TWO)];
    scripts.extend(ready_scripts());
    let mock = spawn_sequence(&["llama3.2"], scripts).await;
    let mut rig = Rig::new(&mock, 1).with_sources(sources);
    rig.audit_on = true;

    let outcome = rig.run(tmp.path(), "ready-to-build").await.unwrap();

    let bodies = mock.chat_bodies();
    assert_eq!(
        bodies.len(),
        9,
        "2 readers + 5 harvesters + auditor + planner"
    );
    assert!(prompt(&bodies[0]).contains("## Your angle\nwhere the change lands"));
    assert!(prompt(&bodies[1]).contains("## Your angle\nscripts, config and tests"));
    let planner = prompt(&bodies[8]);
    let start = planner
        .find("## Prior stage: grounded map")
        .expect("the planner reads the grounded map");
    let preamble = planner.find("## How to use the findings").unwrap();
    let end = start
        + planner[start..]
            .find("\n\n## Idea\n")
            .expect("the idea follows the carried blocks");
    assert!(start < preamble, "the map sits ahead of the findings");
    let third = 12_000 / 3;
    assert!(preamble - start <= third / 2 + 2, "{}", preamble - start);
    assert!(
        end - start <= third,
        "map + preamble + findings = {} > {third}",
        end - start
    );
    assert!(planner[start..preamble].contains("VERIFIED-CLAIM-ALPHA"));

    // The pointer turn is the gates' own; the run record names the plan.
    let convo = store::read_conversation(tmp.path(), SLUG).unwrap();
    let last = store::split_turns(&convo).pop().unwrap();
    assert!(
        last.starts_with(&format!(
            "## assistant (workflow: ready-to-build)\n{POINTER_PREFIX}"
        )),
        "{last}"
    );
    assert!(!last.contains("Stage artifacts:"), "{last}");
    let all = artifacts(tmp.path());
    let plan = all
        .iter()
        .find(|a| a.frontmatter.kind == ArtifactKind::BuildPlan)
        .unwrap();
    let run = all
        .iter()
        .find(|a| a.frontmatter.kind == ArtifactKind::WorkflowRun)
        .unwrap();
    assert!(
        run.body.contains(&format!(
            "final output: build plan [[{}]]",
            plan.frontmatter.slug
        )),
        "{}",
        run.body
    );
    assert_eq!(outcome.artifacts.len(), 2, "ground map + run record");
}

#[tokio::test]
async fn stage_artifacts_are_never_memory_evidence() {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path());
    let mock = spawn_sequence(&["llama3.2"], panel_scripts()).await;
    Rig::new(&mock, 1)
        .run(tmp.path(), "design-panel")
        .await
        .unwrap();

    let losing = "charlie ship this week plan";
    let scorecard = artifacts(tmp.path())
        .into_iter()
        .find(|a| a.frontmatter.kind == ArtifactKind::Scorecard)
        .unwrap();
    assert!(
        scorecard.body.contains(losing),
        "the stage artifact holds it"
    );
    let idea = store::read_idea(tmp.path(), SLUG).unwrap();
    let convo = store::read_conversation(tmp.path(), SLUG).unwrap();
    assert!(
        !convo.contains(losing),
        "no turn carries a stage artifact's text"
    );
    let evidence = Evidence::new(&idea.body, &convo);
    assert_eq!(evidence.locate(losing), None);
    assert!(
        evidence.locate("P1 is the spine").is_some(),
        "the workflow's own turn stays evidence, as every workflow turn is"
    );
}

#[tokio::test]
async fn progress_note_sequence_for_design_panel() {
    let tmp = tempfile::tempdir().unwrap();
    seed_idea(tmp.path());
    let (_src, sources) = source_tree();
    let mut scripts = vec![tokens(READER_ONE), tokens(READER_TWO)];
    scripts.extend(panel_scripts());
    let mock = spawn_sequence(&["llama3.2"], scripts).await;
    let rig = Rig::new(&mock, 1).with_sources(sources);

    rig.run(tmp.path(), "design-panel").await.unwrap();

    // The single `jobs::set_note` string is the whole progress UI (spec §2.6), rendered verbatim,
    // so its grammar is pinned here: `workflow · {name} · {i}/{n} {kind}: {detail} · calls
    // {c}/{ceiling}`, calls counted against the ceiling the chip showed before the run.
    let notes = rig.notes.lock().unwrap().clone();
    let expected = [
        "1/4 ground: mapping the sources · calls 0/12",
        "1/4 ground: reader 1/2 · calls 1/12",
        "1/4 ground: reader 2/2 · calls 2/12",
        "1/4 ground: verified 1 of 3 (0 moved, 2 disproved) · calls 2/12",
        "2/4 panel: 3 proposals · calls 2/12",
        "2/4 panel: proposal 1/3 · calls 3/12",
        "2/4 panel: proposal 2/3 · calls 4/12",
        "2/4 panel: proposal 3/3 · calls 5/12",
        "2/4 panel: scoring 1/3 · calls 6/12",
        "2/4 panel: scoring 2/3 · calls 7/12",
        "2/4 panel: scoring 3/3 · calls 8/12",
        "2/4 panel: P1 wins 8/12 · calls 8/12",
        "4/4 synthesize: converging 3 findings · calls 8/12",
    ]
    .map(|tail| format!("workflow · design-panel · {tail}"));
    assert_eq!(notes, expected, "audit is off here, so stage 3 is silent");
}
