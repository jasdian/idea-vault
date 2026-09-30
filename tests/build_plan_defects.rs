//! The catalogued build-plan defects (bp-1 rows R1–R12, bp-3's worst cases) pinned as gate
//! regressions: each fixture is a small synthetic discussion plus a model answer in the
//! build-plan grammar, run through the public `parse` and `gates::run`, asserting where the
//! offending claim landed and which marker or note it carries. No model, no real vault.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers outside #[test] fns; HTC-6 binds shipping code"
)]

use idea_vault::ai::sources::SourceProbe;
use idea_vault::concepts::audit::Label;
use idea_vault::concepts::build_plan::gates::{
    run, AuditView, AuditedFinding, Evidence, GateInputs, GateReport, OpenArtifact,
};
use idea_vault::concepts::build_plan::plan::{parse, BuildPlan, Item, Provenance};
use idea_vault::domain::Name;
use idea_vault::sources::ResolvedSource;

const IDEA: &str = "A local vault for ideas.";

const TASK: &str = "- [ ] T1: Write the parser\n  accept: `cargo test` → exit 0";

#[derive(Default)]
struct Answer<'a> {
    settled: &'a str,
    verify: &'a str,
    fence: &'a str,
    plan: &'a str,
    kills: &'a str,
}

impl Answer<'_> {
    fn render(&self) -> String {
        fn or_none(s: &str) -> &str {
            if s.is_empty() {
                "- none"
            } else {
                s
            }
        }
        let fence = if self.fence.is_empty() {
            String::new()
        } else {
            format!("## Fence\n{}\n\n", self.fence)
        };
        format!(
            "## Goal\nShip the smallest useful slice.\n\n## Settled\n{}\n\n## Verify first\n{}\n\n\
## Open questions\n- none\n\n{fence}## Plan\n{}\n\n## Kill criteria\n{}\n",
            or_none(self.settled),
            or_none(self.verify),
            if self.plan.is_empty() {
                TASK
            } else {
                self.plan
            },
            or_none(self.kills),
        )
    }
}

struct Extra<'a> {
    open_artifact: Option<&'a OpenArtifact>,
    audit: Option<&'a AuditView>,
    probe: &'a SourceProbe,
}

fn gate_answer(conversation: &str, answer: &str, extra: Extra) -> (BuildPlan, GateReport) {
    let evidence = Evidence::new(IDEA, conversation);
    let mut plan = parse(answer).expect("the fixture is a usable plan");
    let report = run(
        &mut plan,
        &GateInputs {
            evidence: &evidence,
            open_artifact: extra.open_artifact,
            audit: extra.audit,
            probe: extra.probe,
            answered: &[],
        },
    );
    (plan, report)
}

fn gate_with(conversation: &str, answer: Answer, extra: Extra) -> (BuildPlan, GateReport) {
    gate_answer(conversation, &answer.render(), extra)
}

fn gate(conversation: &str, answer: Answer) -> (BuildPlan, GateReport) {
    let probe = SourceProbe::default();
    gate_with(
        conversation,
        answer,
        Extra {
            open_artifact: None,
            audit: None,
            probe: &probe,
        },
    )
}

fn probe_over(files: &[(&str, &str)]) -> (tempfile::TempDir, SourceProbe) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    for (rel, body) in files {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }
    let probe = SourceProbe::new(&[ResolvedSource {
        name: Name::try_from("code").unwrap(),
        root,
    }]);
    (dir, probe)
}

fn markers(item: &Item) -> String {
    item.markers.join(" | ")
}

fn only<'a>(items: &'a [Item], section: &str) -> &'a Item {
    assert_eq!(items.len(), 1, "expected one item in {section}: {items:#?}");
    &items[0]
}

fn with_text<'a>(items: &'a [Item], needle: &str) -> &'a Item {
    items
        .iter()
        .find(|i| i.text.contains(needle))
        .unwrap_or_else(|| panic!("no item containing {needle:?} in {items:#?}"))
}

