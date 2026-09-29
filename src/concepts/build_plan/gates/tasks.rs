//! G7 scope fence, G8 dependency repair, G9 executable task, G10 kill wiring, G11 shape and caps.

use std::collections::BTreeSet;

use super::{GateInputs, GateReport};
use crate::concepts::build_plan::plan::{render, BuildPlan, Item};

const MAX_TASKS: usize = 15;
const MAX_SETTLED: usize = 12;
const MAX_RENDERED_BYTES: usize = 12 * 1024;

const READ_ONLY_VERBS: &[&str] = &[
    "grep",
    "rg",
    "sed -n",
    "ls",
    "cat",
    "head",
    "tail",
    "test",
    "find",
    "wc",
    "git log",
    "git show",
    "git diff",
    "cargo test",
    "diff",
    "jq",
];

const DESTRUCTIVE_STARTS: &[&str] = &["rm", "sudo", "dd"];

const DESTRUCTIVE_PAIRS: &[(&str, &str)] = &[
    ("git", "push"),
    ("git", "reset"),
    ("down", "-v"),
    ("drop", "table"),
];

const STOP_WORDS: &[&str] = &[
    "stop", "stops", "stopped", "stopping", "kill", "kills", "killed", "killing", "abort",
    "aborts", "aborted", "aborting", "halt", "halts", "halted", "halting",
];

const OWNER_WORK: &[&str] = &[
    "hand-label",
    "owner fills",
    "manually",
    "on the owner's host",
    "you fill",
];

const GATE_LANGUAGE: &[&str] = &[
    "must not be built",
    "only if",
    "precondition",
    "kill criteri",
    "before any",
];

const CONTINUE_ANYWAY: &[&str] = &["continue anyway", "proceed anyway", "keep going"];

const GATE_MARKER: &str = "gate language without a kill row";

pub fn apply(plan: &mut BuildPlan, inputs: &GateInputs, report: &mut GateReport) {
    scope_fence(plan, report);
    dependency_repair(plan, report);
    executable_tasks(plan, report);
    kill_wiring(plan, inputs, report);
    shape_and_caps(plan, report);
}

fn mark(item: &mut Item, marker: impl Into<String>) {
    let marker = marker.into();
    if !item.markers.contains(&marker) {
        item.markers.push(marker);
    }
}

fn need_owner(item: &mut Item, report: &mut GateReport) {
    if !item.needs_owner {
        item.needs_owner = true;
        report.count("needs_owner");
    }
}

fn next_open_id(plan: &BuildPlan) -> String {
    let max = plan
        .open
        .iter()
        .filter_map(|i| i.id.strip_prefix('Q')?.parse::<usize>().ok())
        .max()
        .unwrap_or(0);
    format!("Q{}", max + 1)
}

fn ascii_quotes(text: &str) -> String {
    text.replace(['\u{2019}', '\u{2018}'], "'")
}

fn norm_path(p: &str) -> String {
    let mut p = p.trim();
    if let Some(head) = p.strip_suffix("(new)") {
        p = head.trim_end();
    }
    if let Some((head, tail)) = p.rsplit_once(':') {
        let anchor = tail.split('-').count() <= 2
            && tail
                .split('-')
                .all(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
        if anchor {
            p = head;
        }
    }
    let p = p
        .strip_suffix("/**")
        .or_else(|| p.strip_suffix("/*"))
        .or_else(|| p.strip_suffix('*'))
        .unwrap_or(p);
    p.trim_start_matches("./").trim_end_matches('/').to_string()
}

fn paths_overlap(a: &str, b: &str) -> bool {
    let (a, b) = (norm_path(a), norm_path(b));
    if a.is_empty() || b.is_empty() {
        return false;
    }
    let under = |long: &str, short: &str| {
        long.strip_prefix(short)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    };
    under(&a, &b) || under(&b, &a)
}

fn backticked(text: &str) -> Vec<String> {
    text.split('`')
        .skip(1)
        .step_by(2)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && !s.contains(char::is_whitespace))
        .collect()
}

