//! G13, the leaf gate: each planned task is checked against the leaf invariants (one commit
//! subject, one root, one counted check) and the split triggers, through the public `parse` and
//! `gates::run`. The gate only marks and tallies; it never demotes a task to `[?]`.

use idea_vault::ai::sources::SourceProbe;
use idea_vault::concepts::build_plan::gates::{run, Evidence, GateInputs, GateReport};
use idea_vault::concepts::build_plan::plan::{parse, BuildPlan, Item};

const COUNTED: &str = "`cargo test --quiet x` → exit 0, ≥3 passed";

fn gate_plan(plan_section: &str) -> (BuildPlan, GateReport) {
    let answer = format!(
        "## Goal\nShip the smallest useful slice.\n\n## Settled\n- none\n\n## Verify first\n- none\n\n\
## Open questions\n- none\n\n## Plan\n{plan_section}\n\n## Kill criteria\n- none\n"
    );
    let evidence = Evidence::new("A local vault for ideas.", "");
    let probe = SourceProbe::default();
    let mut plan = parse(&answer).expect("the fixture is a usable plan");
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

fn task(id: &str, subject: &str, fields: &[(&str, &str)]) -> String {
    let mut out = format!("- [ ] {id}: {subject}\n");
    for (k, v) in fields {
        out.push_str(&format!("  {k}: {v}\n"));
    }
    out
}

fn filler() -> String {
    task(
        "T9",
        "Document the parser",
        &[
            ("touches", "`docs/parser.md`"),
            ("accept", "`grep -c parser docs/parser.md` → 1 or more"),
        ],
    )
}

fn by_id<'a>(plan: &'a BuildPlan, id: &str) -> &'a Item {
    plan.tasks
        .iter()
        .find(|t| t.id == id)
        .expect("task present")
}

fn marked(item: &Item, prefix: &str) -> bool {
    item.markers.iter().any(|m| m.starts_with(prefix))
}

fn tally(report: &GateReport, key: &str) -> usize {
    report.tally.get(key).copied().unwrap_or(0)
}

#[test]
fn leaf_gate_and_or_a_verb_list_in_the_subject_asks_for_one_commit() {
    let plan_section = [
        task(
            "T1",
            "Add the parser, wire the route",
            &[("touches", "`src/a.rs`"), ("accept", COUNTED)],
        ),
        task(
            "T2",
            "Add the `load and save` helper",
            &[("touches", "`src/b.rs`"), ("accept", COUNTED)],
        ),
        task(
            "T3",
            "Parse ids, aliases, dependencies",
            &[("touches", "`src/c.rs`"), ("accept", COUNTED)],
        ),
        task(
            "T4",
            "Add the reader and the writer",
            &[("touches", "`src/d.rs`"), ("accept", COUNTED)],
        ),
        filler(),
    ]
    .concat();
    let (plan, _) = gate_plan(&plan_section);
    assert!(
        marked(by_id(&plan, "T1"), "split: one commit"),
        "{:?}",
        by_id(&plan, "T1")
    );
    assert!(
        marked(by_id(&plan, "T4"), "split: one commit"),
        "{:?}",
        by_id(&plan, "T4")
    );
    assert!(
        !marked(by_id(&plan, "T2"), "split: one commit"),
        "a backticked 'and' is a name"
    );
    assert!(
        !marked(by_id(&plan, "T3"), "split: one commit"),
        "a noun list is one subject"
    );
}

#[test]
fn leaf_gate_touches_across_top_level_roots_are_marked() {
    let plan_section = [
        task(
            "T1",
            "Add the parser",
            &[
                ("touches", "`src/parser.rs`, `docs/parser.md`"),
                ("accept", COUNTED),
            ],
        ),
        task(
            "T2",
            "Add the reader",
            &[
                (
                    "touches",
                    "`src/reader.rs`, `tests/reader.rs`, `Cargo.toml`",
                ),
                ("accept", COUNTED),
            ],
        ),
        filler(),
    ]
    .concat();
    let (plan, _) = gate_plan(&plan_section);
    assert!(
        marked(by_id(&plan, "T1"), "crosses roots"),
        "{:?}",
        by_id(&plan, "T1")
    );
    assert!(
        !marked(by_id(&plan, "T2"), "crosses roots"),
        "a file and its own test, plus a repo-root manifest, are one cluster: {:?}",
        by_id(&plan, "T2")
    );
}