#[test]
fn row01_a_wrong_anchor_range_goes_to_verify_first_with_a_sed_check() {
    let (_dir, probe) = probe_over(&[
        (
            "src/ai/budget.rs",
            "use x;\n\npub struct ContextInput<'a> {\n    idea_body: &'a str,\n    turns: &'a str,\n}\n",
        ),
        ("src/memory/load.rs", "pub fn load_context(slug: &str) {}\n"),
    ]);
    let (plan, _) = gate_with(
        "## user\nContext is built from one slug, see `src/ai/budget.rs:4-6` and `load_context`.\n",
        Answer {
            settled: "- S1: Context is built from one slug at `src/ai/budget.rs:4-6` in `load_context`\n  \
                      quote: \"Context is built from one slug\"",
            ..Answer::default()
        },
        Extra {
            open_artifact: None,
            audit: None,
            probe: &probe,
        },
    );
    assert!(plan.settled.is_empty(), "{:#?}", plan.settled);
    let item = only(&plan.verify, "Verify first");
    assert!(
        markers(item).contains("symbol missing: load_context not in src/ai/budget.rs"),
        "{}",
        markers(item)
    );
    assert_eq!(
        item.field("check"),
        Some("`sed -n 4,6p -- 'src/ai/budget.rs' | grep -nF -- 'load_context'`")
    );
}

#[test]
fn row02_a_truncated_anchor_range_is_caught_as_moved() {
    let (_dir, probe) = probe_over(&[(
        "src/index/reindex.rs",
        "fn collect() {\n    targets.push(link);\n    links.len();\n}\nfn store() {\n    \
         db.execute(\"INSERT INTO backlinks VALUES (NULL)\");\n}\n",
    )]);
    let (plan, _) = gate_with(
        "## user\nFact links never resolve, see `src/index/reindex.rs:2-3`.\n",
        Answer {
            settled:
                "- S1: Fact links never resolve, see `src/index/reindex.rs:2-3` for `backlinks`\n  \
                      quote: \"Fact links never resolve\"",
            ..Answer::default()
        },
        Extra {
            open_artifact: None,
            audit: None,
            probe: &probe,
        },
    );
    assert!(plan.settled.is_empty(), "{:#?}", plan.settled);
    let item = only(&plan.verify, "Verify first");
    assert!(
        markers(item).contains("moved: symbol found at line 6"),
        "{}",
        markers(item)
    );
}

#[test]
fn row03_a_foil_coined_link_syntax_is_verify_first_without_a_source() {
    let (plan, _) = gate(
        "## user\nWe want fact-to-fact links between stored facts.\n\n\
## assistant\nA `[[slug#fact]]` reference resolves to a fact-to-fact edge.\n",
        Answer {
            settled: "- S1: A `[[slug#fact]]` reference resolves to a fact-to-fact edge\n  \
                      quote: \"reference resolves to a fact-to-fact edge\"",
            ..Answer::default()
        },
    );
    assert!(plan.settled.is_empty(), "{:#?}", plan.settled);
    let item = only(&plan.verify, "Verify first");
    assert!(
        markers(item).contains("foil-coined: [[slug#fact]] — only the foil wrote it"),
        "{}",
        markers(item)
    );
    assert_eq!(
        item.field("check"),
        Some("`grep -rnF -- '[[slug#fact]]' .`")
    );
}

#[test]
fn row04_a_count_without_a_count_command_is_recounted() {
    let conversation = "## user\nOf 56 stored facts, 6 contain cheapest disproof in their text.\n";
    let (plan, _) = gate(
        conversation,
        Answer {
            settled:
                "- S1: 6 facts contain cheapest disproof\n  quote: \"6 contain cheapest disproof\"",
            ..Answer::default()
        },
    );
    assert!(plan.settled.is_empty(), "{:#?}", plan.settled);
    let item = only(&plan.verify, "Verify first");
    assert!(
        markers(item).contains("recount: no count command"),
        "{}",
        markers(item)
    );

    let (plan, _) = gate(
        conversation,
        Answer {
            settled: "- S1: 6 facts contain cheapest disproof\n  quote: \"6 contain cheapest disproof\"\n  \
                      count: `grep -rli 'cheapest.disproof' vault`",
            ..Answer::default()
        },
    );
    assert_eq!(plan.settled.len(), 1, "a stated count command keeps it");
}