fn fenced_paths(plan: &BuildPlan) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in &plan.fence {
        let mut spans = backticked(&item.text);
        for v in item.fields.values() {
            spans.extend(backticked(v));
        }
        if spans.is_empty() && !item.text.contains(char::is_whitespace) {
            spans.push(item.text.trim().to_string());
        }
        out.extend(spans);
    }
    for item in &plan.settled {
        let t = ascii_quotes(&item.text.to_lowercase());
        let fencing = [
            "do not touch",
            "do not modify",
            "do not edit",
            "don't touch",
            "don't modify",
            "don't edit",
            "out of scope",
        ]
        .iter()
        .any(|p| t.contains(p))
            || (t.contains("leave") && t.contains("unchanged"));
        if fencing {
            out.extend(
                backticked(&item.text)
                    .into_iter()
                    .filter(|s| s.contains('/') || s.contains('.')),
            );
        }
    }
    let mut seen = BTreeSet::new();
    out.retain(|p| seen.insert(p.clone()));
    out
}

fn scope_fence(plan: &mut BuildPlan, report: &mut GateReport) {
    let fenced = fenced_paths(plan);
    if fenced.is_empty() {
        return;
    }
    let mut questions = Vec::new();
    for task in &mut plan.tasks {
        for touched in task.list("touches") {
            for path in &fenced {
                if !paths_overlap(&touched, path) {
                    continue;
                }
                let marker = format!("touches fenced {path}");
                if task.markers.contains(&marker) {
                    continue;
                }
                mark(task, marker);
                need_owner(task, report);
                questions.push(format!("{} touches fenced `{path}` — which wins?", task.id));
            }
        }
    }
    for text in questions {
        let mut q = Item::new("", &text);
        q.id = next_open_id(plan);
        plan.open.push(q);
    }
}