#[test]
fn leaf_gate_a_compound_accept_is_marked() {
    let plan_section = [
        task(
            "T1",
            "Add the parser",
            &[
                ("touches", "`src/a.rs`"),
                (
                    "accept",
                    "`cargo test a && cargo test b` → exit 0, 4 passed",
                ),
            ],
        ),
        task(
            "T2",
            "Add the reader",
            &[
                ("touches", "`src/b.rs`"),
                (
                    "accept",
                    "`cargo test a` → exit 0, 2 passed; then `cargo clippy` → exit 0",
                ),
            ],
        ),
        task(
            "T3",
            "Add the writer",
            &[
                ("touches", "`src/c.rs`"),
                ("accept", "`grep -cE 'load|save' src/c.rs` → 2"),
            ],
        ),
        task(
            "T4",
            "Add the store",
            &[
                ("touches", "`src/d.rs`"),
                ("accept", "`cargo test x` → exit 0, ≥1 passed, prints `ok`"),
            ],
        ),
        filler(),
    ]
    .concat();
    let (plan, _) = gate_plan(&plan_section);
    assert!(
        marked(by_id(&plan, "T1"), "compound accept"),
        "{:?}",
        by_id(&plan, "T1")
    );
    assert!(
        marked(by_id(&plan, "T2"), "compound accept"),
        "{:?}",
        by_id(&plan, "T2")
    );
    assert!(
        !marked(by_id(&plan, "T3"), "compound accept"),
        "a quoted pipe is a pattern"
    );
    assert!(
        !marked(by_id(&plan, "T4"), "compound accept"),
        "a backticked value is not a command"
    );
}

#[test]
fn leaf_gate_a_test_runner_accept_without_a_count_is_marked() {
    let plan_section = [
        task(
            "T1",
            "Add the parser",
            &[
                ("touches", "`src/a.rs`"),
                ("accept", "`cargo test parser_` → exit 0"),
            ],
        ),
        task(
            "T2",
            "Add the reader",
            &[
                ("touches", "`web/b.ts`"),
                ("accept", "`pnpm test reader` → passes"),
            ],
        ),
        task(
            "T3",
            "Add the writer",
            &[
                ("touches", "`src/c.rs`"),
                ("accept", "`cargo test writer_` → exit 0, ≥4 passed"),
            ],
        ),
        task(
            "T4",
            "Add the store",
            &[
                ("touches", "`src/d.rs`"),
                ("accept", "`grep -c store src/d.rs` → 1"),
            ],
        ),
        filler(),
    ]
    .concat();
    let (plan, _) = gate_plan(&plan_section);
    let no_count = "no count: a filter matching 0 tests exits 0";
    assert!(
        marked(by_id(&plan, "T1"), no_count),
        "{:?}",
        by_id(&plan, "T1")
    );
    assert!(
        marked(by_id(&plan, "T2"), no_count),
        "{:?}",
        by_id(&plan, "T2")
    );
    assert!(
        !marked(by_id(&plan, "T3"), no_count),
        "{:?}",
        by_id(&plan, "T3")
    );
    assert!(
        !marked(by_id(&plan, "T4"), no_count),
        "a grep is not a test runner"
    );
}

#[test]
fn leaf_gate_empty_touches_is_tallied_as_unscoped_without_a_marker() {
    let plan_section = [
        task("T1", "Add the parser", &[("accept", COUNTED)]),
        filler(),
    ]
    .concat();
    let (plan, report) = gate_plan(&plan_section);
    assert_eq!(tally(&report, "unscoped"), 1, "{:?}", report.tally);
    assert!(!marked(by_id(&plan, "T1"), "unscoped"));
    assert!(!by_id(&plan, "T1").needs_owner);
}

#[test]
fn leaf_gate_one_split_trigger_asks_to_justify_unless_exempt() {
    let four = "`src/a.rs`, `src/b.rs`, `src/c.rs`, `src/d.rs`";
    let plan_section = [
        task(
            "T1",
            "Add the parser",
            &[("touches", four), ("accept", COUNTED)],
        ),
        task(
            "T2",
            "Add the reader",
            &[
                ("touches", four),
                ("accept", COUNTED),
                ("exempt", "F4, one mechanical rename"),
            ],
        ),
        task(
            "T3",
            "Add the writer",
            &[
                (
                    "touches",
                    "`src/e.rs`, `src/f.rs`, `src/g.rs`, `tests/e.rs`, `tests/f.rs`",
                ),
                ("accept", COUNTED),
            ],
        ),
        task(
            "T4",
            "Add the store",
            &[
                ("touches", "`src/h.rs`"),
                ("reads", "`a.rs`, `b.rs`, `c.rs`, `d.rs`, `e.rs`, `f.rs`"),
                ("accept", COUNTED),
            ],
        ),
        filler(),
    ]
    .concat();
    let (plan, _) = gate_plan(&plan_section);
    assert!(
        marked(by_id(&plan, "T1"), "justify:"),
        "{:?}",
        by_id(&plan, "T1")
    );
    assert!(
        !marked(by_id(&plan, "T1"), "split:"),
        "{:?}",
        by_id(&plan, "T1")
    );
    assert!(
        !marked(by_id(&plan, "T2"), "justify:"),
        "{:?}",
        by_id(&plan, "T2")
    );
    assert!(
        !marked(by_id(&plan, "T3"), "justify:"),
        "test files do not count: {:?}",
        by_id(&plan, "T3")
    );
    assert!(
        marked(by_id(&plan, "T4"), "justify:"),
        "reads + touches > 6: {:?}",
        by_id(&plan, "T4")
    );
}