#[test]
fn row05_facts_subtracted_from_pairs_is_mixed_units() {
    let (plan, _) = gate(
        "## user\nOf 36 pairs, 6 facts are contaminated.\n\n\
## assistant\nExcluding the 6 facts, it stays ahead on the remaining 30 pairs.\n",
        Answer {
            settled: "- S1: Excluding the 6 facts, it stays ahead on the remaining 30 pairs\n  \
                      quote: \"it stays ahead on the remaining 30 pairs\"",
            ..Answer::default()
        },
    );
    assert!(plan.settled.is_empty(), "{:#?}", plan.settled);
    let item = only(&plan.verify, "Verify first");
    assert!(
        markers(item).contains("mixed units: facts vs pairs"),
        "{}",
        markers(item)
    );
}

#[test]
fn row06_an_open_premise_stays_verify_first_and_a_writing_check_is_marked() {
    let (plan, _) = gate(
        "## assistant\nDoes the budget enforce a per-idea token cap? Worth checking.\n",
        Answer {
            verify: "- P1: The budget enforces a per-idea cap\n  \
                     check: `grep -nE 'per.idea|MAX_FACTS' src/ai/budget.rs`\n\
                     - P2: The cap was raised twice\n  \
                     check: `sed -i s/cap/limit/ src/ai/budget.rs`",
            ..Answer::default()
        },
    );
    assert_eq!(plan.verify.len(), 2, "{:#?}", plan.verify);
    let premise = with_text(&plan.verify, "per-idea cap");
    assert!(premise.markers.is_empty(), "{}", markers(premise));
    assert_eq!(
        premise.field("check"),
        Some("`grep -nE 'per.idea|MAX_FACTS' src/ai/budget.rs`")
    );
    let writer = with_text(&plan.verify, "raised twice");
    assert!(
        markers(writer).contains("check is not read-only"),
        "{}",
        markers(writer)
    );

    let answer = "## Goal\nShip it.\n\n## Settled\n- none\n\n## Open questions\n- none\n\n\
## Plan\n- [ ] T1: Write the parser\n  accept: `cargo test` → exit 0\n\n## Kill criteria\n- none\n";
    let probe = SourceProbe::default();
    let (_, report) = gate_answer(
        "## user\nPlan it.\n",
        answer,
        Extra {
            open_artifact: None,
            audit: None,
            probe: &probe,
        },
    );
    assert!(
        report
            .notes
            .iter()
            .any(|n| n == "missing section: ## Verify first — placeholder"),
        "{:?}",
        report.notes
    );
}

#[test]
fn row07_a_task_touching_a_fenced_path_needs_the_owner() {
    let (plan, _) = gate(
        "## user\nDo not touch `src/memory/load.rs`; inject the related block alongside.\n",
        Answer {
            fence: "- F1: `src/memory/load.rs`",
            plan: "- [ ] T1: Wire the related block into the loader\n  \
                   touches: `src/memory/load.rs`\n  accept: `cargo test` → exit 0",
            ..Answer::default()
        },
    );
    let task = only(&plan.tasks, "Plan");
    assert!(task.needs_owner);
    assert!(
        markers(task).contains("touches fenced src/memory/load.rs"),
        "{}",
        markers(task)
    );
    with_text(
        &plan.open,
        "T1 touches fenced `src/memory/load.rs` — which wins?",
    );
}

#[test]
fn row08_parallel_streams_sharing_a_file_get_a_dependency() {
    let (plan, report) = gate(
        "## user\nTwo streams can run in parallel.\n",
        Answer {
            plan: "- [ ] T1: Store fact links\n  touches: `src/index/reindex.rs`\n  \
                   accept: `cargo test links` → exit 0\n\
                   - [ ] T2: Resolve fact links\n  touches: `src/index/reindex.rs`\n  \
                   accept: `cargo test resolve` → exit 0",
            ..Answer::default()
        },
    );
    let t2 = with_text(&plan.tasks, "Resolve");
    assert_eq!(t2.list("depends"), ["T1"]);
    assert!(
        markers(t2).contains("added: shares src/index/reindex.rs with T1"),
        "{}",
        markers(t2)
    );
    assert_eq!(report.tally.get("repaired"), Some(&1));
}

