//! G14, the tree lint, end to end through the public `parse` and `gates::run`: the task graph's
//! references, cycles and `[?]` inheritance, and the code-owned `score`, `model` and `wave`
//! fields a stored artifact carries back.

use idea_vault::ai::sources::SourceProbe;
use idea_vault::concepts::build_plan::gates::{run, Evidence, GateInputs, GateReport};
use idea_vault::concepts::build_plan::plan::{parse, parse_artifact, render, BuildPlan, Item};

const COUNTED: &str = "`cargo test --quiet x` → exit 0, ≥3 passed";

fn task(id: &str, subject: &str, fields: &[(&str, &str)]) -> String {
    let mut out = format!("- [ ] {id}: {subject}\n");
    for (k, v) in fields {
        out.push_str(&format!("  {k}: {v}\n"));
    }
    out
}

fn gate(answer: &str) -> (BuildPlan, GateReport) {
    let evidence = Evidence::new("A local vault for ideas.", "");
    let probe = SourceProbe::default();
    let mut plan = parse(answer).expect("the fixture is a usable plan");
    let report = run(
        &mut plan,
        &GateInputs {
            evidence: &evidence,
            open_artifact: None,
            audit: None,
            probe: &probe,
        },
    );
    (plan, report)
}

fn answer() -> String {
    let plan = [
        task(
            "T1",
            "Add the parser",
            &[("touches", "`src/a.rs`"), ("accept", COUNTED)],
        ),
        task(
            "T2",
            "Add the reader",
            &[("touches", "`src/b.rs`"), ("accept", COUNTED)],
        ),
        task(
            "T3",
            "Add the parser cache",
            &[
                ("touches", "`src/a.rs`"),
                ("depends", "T1"),
                ("accept", COUNTED),
            ],
        ),
        task(
            "T4",
            "Add the writer",
            &[
                ("touches", "`src/d.rs`"),
                ("depends", "T9"),
                ("accept", COUNTED),
            ],
        ),
        task(
            "T5",
            "Add the index",
            &[
                ("touches", "`src/e.rs`"),
                ("depends", "T6"),
                ("accept", COUNTED),
            ],
        ),
        task(
            "T6",
            "Add the search",
            &[
                ("touches", "`src/f.rs`"),
                ("depends", "T5"),
                ("accept", COUNTED),
            ],
        ),
        task(
            "T7",
            "Add the spread",
            &[
                ("touches", "`src/g.rs`"),
                ("depends", "Q1"),
                ("accept", COUNTED),
            ],
        ),
        task(
            "T8",
            "Add the spread view",
            &[
                ("touches", "`src/h.rs`"),
                ("depends", "T7"),
                ("accept", COUNTED),
            ],
        ),
    ]
    .concat();
    format!(
        "## Goal\nShip the smallest useful slice.\n\n## Settled\n- none\n\n## Verify first\n- none\n\n\
## Open questions\n- Q1: Which spread do we quote?\n\n## Plan\n{plan}\n## Kill criteria\n- none\n"
    )
}

fn by_id<'a>(plan: &'a BuildPlan, id: &str) -> &'a Item {
    plan.tasks
        .iter()
        .find(|t| t.id == id)
        .expect("task present")
}

#[test]
fn tree_gate_names_cycles_and_dangling_refs_and_inherits_owner_blocks() {
    let (plan, report) = gate(&answer());
    assert!(
        report
            .notes
            .contains(&"dependency cycle: T5 → T6 → T5".to_string()),
        "{:?}",
        report.notes
    );
    assert!(
        report
            .notes
            .contains(&"T4: dropped dangling reference T9".to_string()),
        "{:?}",
        report.notes
    );
    let t8 = by_id(&plan, "T8");
    assert!(t8.needs_owner, "{t8:?}");
    assert!(t8.markers.contains(&"blocked by T7".to_string()), "{t8:?}");
    assert_eq!(report.tally.get("blocked_by_owner"), Some(&1));
}

#[test]
fn tree_gate_writes_scores_models_and_waves_that_the_artifact_reads_back() {
    let (plan, report) = gate(&answer());
    let fields = |id: &str| {
        let t = by_id(&plan, id);
        (t.field("wave"), t.field("score"), t.field("model"))
    };
    assert_eq!(fields("T1"), (Some("1"), Some("00000"), Some("sonnet")));
    assert_eq!(fields("T2").0, Some("1"));
    assert_eq!(fields("T4").0, Some("1"));
    assert_eq!(fields("T3").0, Some("2"), "T3 shares src/a.rs with T1");
    for owner in ["T5", "T6", "T7", "T8"] {
        assert_eq!(fields(owner).0, None, "{owner} needs the owner, so no wave");
    }
    assert_eq!(report.tally.get("waves"), Some(&2));

    let body = render(&plan);
    let stored = parse_artifact(&body).expect("renders a usable artifact");
    assert_eq!(by_id(&stored, "T3").field("wave"), Some("2"));
    assert_eq!(by_id(&stored, "T1").field("score"), Some("00000"));
    let forged = parse(&body).expect("usable");
    assert_eq!(
        by_id(&forged, "T1").field("wave"),
        None,
        "a model answer cannot author a wave"
    );
}