#[test]
fn leaf_gate_two_split_triggers_split_even_when_exempt() {
    let plan_section = [
        task(
            "T1",
            "Add the parser",
            &[
                ("touches", "`src/a.rs`, `src/b.rs`, `src/c.rs`, `src/d.rs`"),
                ("reads", "`x.rs`, `y.rs`, `z.rs`"),
                ("exempt", "F2"),
                ("accept", COUNTED),
            ],
        ),
        filler(),
    ]
    .concat();
    let (plan, _) = gate_plan(&plan_section);
    let t1 = by_id(&plan, "T1");
    assert!(marked(t1, "split:"), "{t1:?}");
    assert!(!marked(t1, "justify:"), "{t1:?}");
}

#[test]
fn leaf_gate_a_behaviour_verb_without_red_needs_red_first_proof() {
    let plan_section = [
        task(
            "T1",
            "Reject empty slugs",
            &[("touches", "`src/a.rs`"), ("accept", COUNTED)],
        ),
        task(
            "T2",
            "Fix the slug parser",
            &[
                ("touches", "`src/b.rs`"),
                ("accept", COUNTED),
                ("red", "`cargo test slug_` → 1 failed"),
            ],
        ),
        task(
            "T3",
            "Add the fixture",
            &[("touches", "`src/c.rs`"), ("accept", COUNTED)],
        ),
        filler(),
    ]
    .concat();
    let (plan, _) = gate_plan(&plan_section);
    assert!(
        marked(by_id(&plan, "T1"), "no red-first proof"),
        "{:?}",
        by_id(&plan, "T1")
    );
    assert!(!marked(by_id(&plan, "T2"), "no red-first proof"));
    assert!(!marked(by_id(&plan, "T3"), "no red-first proof"));
}

#[test]
fn leaf_gate_a_sweep_must_end_with_a_grep() {
    let plan_section = [
        task(
            "T1",
            "Rename the slug type across all modules",
            &[("touches", "`src/a.rs`"), ("accept", COUNTED)],
        ),
        task(
            "T2",
            "Rename the slug type everywhere",
            &[
                ("touches", "`src/b.rs`"),
                ("accept", "`grep -rc OldSlug src` → 0"),
            ],
        ),
        filler(),
    ]
    .concat();
    let (plan, _) = gate_plan(&plan_section);
    let sweep = "sweep: end with a grep printing 0";
    assert!(
        marked(by_id(&plan, "T1"), sweep),
        "{:?}",
        by_id(&plan, "T1")
    );
    assert!(
        !marked(by_id(&plan, "T2"), sweep),
        "{:?}",
        by_id(&plan, "T2")
    );
}

#[test]
fn leaf_gate_a_one_task_one_file_plan_gets_no_split_notes() {
    let plan_section = task(
        "T1",
        "Add the reader and the writer",
        &[
            ("touches", "`src/a.rs`"),
            ("reads", "`b.rs`, `c.rs`, `d.rs`, `e.rs`, `f.rs`, `g.rs`"),
            ("accept", COUNTED),
        ],
    );
    let (plan, _) = gate_plan(&plan_section);
    let t1 = by_id(&plan, "T1");
    assert!(
        !t1.markers
            .iter()
            .any(|m| m == "split: one commit" || m.starts_with("justify:")),
        "{t1:?}"
    );

    let (two, _) = gate_plan(&[plan_section, filler()].concat());
    assert!(
        marked(by_id(&two, "T1"), "split: one commit"),
        "the floor needs one task"
    );
    assert!(
        marked(by_id(&two, "T1"), "justify:"),
        "the floor needs one task"
    );
}

#[test]
fn leaf_gate_marks_and_tallies_but_never_demotes() {
    let plan_section = [
        task(
            "T1",
            "Add the parser",
            &[("touches", "`src/a.rs`"), ("accept", COUNTED)],
        ),
        task(
            "T2",
            "Add the reader and the writer",
            &[("touches", "`src/b.rs`"), ("accept", COUNTED)],
        ),
        task(
            "T3",
            "Change the slug rule",
            &[
                ("touches", "`src/c.rs`"),
                ("accept", "`cargo test slug` → exit 0"),
            ],
        ),
        task("T4", "Add the store", &[("accept", COUNTED)]),
    ]
    .concat();
    let (plan, report) = gate_plan(&plan_section);
    assert!(
        plan.tasks.iter().all(|t| !t.needs_owner),
        "{:?}",
        plan.tasks
    );
    assert_eq!(tally(&report, "leaf_ok"), 1, "{:?}", report.tally);
    assert_eq!(tally(&report, "leaf_split"), 1, "{:?}", report.tally);
    assert_eq!(tally(&report, "leaf_notes"), 2, "{:?}", report.tally);
    assert_eq!(tally(&report, "needs_owner"), 0, "{:?}", report.tally);
}