#[test]
fn row09_scope_transfer_is_a_known_miss() {
    let (plan, _) = gate(
        "## assistant\nResearch: the sqlite-vec brute-force scan handles 1M vectors in 17-35ms.\n",
        Answer {
            settled: "- S1: Brute-force cosine in plain Rust handles 1M vectors in 17-35ms\n  \
                      quote: \"handles 1M vectors in 17-35ms\"",
            ..Answer::default()
        },
    );
    let item = only(&plan.settled, "Settled");
    assert!(item.markers.is_empty(), "{}", markers(item));
    assert_eq!(item.provenance, Some(Provenance::Foil));
    assert!(plan.verify.is_empty() && plan.open.is_empty() && plan.quarantined.is_empty());
}

#[test]
fn row10_a_next_free_number_is_rechecked_at_bootstrap() {
    let (plan, _) = gate(
        "## user\nThe next free ADR in `docs/adr` is 0026.\n",
        Answer {
            settled:
                "- S1: The next free ADR in `docs/adr` is 0026\n  quote: \"The next free ADR\"",
            ..Answer::default()
        },
    );
    assert!(plan.settled.is_empty(), "{:#?}", plan.settled);
    let item = only(&plan.verify, "Verify first");
    assert!(
        markers(item).contains("freshness: re-check at bootstrap"),
        "{}",
        markers(item)
    );
    assert_eq!(item.field("check"), Some("`ls -- 'docs/adr' | tail -1`"));
}

#[test]
fn row11_an_uncounted_or_unstated_figure_is_recounted() {
    let conversation = "## user\nThe fact corpus holds 56 facts across six ideas.\n";
    let (plan, _) = gate(
        conversation,
        Answer {
            settled: "- S1: The fact corpus holds 56 facts\n  \
                      quote: \"The fact corpus holds 56 facts\"",
            ..Answer::default()
        },
    );
    assert!(plan.settled.is_empty(), "{:#?}", plan.settled);
    let stale = only(&plan.verify, "Verify first");
    assert!(
        markers(stale).contains("recount: no count command"),
        "{}",
        markers(stale)
    );

    let (plan, _) = gate(
        conversation,
        Answer {
            settled: "- S1: The fact corpus holds 68 facts\n  quote: \"The fact corpus holds\"",
            ..Answer::default()
        },
    );
    let unstated = only(&plan.verify, "Verify first");
    assert!(unstated.text.contains("68"));
    assert!(
        markers(unstated).contains("figure not in the discussion: 68"),
        "{}",
        markers(unstated)
    );
}

#[test]
fn row11_a_counted_stale_figure_is_a_known_miss() {
    let (plan, _) = gate(
        "## user\nThe fact corpus holds 56 facts across six ideas.\n",
        Answer {
            settled: "- S1: The fact corpus holds 56 facts\n  quote: \"The fact corpus holds\"\n  \
                      count: `find vault -name '*.md'`",
            ..Answer::default()
        },
    );
    let counted = only(&plan.settled, "Settled");
    assert!(counted.text.contains("56"));
}

#[test]
fn row12_a_backslash_escaped_quote_still_grounds() {
    let (plan, _) = gate(
        "## user\nOf 56 stored facts, 6 contain \"cheapest disproof\" in two ideas.\n",
        Answer {
            settled: "- S1: Method-echo contamination is measurable\n  \
                      quote: \"6 contain \\\"cheapest disproof\\\" in two ideas\"",
            ..Answer::default()
        },
    );
    assert!(plan.quarantined.is_empty(), "{:#?}", plan.quarantined);
    let item = only(&plan.settled, "Settled");
    assert_eq!(item.provenance, Some(Provenance::Owner));
}

#[test]
fn bp3_freeze_or_dwell_is_opened() {
    let artifact = OpenArtifact {
        name: "20260803-194038-open-questions".into(),
        items: vec![
            "Freeze or dwell: freeze the zone snapshot at entry, or dwell hysteresis on the live \
             regime label?"
                .into(),
            "Should close be gated symmetrically with open?".into(),
        ],
    };
    let probe = SourceProbe::default();
    let (plan, _) = gate_with(
        "## user\nThe regime label flickers near the boundary.\n\n\
## assistant\nWe can freeze the zone snapshot at entry, or dwell hysteresis on the live regime \
label — pick one explicitly.\n\n\
## assistant\nClose must not be gated symmetrically with open.\n",
        Answer {
            settled: "- S1: Freeze the zone snapshot at entry\n  \
                      quote: \"freeze the zone snapshot at entry\"\n\
                      - S2: Close is never gated symmetrically with open\n  \
                      quote: \"Close must not be gated symmetrically with open\"",
            ..Answer::default()
        },
        Extra {
            open_artifact: Some(&artifact),
            audit: None,
            probe: &probe,
        },
    );
    assert!(plan.settled.is_empty(), "{:#?}", plan.settled);
    let freeze = with_text(&plan.open, "proposed: Freeze the zone snapshot at entry");
    assert!(
        markers(freeze).contains("its quote sits beside \"pick one\""),
        "{}",
        markers(freeze)
    );
    let close = with_text(&plan.open, "proposed: Close is never gated");
    assert!(
        markers(close).contains("opened from Settled: listed in 20260803-194038-open-questions"),
        "{}",
        markers(close)
    );
}