fn task_deps(task: &Item) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for id in task
        .field("depends")
        .map(|v| task_refs(&v.to_uppercase()))
        .unwrap_or_default()
    {
        if !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

fn depends_of(plan: &BuildPlan, id: &str) -> Vec<String> {
    plan.tasks
        .iter()
        .find(|t| t.id == id)
        .map(task_deps)
        .unwrap_or_default()
}

fn reaches(plan: &BuildPlan, from: &str, to: &str) -> bool {
    let mut seen = BTreeSet::new();
    let mut stack = depends_of(plan, from);
    while let Some(id) = stack.pop() {
        if id == to {
            return true;
        }
        if seen.insert(id.clone()) {
            stack.extend(depends_of(plan, &id));
        }
    }
    false
}

fn set_depends(task: &mut Item, deps: &[String]) {
    if deps.is_empty() {
        task.fields.remove("depends");
    } else {
        task.fields.insert("depends".to_string(), deps.join(", "));
    }
}

fn add_dependency(task: &mut Item, dep: &str) {
    let mut deps = task_deps(task);
    deps.push(dep.to_string());
    set_depends(task, &deps);
}

fn dependency_repair(plan: &mut BuildPlan, report: &mut GateReport) {
    let known: BTreeSet<String> = plan.tasks.iter().map(|t| t.id.clone()).collect();
    for task in &mut plan.tasks {
        let deps = task_deps(task);
        if deps.iter().all(|d| known.contains(d)) {
            continue;
        }
        for dropped in deps.iter().filter(|d| !known.contains(*d)) {
            mark(task, format!("unknown dependency {dropped}"));
            report.count("repaired");
        }
        let kept: Vec<String> = deps.into_iter().filter(|d| known.contains(d)).collect();
        set_depends(task, &kept);
    }

    let cyclic: Vec<bool> = plan
        .tasks
        .iter()
        .map(|t| reaches(plan, &t.id, &t.id))
        .collect();
    for (task, cyclic) in plan.tasks.iter_mut().zip(&cyclic) {
        if *cyclic {
            mark(task, "dependency cycle");
            need_owner(task, report);
        }
    }

    for later in 1..plan.tasks.len() {
        for earlier in (0..later).rev() {
            let (a, b) = (plan.tasks[earlier].clone(), plan.tasks[later].clone());
            let b_touches = b.list("touches");
            let shared = a
                .list("touches")
                .into_iter()
                .find(|x| b_touches.iter().any(|y| paths_overlap(x, y)));
            let Some(path) = shared else { continue };
            if reaches(plan, &b.id, &a.id) || reaches(plan, &a.id, &b.id) {
                continue;
            }
            let task = &mut plan.tasks[later];
            add_dependency(task, &a.id);
            mark(task, format!("added: shares {path} with {}", a.id));
            report.count("repaired");
        }
    }
}

fn segments(text: &str) -> Vec<(bool, Vec<String>)> {
    let lowered: String = text
        .to_lowercase()
        .chars()
        .map(|c| match c {
            ';' | '&' | '\n' => " ; ".to_string(),
            '|' => " | ".to_string(),
            '`' | '"' | '\'' | '(' | ')' => " ".to_string(),
            c => c.to_string(),
        })
        .collect();
    let mut out: Vec<(bool, Vec<String>)> = vec![(false, Vec::new())];
    for tok in lowered.split_whitespace() {
        match tok {
            ";" | "-c" => out.push((false, Vec::new())),
            "|" => out.push((true, Vec::new())),
            t => out.last_mut().expect("seeded").1.push(t.to_string()),
        }
    }
    out.retain(|(_, toks)| !toks.is_empty());
    out
}

// Segment-start matching cannot see wrappers such as `xargs rm` or `env rm`.
fn is_destructive(text: &str) -> bool {
    segments(text).iter().any(|(piped, toks)| {
        let first = toks[0].as_str();
        DESTRUCTIVE_STARTS.contains(&first)
            || (*piped && matches!(first, "sh" | "bash"))
            || toks
                .windows(2)
                .any(|w| DESTRUCTIVE_PAIRS.contains(&(w[0].as_str(), w[1].as_str())))
    })
}

fn read_only(check: &str) -> bool {
    let c = check
        .trim()
        .trim_matches('`')
        .trim()
        .to_lowercase()
        .replace("2>&1", "");
    if c.contains('>') || c.contains("-delete") || c.contains("-exec") {
        return false;
    }
    c.replace("&&", "\n")
        .replace("||", "\n")
        .replace([';', '|'], "\n")
        .lines()
        .map(|seg| seg.trim().trim_matches('`').trim())
        .filter(|seg| !seg.is_empty())
        .all(|seg| {
            READ_ONLY_VERBS.iter().any(|v| {
                seg.strip_prefix(v)
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
            })
        })
}

fn runnable_accept(accept: &str) -> bool {
    let Some(rest) = accept.trim().strip_prefix('`') else {
        return false;
    };
    let Some((cmd, rest)) = rest.split_once('`') else {
        return false;
    };
    let condition = rest
        .find('→')
        .map(|at| &rest[at + '→'.len_utf8()..])
        .or_else(|| rest.find("->").map(|at| &rest[at + 2..]));
    !cmd.trim().is_empty() && condition.is_some_and(|c| !c.trim().is_empty())
}

fn executable_tasks(plan: &mut BuildPlan, report: &mut GateReport) {
    for task in &mut plan.tasks {
        if !task.field("accept").is_some_and(runnable_accept) {
            mark(task, "no runnable accept");
            need_owner(task, report);
        }
        let commands = format!(
            "{} ; {}",
            task.field("accept").unwrap_or_default(),
            task.field("check").unwrap_or_default()
        );
        if is_destructive(&commands) {
            mark(task, "destructive command");
            need_owner(task, report);
        }
        let everything = ascii_quotes(
            &format!(
                "{} {}",
                task.text,
                task.fields.values().cloned().collect::<Vec<_>>().join(" ")
            )
            .to_lowercase(),
        );
        if OWNER_WORK.iter().any(|p| everything.contains(p)) {
            mark(task, "needs you");
            need_owner(task, report);
        }
        if task.text.contains(" and ") {
            mark(task, "split: one task per commit");
        }
    }
    for item in &mut plan.verify {
        let Some(check) = item.field("check").map(str::to_string) else {
            continue;
        };
        if is_destructive(&check) {
            mark(item, "destructive command");
            need_owner(item, report);
        } else if !read_only(&check) {
            mark(item, "check is not read-only");
        }
    }
}

fn task_refs(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (i, c) in text.char_indices() {
        if c != 'T' || text[..i].ends_with(|p: char| p.is_ascii_alphanumeric()) {
            continue;
        }
        let digits: String = text[i + 1..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        if !digits.is_empty() {
            out.push(format!("T{digits}"));
        }
    }
    out
}

fn kill_wiring(plan: &mut BuildPlan, inputs: &GateInputs, report: &mut GateReport) {
    for k in 0..plan.kills.len() {
        let kill = plan.kills[k].clone();
        let text = format!(
            "{} {}",
            kill.text,
            kill.fields.values().cloned().collect::<Vec<_>>().join(" ")
        )
        .to_lowercase();
        let checkers = kill.field("checked by").map(task_refs).unwrap_or_default();
        let gated = kill.field("gates").map(task_refs).unwrap_or_default();
        let mut missing = Vec::new();
        if checkers.is_empty() {
            missing.push("checked by");
        }
        if gated.is_empty() {
            missing.push("gates");
        }
        if !kill
            .text
            .to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .any(|w| STOP_WORDS.contains(&w))
        {
            missing.push("stop action");
        }
        if !missing.is_empty() {
            mark(
                &mut plan.kills[k],
                format!("incomplete kill wiring: {}", missing.join(", ")),
            );
        }
        if CONTINUE_ANYWAY.iter().any(|p| text.contains(p)) {
            mark(&mut plan.kills[k], "kill criterion says continue anyway");
            report.note(format!("{}: kill criterion says continue anyway", kill.id));
        }
        for id in checkers.iter().chain(&gated) {
            if !plan.tasks.iter().any(|t| &t.id == id) {
                mark(
                    &mut plan.kills[k],
                    format!("incomplete kill wiring: unknown {id}"),
                );
            }
        }
        for gate in &gated {
            for checker in &checkers {
                let both = plan.tasks.iter().any(|t| &t.id == gate)
                    && plan.tasks.iter().any(|t| &t.id == checker);
                if !both || gate == checker || reaches(plan, gate, checker) {
                    continue;
                }
                if reaches(plan, checker, gate) {
                    mark(
                        &mut plan.kills[k],
                        format!("incomplete kill wiring: {checker} depends on {gate}"),
                    );
                    continue;
                }
                if let Some(task) = plan.tasks.iter_mut().find(|t| &t.id == gate) {
                    add_dependency(task, checker);
                    mark(task, format!("added: {} gates {gate}", kill.id));
                    report.count("repaired");
                }
            }
        }
    }

    let already_asked = plan
        .open
        .iter()
        .any(|q| q.markers.iter().any(|m| m == GATE_MARKER));
    if !plan.kills.is_empty() || already_asked {
        return;
    }
    for turn in inputs.evidence.turns() {
        let lower = turn.raw.to_ascii_lowercase();
        let Some((at, phrase)) = GATE_LANGUAGE
            .iter()
            .filter_map(|p| lower.find(p).map(|at| (at, *p)))
            .min_by_key(|(at, _)| *at)
        else {
            continue;
        };
        let before: Vec<char> = turn.raw[..at].chars().rev().take(30).collect();
        let window = before
            .into_iter()
            .rev()
            .chain(turn.raw[at..].chars().take(phrase.len() + 60))
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        plan.open_from(
            Item::new(
                "",
                &format!("gate language without a kill row: \"{window}\""),
            ),
            GATE_MARKER,
        );
        break;
    }
}

fn shape_and_caps(plan: &BuildPlan, report: &mut GateReport) {
    for name in &plan.missing {
        report.note(format!("missing section: {name} — placeholder"));
    }
    if plan.tasks.len() > MAX_TASKS
        || plan.settled.len() > MAX_SETTLED
        || render(plan).len() > MAX_RENDERED_BYTES
    {
        report.note("too large: split into phases");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::sources::SourceProbe;
    use crate::concepts::build_plan::gates::Evidence;
    use crate::concepts::build_plan::plan::Item;

    fn item(id: &str, text: &str, fields: &[(&str, &str)]) -> Item {
        let mut it = Item::new(id, text);
        for (k, v) in fields {
            it.fields.insert((*k).to_string(), (*v).to_string());
        }
        it
    }

    fn run_with(plan: &mut BuildPlan, conversation: &str) -> GateReport {
        let evidence = Evidence::new("", conversation, &[]);
        let probe = SourceProbe::default();
        let inputs = GateInputs {
            evidence: &evidence,
            open_artifact: None,
            audit: None,
            probe: &probe,
        };
        let mut report = GateReport::default();
        apply(plan, &inputs, &mut report);
        report
    }

    fn run(plan: &mut BuildPlan) -> GateReport {
        run_with(plan, "")
    }

    fn has_marker(it: &Item, marker: &str) -> bool {
        it.markers.iter().any(|m| m == marker)
    }

    const RUNNABLE: &str = "`cargo test x` → exit 0";

    #[test]
    fn g7_a_task_touching_a_fenced_path_needs_the_owner() {
        let mut plan = BuildPlan::default();
        plan.fence
            .push(item("F1", "`src/domain/links.rs` is frozen", &[]));
        plan.settled
            .push(item("S1", "Do not touch `templates/idea.html`.", &[]));
        plan.tasks.push(item(
            "T1",
            "Edit links",
            &[("touches", "`src/domain/links.rs`"), ("accept", RUNNABLE)],
        ));
        plan.tasks.push(item(
            "T2",
            "Edit templates",
            &[("touches", "templates/"), ("accept", RUNNABLE)],
        ));
        plan.tasks.push(item(
            "T3",
            "Edit sibling",
            &[
                ("touches", "src/domain/links_extra.rs"),
                ("accept", RUNNABLE),
            ],
        ));
        let report = run(&mut plan);
        assert!(plan.tasks[0].needs_owner);
        assert!(has_marker(
            &plan.tasks[0],
            "touches fenced src/domain/links.rs"
        ));
        assert!(plan.tasks[1].needs_owner);
        assert!(!plan.tasks[2].needs_owner, "segment-aware prefix match");
        assert_eq!(
            plan.open[0].text,
            "T1 touches fenced `src/domain/links.rs` — which wins?"
        );
        assert_eq!(plan.open.len(), 2);
        assert_eq!(report.tally.get("needs_owner"), Some(&2));
    }

    #[test]
    fn g8_shared_touches_get_a_dependency() {
        let mut plan = BuildPlan::default();
        plan.tasks.push(item(
            "T1",
            "First",
            &[("touches", "src/x.rs"), ("accept", RUNNABLE)],
        ));
        plan.tasks.push(item(
            "T2",
            "Second",
            &[("touches", "src/"), ("depends", "T9"), ("accept", RUNNABLE)],
        ));
        let report = run(&mut plan);
        assert_eq!(plan.tasks[1].list("depends"), ["T1"]);
        assert!(has_marker(&plan.tasks[1], "added: shares src/x.rs with T1"));
        assert!(has_marker(&plan.tasks[1], "unknown dependency T9"));
        assert_eq!(report.tally.get("repaired"), Some(&2));
    }

    #[test]
    fn g8_a_transitive_path_needs_no_new_edge() {
        let mut plan = BuildPlan::default();
        plan.tasks.push(item(
            "T1",
            "A",
            &[("touches", "src/x.rs"), ("accept", RUNNABLE)],
        ));
        plan.tasks
            .push(item("T2", "B", &[("depends", "T1"), ("accept", RUNNABLE)]));
        plan.tasks.push(item(
            "T3",
            "C",
            &[
                ("touches", "src/x.rs"),
                ("depends", "T2"),
                ("accept", RUNNABLE),
            ],
        ));
        run(&mut plan);
        assert_eq!(plan.tasks[2].list("depends"), ["T2"]);
        assert!(plan.tasks[2].markers.is_empty());
    }

    #[test]
    fn g8_a_cycle_needs_the_owner() {
        let mut plan = BuildPlan::default();
        plan.tasks
            .push(item("T1", "A", &[("depends", "T2"), ("accept", RUNNABLE)]));
        plan.tasks
            .push(item("T2", "B", &[("depends", "T1"), ("accept", RUNNABLE)]));
        plan.tasks.push(item("T3", "C", &[("accept", RUNNABLE)]));
        run(&mut plan);
        assert!(plan.tasks[0].needs_owner && plan.tasks[1].needs_owner);
        assert!(has_marker(&plan.tasks[0], "dependency cycle"));
        assert!(!plan.tasks[2].needs_owner);
    }

    #[test]
    fn g9_prose_accept_needs_the_owner() {
        let mut plan = BuildPlan::default();
        plan.tasks
            .push(item("T1", "Prose", &[("accept", "it works well")]));
        plan.tasks.push(item("T2", "Missing", &[]));
        plan.tasks
            .push(item("T3", "Arrow", &[("accept", "`ls` -> lists files")]));
        plan.tasks
            .push(item("T4", "Empty", &[("accept", "`ls` →")]));
        run(&mut plan);
        assert!(plan.tasks[0].needs_owner);
        assert!(has_marker(&plan.tasks[0], "no runnable accept"));
        assert!(plan.tasks[1].needs_owner);
        assert!(!plan.tasks[2].needs_owner);
        assert!(plan.tasks[3].needs_owner);
    }

    #[test]
    fn g9_a_destructive_check_is_flagged() {
        let mut plan = BuildPlan::default();
        plan.verify
            .push(item("P1", "Wipe", &[("check", "`rm -rf target`")]));
        plan.verify
            .push(item("P2", "Mutating", &[("check", "`touch x`")]));
        plan.verify.push(item(
            "P3",
            "Fine",
            &[("check", "`grep -n foo src/a.rs | wc -l`")],
        ));
        plan.tasks.push(item(
            "T1",
            "Ship",
            &[("accept", "`git push origin main` → exit 0")],
        ));
        run(&mut plan);
        assert!(has_marker(&plan.verify[0], "destructive command"));
        assert!(has_marker(&plan.verify[1], "check is not read-only"));
        assert!(plan.verify[2].markers.is_empty());
        assert!(plan.tasks[0].needs_owner);
        assert!(has_marker(&plan.tasks[0], "destructive command"));
    }

    #[test]
    fn g9_owner_work_is_marked() {
        let mut plan = BuildPlan::default();
        plan.tasks.push(item(
            "T1",
            "Label rows and write docs",
            &[("accept", "`cargo test` → hand-label 20 rows")],
        ));
        run(&mut plan);
        assert!(plan.tasks[0].needs_owner);
        assert!(has_marker(&plan.tasks[0], "needs you"));
        assert!(has_marker(&plan.tasks[0], "split: one task per commit"));
    }

    fn kill_plan() -> BuildPlan {
        let mut plan = BuildPlan::default();
        plan.tasks
            .push(item("T1", "Measure", &[("accept", RUNNABLE)]));
        plan.tasks
            .push(item("T2", "Build", &[("accept", RUNNABLE)]));
        plan.kills.push(item(
            "K1",
            "Stop the build if the measure fails",
            &[("checked by", "T1"), ("gates", "T2")],
        ));
        plan
    }

    #[test]
    fn g10_a_gated_task_gets_the_checking_edge() {
        let mut plan = kill_plan();
        plan.kills.push(item("K2", "Nothing wired", &[]));
        run(&mut plan);
        assert_eq!(plan.tasks[1].list("depends"), ["T1"]);
        assert!(has_marker(&plan.tasks[1], "added: K1 gates T2"));
        assert!(has_marker(
            &plan.kills[1],
            "incomplete kill wiring: checked by, gates, stop action"
        ));
        assert!(plan.kills[0].markers.is_empty());
    }

    #[test]
    fn g10_continue_anyway_is_flagged() {
        let mut plan = kill_plan();
        plan.kills[0].text = "Stop the build, then continue anyway".to_string();
        let report = run(&mut plan);
        assert!(has_marker(
            &plan.kills[0],
            "kill criterion says continue anyway"
        ));
        assert!(report.notes.iter().any(|n| n.contains("K1")));
    }

    #[test]
    fn g10_gate_language_without_a_kill_row_opens_a_question() {
        let mut plan = BuildPlan::default();
        plan.tasks
            .push(item("T1", "Build", &[("accept", RUNNABLE)]));
        run_with(
            &mut plan,
            "## user\nThe indexer must not be built until the probe is measured.\n",
        );
        assert_eq!(plan.open.len(), 1);
        assert!(plan.open[0].text.contains("must not be built"));
        assert!(has_marker(
            &plan.open[0],
            "gate language without a kill row"
        ));

        let mut wired = kill_plan();
        run_with(&mut wired, "## user\nThe indexer must not be built yet.\n");
        assert!(wired.open.is_empty());
    }

    #[test]
    fn g11_caps_note_a_split_and_trim_nothing() {
        let mut plan = BuildPlan {
            missing: vec!["## Kill criteria"],
            ..BuildPlan::default()
        };
        for n in 1..=16 {
            plan.tasks
                .push(item(&format!("T{n}"), "Step", &[("accept", RUNNABLE)]));
        }
        let report = run(&mut plan);
        assert_eq!(plan.tasks.len(), 16);
        assert!(report
            .notes
            .contains(&"missing section: ## Kill criteria — placeholder".to_string()));
        assert_eq!(
            report
                .notes
                .iter()
                .filter(|n| *n == "too large: split into phases")
                .count(),
            1
        );

        let mut small = BuildPlan::default();
        small
            .tasks
            .push(item("T1", "Step", &[("accept", RUNNABLE)]));
        assert!(run(&mut small).notes.is_empty());
    }

    #[test]
    fn g8_prose_dependencies_are_read_by_task_id() {
        let mut plan = BuildPlan::default();
        plan.tasks.push(item("T1", "A", &[("accept", RUNNABLE)]));
        plan.tasks.push(item("T2", "B", &[("accept", RUNNABLE)]));
        plan.tasks.push(item(
            "T3",
            "C",
            &[("depends", "T1 and T2"), ("accept", RUNNABLE)],
        ));
        plan.tasks.push(item(
            "T4",
            "D",
            &[("depends", "t1 (scaffold), T9"), ("accept", RUNNABLE)],
        ));
        run(&mut plan);
        assert!(plan.tasks[2].markers.is_empty(), "{:?}", plan.tasks[2]);
        assert_eq!(plan.tasks[2].field("depends"), Some("T1 and T2"));
        assert_eq!(plan.tasks[3].list("depends"), ["T1"]);
        assert!(has_marker(&plan.tasks[3], "unknown dependency T9"));
        assert!(!has_marker(&plan.tasks[3], "unknown dependency T1"));
    }

    #[test]
    fn g8_nearest_task_suppresses_the_redundant_edge() {
        let mut plan = BuildPlan::default();
        plan.tasks.push(item(
            "T1",
            "A",
            &[("touches", "src/x.rs"), ("accept", RUNNABLE)],
        ));
        plan.tasks.push(item(
            "T2",
            "B",
            &[
                ("touches", "src/x.rs"),
                ("depends", "T1"),
                ("accept", RUNNABLE),
            ],
        ));
        plan.tasks.push(item(
            "T3",
            "C",
            &[("touches", "src/x.rs"), ("accept", RUNNABLE)],
        ));
        run(&mut plan);
        assert_eq!(plan.tasks[2].list("depends"), ["T2"]);
    }

    #[test]
    fn g7_anchored_glob_and_new_paths_still_overlap() {
        let mut plan = BuildPlan::default();
        plan.fence
            .push(item("F1", "`src/index/reindex.rs:382-390` is frozen", &[]));
        plan.fence.push(item("F2", "`src/memory/*`", &[]));
        plan.settled
            .push(item("S1", "Don\u{2019}t modify `src/ai/budget.rs`.", &[]));
        plan.tasks.push(item(
            "T1",
            "A",
            &[("touches", "src/index/reindex.rs"), ("accept", RUNNABLE)],
        ));
        plan.tasks.push(item(
            "T2",
            "B",
            &[("touches", "src/memory/load.rs"), ("accept", RUNNABLE)],
        ));
        plan.tasks.push(item(
            "T3",
            "C",
            &[("touches", "src/ai/budget.rs (new)"), ("accept", RUNNABLE)],
        ));
        plan.tasks.push(item(
            "T4",
            "D",
            &[("touches", "src/index/reindex.rs:12"), ("accept", RUNNABLE)],
        ));
        run(&mut plan);
        assert!(plan.tasks[0].needs_owner, "anchored fence");
        assert!(plan.tasks[1].needs_owner, "glob fence");
        assert!(plan.tasks[2].needs_owner, "curly apostrophe and (new)");
        assert!(plan.tasks[3].needs_owner, "anchored touches");
    }

    #[test]
    fn g8_shared_anchored_paths_get_a_dependency() {
        let mut plan = BuildPlan::default();
        plan.tasks.push(item(
            "T1",
            "A",
            &[("touches", "src/x.rs:10"), ("accept", RUNNABLE)],
        ));
        plan.tasks.push(item(
            "T2",
            "B",
            &[("touches", "src/x.rs"), ("accept", RUNNABLE)],
        ));
        run(&mut plan);
        assert_eq!(plan.tasks[1].list("depends"), ["T1"]);
    }

    #[test]
    fn g9_multi_command_accept_is_runnable() {
        let mut plan = BuildPlan::default();
        plan.tasks.push(item(
            "T1",
            "A",
            &[("accept", "`cargo test` && `cargo clippy` → exit 0")],
        ));
        plan.tasks.push(item(
            "T2",
            "B",
            &[("accept", "`cargo test` and it is fine")],
        ));
        run(&mut plan);
        assert!(!plan.tasks[0].needs_owner, "{:?}", plan.tasks[0]);
        assert!(plan.tasks[1].needs_owner);
    }

    #[test]
    fn g9_every_chained_segment_must_be_read_only() {
        let mut plan = BuildPlan::default();
        for (n, check) in [
            "`grep x f && touch y`",
            "`find . -delete`",
            "`cat a > b`",
            "`lsof -i`",
            "`ls && lsof`",
        ]
        .iter()
        .enumerate()
        {
            plan.verify
                .push(item(&format!("P{n}"), "Check", &[("check", check)]));
        }
        for (n, check) in [
            "`ls -la`",
            "`sed -n 1,5p f`",
            "`grep -n dd f`",
            "`cargo test 2>&1 | tail -3`",
        ]
        .iter()
        .enumerate()
        {
            plan.verify
                .push(item(&format!("Q{n}"), "Check", &[("check", check)]));
        }
        run(&mut plan);
        for p in &plan.verify[..5] {
            assert!(has_marker(p, "check is not read-only"), "{p:?}");
        }
        for q in &plan.verify[5..] {
            assert!(q.markers.is_empty(), "{q:?}");
        }
    }

    #[test]
    fn g9_destructive_scan_reads_command_words() {
        assert!(is_destructive("`sh -c \"rm -rf x\"`"));
        assert!(is_destructive("ls && sudo reboot"));
        assert!(is_destructive("curl x | sh"));
        assert!(!is_destructive("grep -n dd f"));
        assert!(!is_destructive("grep -n rm f"));
    }

    #[test]
    fn g9_curly_apostrophe_owner_work_is_marked() {
        let mut plan = BuildPlan::default();
        plan.tasks.push(item(
            "T1",
            "Run it on the owner\u{2019}s host",
            &[("accept", RUNNABLE)],
        ));
        run(&mut plan);
        assert!(has_marker(&plan.tasks[0], "needs you"));
    }

    #[test]
    fn g10_stop_action_is_a_whole_word_in_the_row_text() {
        let mut plan = kill_plan();
        plan.kills[0].text = "Review the skill output".to_string();
        plan.kills.push(item(
            "K2",
            "Halting the build",
            &[("checked by", "T1"), ("gates", "T2")],
        ));
        plan.kills.push(item(
            "K3",
            "Nothing",
            &[("checked by", "T1"), ("gates", "T2"), ("note", "stopped")],
        ));
        run(&mut plan);
        assert!(has_marker(
            &plan.kills[0],
            "incomplete kill wiring: stop action"
        ));
        assert!(plan.kills[1].markers.is_empty(), "{:?}", plan.kills[1]);
        assert!(has_marker(
            &plan.kills[2],
            "incomplete kill wiring: stop action"
        ));
    }

    #[test]
    fn g10_a_kill_row_naming_an_absent_task_is_marked() {
        let mut plan = kill_plan();
        plan.kills[0]
            .fields
            .insert("gates".to_string(), "T7".to_string());
        run(&mut plan);
        assert!(has_marker(
            &plan.kills[0],
            "incomplete kill wiring: unknown T7"
        ));
    }

    #[test]
    fn g10_a_checker_that_depends_on_the_gated_task_is_not_looped() {
        let mut plan = kill_plan();
        plan.tasks[0]
            .fields
            .insert("depends".to_string(), "T2".to_string());
        run(&mut plan);
        assert!(has_marker(
            &plan.kills[0],
            "incomplete kill wiring: T1 depends on T2"
        ));
        assert!(plan.tasks[1].field("depends").is_none());
    }

    #[test]
    fn g10_only_one_gate_language_question_is_asked() {
        let mut plan = BuildPlan::default();
        plan.tasks
            .push(item("T1", "Build", &[("accept", RUNNABLE)]));
        let talk = "## user\nIt must not be built yet.\n## assistant\nA precondition applies.\n";
        run_with(&mut plan, talk);
        run_with(&mut plan, talk);
        assert_eq!(plan.open.len(), 1);
    }

    #[test]
    fn g11_settled_count_and_rendered_size_are_capped() {
        let too_large = "too large: split into phases".to_string();
        let mut twelve = BuildPlan::default();
        for n in 1..=12 {
            twelve.settled.push(item(&format!("S{n}"), "Fact", &[]));
        }
        assert!(!run(&mut twelve).notes.contains(&too_large));
        twelve.settled.push(item("S13", "Fact", &[]));
        assert!(run(&mut twelve).notes.contains(&too_large));

        let mut fat = BuildPlan::default();
        fat.tasks
            .push(item("T1", &"a".repeat(13 * 1024), &[("accept", RUNNABLE)]));
        assert!(run(&mut fat).notes.contains(&too_large));
    }
}