#[test]
fn bp3_live_tension_is_opened() {
    let (plan, _) = gate(
        "## user\nWe govern spend per call.\n\n\
## assistant\nThe per-token cap and the per-call cap both remain live for now.\n\n\
## assistant\nThere is a tension here: the auditor runs on every finding.\n",
        Answer {
            settled: "- S1: The per-call cap governs spend\n  \
                      quote: \"the per-token cap and the per-call cap\"\n\
                      - S2: The auditor runs on every finding\n  \
                      quote: \"the auditor runs on every finding\"",
            ..Answer::default()
        },
    );
    assert!(plan.settled.is_empty(), "{:#?}", plan.settled);
    let live = with_text(&plan.open, "per-call cap governs");
    assert!(
        markers(live).contains("its quote sits beside \"remain live\""),
        "{}",
        markers(live)
    );
    let tension = with_text(&plan.open, "auditor runs");
    assert!(
        markers(tension).contains("its quote sits beside \"tension\""),
        "{}",
        markers(tension)
    );
}

#[test]
fn bp3_continue_anyway_is_flagged() {
    let (plan, report) = gate(
        "## user\nBacktest first, then scaffold.\n",
        Answer {
            plan: "- [ ] T1: Backtest the spec at a pessimistic spread\n  \
                   accept: `python run.py` → last line is KILL or SURVIVES\n\
                   - [ ] T2: Scaffold the trader workspace\n  depends: T1\n  \
                   accept: `cargo test` → exit 0",
            kills: "- K1: If the backtest prints KILL, log it and continue anyway\n  \
                    checked by: T1\n  gates: T2",
            ..Answer::default()
        },
    );
    let kill = only(&plan.kills, "Kill criteria");
    assert!(
        markers(kill).contains("kill criterion says continue anyway"),
        "{}",
        markers(kill)
    );
    assert!(
        report
            .notes
            .iter()
            .any(|n| n == "K1: kill criterion says continue anyway"),
        "{:?}",
        report.notes
    );
}

#[test]
fn bp3_an_invented_token_is_quarantined() {
    let (plan, report) = gate(
        "## user\nThe cap must scale with leverage.\n",
        Answer {
            settled: "- S1: The cap is enforced by `MaxLeverageCap`\n  \
                      quote: \"The cap must scale with leverage\"",
            ..Answer::default()
        },
    );
    assert!(plan.settled.is_empty() && plan.verify.is_empty());
    let item = only(&plan.quarantined, "Quarantined");
    assert_eq!(
        item.field("reason"),
        Some("MaxLeverageCap appears nowhere in the discussion or the sources")
    );
    assert_eq!(report.tally.get("quarantined"), Some(&1));
}

#[test]
fn bp3_a_foil_coined_token_is_verify_first() {
    let (plan, _) = gate(
        "## user\nThe job must run every night.\n\n\
## assistant\nSchedule it with APScheduler inside the web process.\n",
        Answer {
            settled: "- S1: The nightly job runs under APScheduler\n  \
                      quote: \"Schedule it with APScheduler\"",
            ..Answer::default()
        },
    );
    assert!(plan.settled.is_empty() && plan.quarantined.is_empty());
    let item = only(&plan.verify, "Verify first");
    assert!(
        markers(item).contains("foil-coined: APScheduler — only the foil wrote it"),
        "{}",
        markers(item)
    );
    assert_eq!(item.field("check"), Some("`grep -rnF -- 'APScheduler' .`"));
}

#[test]
fn bp3_mixed_unit_arithmetic_is_verify_first() {
    let (plan, _) = gate(
        "## assistant\n36 pairs minus 6 facts leaves 30 pairs.\n",
        Answer {
            settled: "- S1: 36 pairs minus 6 facts leaves 30 pairs\n  \
                      quote: \"36 pairs minus 6 facts\"",
            ..Answer::default()
        },
    );
    assert!(plan.settled.is_empty(), "{:#?}", plan.settled);
    let item = only(&plan.verify, "Verify first");
    assert!(
        markers(item).contains("mixed units: pairs vs facts"),
        "{}",
        markers(item)
    );
}

#[test]
fn bp3_a_refuted_finding_carried_as_settled_is_quarantined_with_the_reason() {
    let audit = AuditView {
        findings: vec![AuditedFinding {
            text: "The budget enforces a per-idea token cap".into(),
            lenses: vec!["extract-decisions".into()],
            label: Label::Refuted,
            reason: "the budget is one byte budget per call, not per idea".into(),
        }],
        failed: false,
        uniform_pass: false,
    };
    let probe = SourceProbe::default();
    let (plan, _) = gate_with(
        "## assistant\nThe budget enforces a per-idea token cap on every call.\n",
        Answer {
            settled: "- S1: The budget enforces a per-idea token cap\n  \
                      quote: \"The budget enforces a per-idea token cap\"",
            ..Answer::default()
        },
        Extra {
            open_artifact: None,
            audit: Some(&audit),
            probe: &probe,
        },
    );
    assert!(plan.settled.is_empty() && plan.open.is_empty());
    let item = only(&plan.quarantined, "Quarantined");
    assert_eq!(
        item.field("reason"),
        Some("refuted by the audit: the budget is one byte budget per call, not per idea")
    );
}

#[test]
fn bp3_prose_acceptance_needs_the_owner() {
    let (plan, report) = gate(
        "## user\nTune it until it feels right.\n",
        Answer {
            plan:
                "- [ ] T1: Tune the thresholds\n  accept: the owner confirms the chart looks right",
            ..Answer::default()
        },
    );
    let task = only(&plan.tasks, "Plan");
    assert!(task.needs_owner);
    assert!(
        markers(task).contains("no runnable accept"),
        "{}",
        markers(task)
    );
    assert_eq!(report.tally.get("needs_owner"), Some(&1));
}

fn accept_of(plan_line: &str) -> (BuildPlan, GateReport) {
    gate(
        "## user\nBuild the parser.\n",
        Answer {
            plan: plan_line,
            ..Answer::default()
        },
    )
}

#[test]
fn accept_repair_wraps_an_unbackticked_runner_command_and_keeps_the_task() {
    for (written, fixed) in [
        (
            "cargo test --lib parser → exit 0",
            "`cargo test --lib parser` → exit 0",
        ),
        (
            "cargo test --lib parser → it doesn't panic, 3 passed",
            "`cargo test --lib parser` → it doesn't panic, 3 passed",
        ),
        (
            "grep -c TODO src/a.rs -> prints 0",
            "`grep -c TODO src/a.rs` -> prints 0",
        ),
        (
            "docker compose config → exits 0",
            "`docker compose config` → exits 0",
        ),
        (
            "rg \"fn .*->\" src → 3 matches",
            "`rg \"fn .*->\" src` → 3 matches",
        ),
        ("rg 'a → b' src -> 0 hits", "`rg 'a → b' src` -> 0 hits"),
        ("go test ./... → ok", "`go test ./...` → ok"),
        ("make test → exit 0", "`make test` → exit 0"),
        ("just build-all → exit 0", "`just build-all` → exit 0"),
    ] {
        let (plan, report) = accept_of(&format!("- [ ] T1: Parse\n  accept: {written}"));
        let task = only(&plan.tasks, "Plan");
        assert_eq!(task.field("accept"), Some(fixed));
        assert!(!task.needs_owner, "{written}: {}", markers(task));
        assert!(
            markers(task).contains("accept repaired"),
            "{}",
            markers(task)
        );
        assert!(!markers(task).contains("no runnable accept"));
        assert_eq!(report.tally.get("needs_owner"), None);
    }
}

#[test]
fn accept_repair_leaves_prose_and_conditionless_accepts_owner_bound() {
    for written in [
        "the owner confirms the chart looks right",
        "make sure the parser works → it does",
        "cargo test →",
        "terraform apply → exit 0",
        "cargo test passes",
        "go through the flow → works",
        "just check that it builds → ok",
        "make each test pass → ok",
        "cargo test passes and the parser is fast → yes",
        "cargo test finishes. Then check the output → ok",
        "rg \"fn .*→ src → 3 matches",
    ] {
        let (plan, _) = accept_of(&format!("- [ ] T1: Parse\n  accept: {written}"));
        let task = only(&plan.tasks, "Plan");
        assert!(task.needs_owner, "{written}");
        assert!(markers(task).contains("no runnable accept"), "{written}");
        assert!(!markers(task).contains("accept repaired"), "{written}");
        assert_eq!(task.field("accept"), Some(written));
    }
}

#[test]
fn accept_repair_does_not_launder_a_destructive_command() {
    let (plan, report) =
        accept_of("- [ ] T1: Reset\n  accept: docker compose down -v → volumes gone");
    let task = only(&plan.tasks, "Plan");
    assert!(task.needs_owner);
    assert!(
        markers(task).contains("destructive command"),
        "{}",
        markers(task)
    );
    assert_eq!(report.tally.get("needs_owner"), Some(&1));
    assert_eq!(
        task.field("accept"),
        Some("docker compose down -v → volumes gone")
    );
    assert!(!markers(task).contains("accept repaired"));
}

const GATE_SOURCES: [(&str, &str); 7] = [
    (
        "build_plan/plan.rs",
        include_str!("../src/concepts/build_plan/plan.rs"),
    ),
    (
        "gates/mod.rs",
        include_str!("../src/concepts/build_plan/gates/mod.rs"),
    ),
    (
        "gates/claims.rs",
        include_str!("../src/concepts/build_plan/gates/claims.rs"),
    ),
    (
        "gates/sources.rs",
        include_str!("../src/concepts/build_plan/gates/sources.rs"),
    ),
    (
        "gates/tasks.rs",
        include_str!("../src/concepts/build_plan/gates/tasks.rs"),
    ),
    (
        "gates/leaf.rs",
        include_str!("../src/concepts/build_plan/gates/leaf.rs"),
    ),
    (
        "gates/tree.rs",
        include_str!("../src/concepts/build_plan/gates/tree.rs"),
    ),
];

const MODEL_CALL_NEEDLES: [&str; 18] = [
    "ask_on_contract",
    "llmbackend",
    ".chat(",
    "chat_stream",
    "run_agent",
    "ollama",
    "claude_code",
    "ai::backend",
    "ai::claude",
    "reqwest",
    "std::process",
    "tokio::process",
    "skills::invoke",
    "run_workflow",
    "extract_knowledge",
    "swarm::swarm",
    "audit::audit(",
    "process::command",
];

fn model_calls(source: &str) -> Vec<&'static str> {
    let code = source
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
        .to_lowercase();
    MODEL_CALL_NEEDLES
        .iter()
        .copied()
        .filter(|n| code.contains(n))
        .collect()
}

#[test]
fn gates_source_never_names_a_model_call() {
    for snippet in [
        "let answer = backend.chat(request).await?;",
        "skills::ask_on_contract(llm, prompt, contract).await",
        "fn gate(llm: &LlmBackend) {}",
        "agents::run_agent(role, idea).await",
        "use crate::ai::ollama::OllamaClient;",
        "use crate::ai::claude_code::ClaudeCode;",
        "use std::process::Command;",
        "use std::process::{Command, Stdio};",
        "tokio::process::Command::new(\"claude\")",
        "let reply = backend.chat_stream(msgs, on_token).await?;",
        "let body = reqwest::Client::new().post(url).send().await?;",
        "skills::invoke(llm, skill, idea).await",
        "workflows::run_workflow(llm, workflow, idea).await",
        "knowledge::extract_knowledge(llm, transcript).await",
        "swarm::swarm(llm, idea, roles).await",
        "audit::audit(llm, findings).await",
    ] {
        assert!(
            !model_calls(snippet).is_empty(),
            "the needles miss a model call: {snippet}"
        );
    }
    assert!(model_calls("// calls .chat( on the backend\nfn pure() {}").is_empty());
    assert!(
        model_calls("    // swarm::swarm, reqwest and std::process stay out\nfn pure() {}")
            .is_empty()
    );
    for (name, source) in GATE_SOURCES {
        assert!(source.contains("fn "), "{name} was not embedded");
        assert_eq!(model_calls(source), Vec::<&str>::new(), "{name}");
    }
}

const WIRE_PLAN: &str =
    "- [ ] T1: Write the parser\n  touches: `src/parser.rs`\n  accept: `cargo test` → exit 0\n\
- [ ] T2: Log the spread\n  touches: `src/log.rs`\n  accept: `cargo test` → exit 0";

#[test]
fn premise_wired_a_check_grepping_a_touched_file_gets_wired() {
    let (plan, report) = gate(
        "## user\nBuild the parser.\n",
        Answer {
            verify: "- P1: The tokenizer exists\n  check: `grep -rnF tokenize src/parser.rs`",
            plan: WIRE_PLAN,
            ..Answer::default()
        },
    );
    let t1 = with_text(&plan.tasks, "parser");
    assert_eq!(t1.depends_premises(), ["P1"], "{:?}", t1.fields);
    assert!(
        t1.markers.iter().any(|m| m == "premise P1 wired"),
        "{}",
        markers(t1)
    );
    let t2 = with_text(&plan.tasks, "spread");
    assert!(t2.depends_premises().is_empty(), "{:?}", t2.fields);
    assert_eq!(report.tally.get("premises_wired"), Some(&1));
}

#[test]
fn premise_wired_a_premise_sharing_only_a_plain_word_is_not_wired() {
    let (plan, report) = gate(
        "## user\nBuild the parser.\n",
        Answer {
            verify: "- P1: The parser handles quoting\n  check: `grep -rnF quoting src/lexer.rs`",
            plan: WIRE_PLAN,
            ..Answer::default()
        },
    );
    for t in &plan.tasks {
        assert!(t.depends_premises().is_empty(), "{:?}", t.fields);
    }
    assert_eq!(report.tally.get("premises_wired"), None);
}

const GENERIC_PLAN: &str = "- [ ] T1: Return `None` from `main` in `src`\n  touches: `src/parser.rs`\n  accept: `cargo test` → exit 0\n\
- [ ] T2: Cache the `HashMap` and `tokenize_all`\n  touches: `src/cache.rs`\n  accept: `cargo test` → exit 0";

#[test]
fn premise_wired_generic_one_word_spans_do_not_wire() {
    let (plan, report) = gate(
        "## user\nBuild the parser.\n",
        Answer {
            verify:
                "- P1: Lookups may be empty `None` or `main` or `src`\n  check: `grep -rn None`",
            plan: GENERIC_PLAN,
            ..Answer::default()
        },
    );
    for t in &plan.tasks {
        assert!(t.depends_premises().is_empty(), "{:?}", t.fields);
    }
    assert_eq!(report.tally.get("premises_wired"), None);
}

#[test]
fn premise_wired_identifier_shaped_one_word_spans_wire() {
    let (plan, _) = gate(
        "## user\nBuild the parser.\n",
        Answer {
            verify: "- P1: A `HashMap` is fast enough\n  check: `grep -rn tokenize_all`",
            plan: GENERIC_PLAN,
            ..Answer::default()
        },
    );
    let t2 = with_text(&plan.tasks, "Cache");
    assert_eq!(t2.depends_premises(), ["P1"], "{:?}", t2.fields);
    let t1 = with_text(&plan.tasks, "Return");
    assert!(t1.depends_premises().is_empty(), "{:?}", t1.fields);
}

#[test]
fn premise_wired_a_directory_level_check_path_does_not_wire() {
    let (plan, report) = gate(
        "## user\nBuild the parser.\n",
        Answer {
            verify: "- P1: The parser dir is clean\n  check: `grep -rnF tokenize src/parser`",
            plan: "- [ ] T1: Write the parser\n  touches: `src/parser.rs`\n  accept: `cargo test` → exit 0\n\
- [ ] T2: Tune the parser\n  touches: `src/parser/lex.rs`\n  accept: `cargo test` → exit 0",
            ..Answer::default()
        },
    );
    for t in &plan.tasks {
        assert!(t.depends_premises().is_empty(), "{:?}", t.fields);
    }
    assert_eq!(report.tally.get("premises_wired"), None);
}
